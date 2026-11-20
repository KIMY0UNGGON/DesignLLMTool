//! Layout and visual export. Same-format export is the native save in `lib.rs`.
//!
//! Layout always computes lines fresh. Line-segment caches from the file are never replayed,
//! because they can describe an older text. Content this engine does not render is listed as a
//! blocking loss; the export is refused unless the caller allows that loss code.
use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;

use designllm_contracts::page_losses::{
    self,
    HEADER_FOOTER_NOT_RENDERED, MASTER_PAGE_NOT_RENDERED, NOTE_NOT_RENDERED, PAGE_BACKGROUND_NOT_RENDERED,
    PAGE_BORDER_NOT_RENDERED, PAGE_NUMBER_NOT_RENDERED, WHOLE_DOCUMENT_LOSSES,
};
use designllm_contracts::{
    Diagnostic, DltError, DltResult, EncodedOutput, EngineContext, ExportRequest, FlowDocument,
    RenderScene,
};
use designllm_flow::{grapheme_span, LogicalParagraph, TextLinkSpan, OBJECT_REPLACEMENT};
use designllm_package::xml::direct_text;
use designllm_package::Package;
use roxmltree::Node as XNode;
use serde_json::json;

use crate::diag;
use crate::header::{child_el, is_el};
use crate::ops::NativeLines;
use crate::section::{node_id, run_items, AtomKind, PBlock, POpaque, PSection, RunItem};
use crate::xmlx::{Xml, NS_HC, NS_HH, NS_HP};

/// Local names of object kinds that are visible in the page but are not laid out by this engine.
/// Opaque records carry local names (`pic`, `rect`, ...).
const VISIBLE_UNMODELED_LABELS: &[&str] = &[
    "pic",
    "rect",
    "ellipse",
    "line",
    "arc",
    "polygon",
    "curve",
    "container",
    "ole",
    "textart",
    "video",
    "equation",
    "compose",
    "chart",
];

/// Lays out the document with the shared flow engine and marks the result as fresh layout. `links` are the body's
/// hyperlinks (see [`text_link_spans`]). `sections` are those of the original package (none for a document without
/// one): the page-break carriers that the save folds into an object's host (`crate::writer::folded_carriers`) take no
/// line. The preview and the visual export both lay out through here, so they give the same pages.
pub fn layout(body: &FlowDocument, sections: &[PSection], links: &[TextLinkSpan], lines: &NativeLines, ctx: &EngineContext<'_>) -> DltResult<RenderScene> {
    let carriers = crate::writer::folded_carriers(sections, body, lines);
    let mut scene = designllm_flow::layout_flow_with_carriers(body, links, &carriers, ctx)?;
    scene.metadata.insert("layout_origin".into(), json!("fresh"));
    scene.metadata.insert("engine".into(), json!("hwpx"));
    scene.metadata.insert("line_cache".into(), json!("invalidated_on_reflow_not_replayed"));
    scene.diagnostics.push(diag::info(
        diag::LAYOUT_APPROXIMATE,
        "HWPX layout uses this engine's line breaking; Hancom's line layout can differ",
        None,
    ));
    Ok(scene)
}

/// Visits every block of `blocks`, and the blocks of their table cells, in document order.
fn walk_blocks<'a>(blocks: &'a [PBlock], visit: &mut impl FnMut(&'a PBlock)) {
    for block in blocks {
        visit(block);
        if let PBlock::Table(t) = block {
            for cell in t.rows.iter().flatten() {
                walk_blocks(&cell.blocks, visit);
            }
        }
    }
}

/// The key of the `hp:p` that holds the element at `key` (the last paragraph step of the anchor path), if any.
fn anchor_paragraph(key: &str) -> Option<&str> {
    let mut end = None;
    let mut offset = 0;
    for step in key.split('/') {
        offset += step.len();
        if step == "hp:p" || step.starts_with("hp:p[") || step.starts_with("hp:p#") {
            end = Some(offset);
        }
        offset += 1;
    }
    end.map(|end| &key[..end])
}

/// Content that the visual output would omit, one diagnostic for each object so that a page selection keeps those of
/// its pages. An object placed as a character (an inline atom of its paragraph) is shown as a placeholder in the text, so
/// it names that paragraph. Any other visible unmodeled object floats: Hancom places it from its anchor, and the page it
/// lands on is not known here, so it names itself. No page draws that id, and every page selection keeps the loss. An
/// opaque block names itself and follows the page that draws its placeholder.
pub fn visible_losses(sections: &[PSection]) -> Vec<Diagnostic> {
    let modeled_pictures: BTreeSet<&str> = sections
        .iter()
        .flat_map(|s| s.blocks.iter())
        .filter_map(|b| match b {
            PBlock::Image(i) => Some(i.key.as_str()),
            _ => None,
        })
        .collect();
    let mut inline_objects: BTreeSet<&str> = BTreeSet::new();
    for section in sections {
        walk_blocks(&section.blocks, &mut |block| {
            if let PBlock::Para(p) = block {
                inline_objects.extend(p.atoms.iter().filter(|a| a.kind == AtomKind::Object).map(|a| a.key.as_str()));
            }
        });
    }
    let mut out = Vec::new();
    for section in sections {
        let visible = section.opaque.iter().filter(|r| VISIBLE_UNMODELED_LABELS.contains(&r.label.as_str()) && !modeled_pictures.contains(r.key.as_str()));
        for record in visible {
            let anchor = anchor_paragraph(&record.key).filter(|_| inline_objects.contains(record.key.as_str()));
            out.push(diag::blocking(
                diag::FLOATING_NOT_RENDERED,
                format!("A visible {} object ({}) is preserved in the native file but not drawn in visual output", record.label, record.key),
                Some(node_id(&section.part, anchor.unwrap_or(&record.key))),
            ));
        }
    }
    for section in sections {
        let mut opaque_blocks: Vec<&POpaque> = Vec::new();
        walk_blocks(&section.blocks, &mut |block| {
            if let PBlock::Opaque(o) = block {
                opaque_blocks.push(o);
            }
        });
        for block in opaque_blocks {
            out.push(diag::blocking(
                diag::EXPORT_OMITS,
                format!("A {} block that this engine cannot draw is omitted from visual output", block.label),
                Some(block.id.clone()),
            ));
        }
    }
    out
}

/// Page content that the native file keeps and visual output does not draw: headers and footers, page numbers, footnotes
/// and endnotes (each a child of an `hp:ctrl`), and a page border, a page background or a master page of a section (a
/// child of `hp:secPr`). A page border fill counts only when its `hh:borderFill` draws a line or paints a fill; Hancom
/// writes one with no line and no fill into most documents. `package` is the original package, the one `sections` were
/// read from. Each section part gives one blocking loss for each code, naming the first such element. No page draws that
/// element and the codes are [`WHOLE_DOCUMENT_LOSSES`], so every page selection keeps the loss.
pub fn unrendered_page_content(package: &Package) -> DltResult<Vec<Diagnostic>> {
    let header_source = utf8_part(package, crate::package::HEADER)?;
    let header = Xml::parse(header_source).map_err(|e| diag::malformed(format!("{}: {e}", crate::package::HEADER)))?;
    let fills: BTreeMap<&str, (bool, bool)> = header
        .doc
        .descendants()
        .filter(|n| is_el(*n, NS_HH, "borderFill"))
        .filter_map(|n| Some((n.attribute("id")?, border_fill_paints(n))))
        .collect();
    let mut out = Vec::new();
    for part in crate::package::section_parts(package) {
        let source = utf8_part(package, &part)?;
        let xml = Xml::parse(source).map_err(|e| diag::malformed(format!("{part}: {e}")))?;
        // Code -> (key of the first element, number of elements).
        let mut found: BTreeMap<&'static str, (&str, usize)> = BTreeMap::new();
        for node in xml.doc.descendants().filter(|n| n.is_element()) {
            let Some(key) = xml.key(node) else { continue };
            for code in page_element_losses(node, &fills) {
                found.entry(code).or_insert((key, 0)).1 += 1;
            }
        }
        for (code, (key, count)) in found {
            out.push(diag::blocking(
                code,
                format!("{count} {} of {part} preserved in the native file and not drawn in visual output", page_element_name(code)),
                Some(node_id(&part, key)),
            ));
        }
    }
    Ok(out)
}

fn utf8_part<'p>(package: &'p Package, part: &str) -> DltResult<&'p str> {
    std::str::from_utf8(package.part(part)?).map_err(|e| diag::malformed(format!("{part} is not UTF-8: {e}")))
}

/// Whether an `hh:borderFill` draws a line on a side of the page, and whether it paints the page. A side draws a line
/// when its type is not `NONE` and its width (`"0.1 mm"`) is not zero. The `alpha` of a brush is not read: Hancom writes
/// `alpha="0"` on painted fills, and rhwp draws border fills without it.
fn border_fill_paints(fill: XNode<'_, '_>) -> (bool, bool) {
    let drawn = |side: XNode<'_, '_>| {
        let width = side.attribute("width").and_then(|w| w.split_whitespace().next()).and_then(|w| w.parse::<f64>().ok());
        side.attribute("type").is_some_and(|t| t != "NONE") && width.is_none_or(|w| w > 0.0)
    };
    let line = ["leftBorder", "rightBorder", "topBorder", "bottomBorder"].iter().any(|side| child_el(fill, NS_HH, side).is_some_and(drawn));
    let paint = child_el(fill, NS_HC, "fillBrush").is_some_and(|brush| {
        brush.children().filter(|c| c.is_element()).any(|c| {
            if c.tag_name().name() != "winBrush" {
                // hc:gradation, hc:imgBrush.
                return true;
            }
            let face = c.attribute("faceColor").unwrap_or("none");
            let hatch = c.attribute("hatchStyle").is_some_and(|h| h != "NONE");
            hatch || !(face.eq_ignore_ascii_case("none") || face.eq_ignore_ascii_case("#FFFFFF"))
        })
    });
    (line, paint)
}

/// The losses of one element of a section part (see [`unrendered_page_content`]).
fn page_element_losses(node: XNode<'_, '_>, fills: &BTreeMap<&str, (bool, bool)>) -> Vec<&'static str> {
    if node.tag_name().namespace() != Some(NS_HP) {
        return Vec::new();
    }
    let parent = node.parent_element();
    let in_ctrl = parent.is_some_and(|p| is_el(p, NS_HP, "ctrl"));
    let in_section_props = parent.is_some_and(|p| is_el(p, NS_HP, "secPr"));
    match node.tag_name().name() {
        "header" | "footer" if in_ctrl && draws_content(node) => vec![HEADER_FOOTER_NOT_RENDERED],
        "pageNum" if in_ctrl && node.attribute("pos") != Some("NONE") => vec![PAGE_NUMBER_NOT_RENDERED],
        "footNote" | "endNote" if in_ctrl => vec![NOTE_NOT_RENDERED],
        "masterPage" if in_section_props => vec![MASTER_PAGE_NOT_RENDERED],
        "pageBorderFill" if in_section_props => {
            let (line, paint) = node.attribute("borderFillIDRef").and_then(|id| fills.get(id)).copied().unwrap_or_default();
            let mut codes = Vec::new();
            if line {
                codes.push(PAGE_BORDER_NOT_RENDERED);
            }
            if paint {
                codes.push(PAGE_BACKGROUND_NOT_RENDERED);
            }
            codes
        }
        _ => Vec::new(),
    }
}

/// Whether a header or footer shows anything: text, a visible object, a table, or an automatic number. An empty one
/// (Hancom writes one for an odd or even page that has no header) shows nothing.
fn draws_content(node: XNode<'_, '_>) -> bool {
    node.descendants().any(|n| {
        if is_el(n, NS_HP, "t") {
            return n.children().any(|c| c.is_text() && c.text().is_some_and(|t| !t.trim().is_empty()));
        }
        let name = n.tag_name().name();
        n.is_element() && n.tag_name().namespace() == Some(NS_HP) && (VISIBLE_UNMODELED_LABELS.contains(&name) || matches!(name, "tbl" | "autoNum" | "pageNum"))
    })
}

fn page_element_name(code: &str) -> &'static str {
    match code {
        HEADER_FOOTER_NOT_RENDERED => "header or footer element(s)",
        PAGE_NUMBER_NOT_RENDERED => "page number element(s)",
        NOTE_NOT_RENDERED => "footnote or endnote element(s)",
        PAGE_BORDER_NOT_RENDERED => "page border fill(s) with a border line",
        PAGE_BACKGROUND_NOT_RENDERED => "page border fill(s) with a background",
        _ => "master page reference(s)",
    }
}

/// Visual export (PDF, SVG, PNG) through the injected renderer. `package` is the original package (see
/// [`unrendered_page_content`]).
pub fn export_visual(
    body: &FlowDocument,
    sections: &[PSection],
    package: &Package,
    links: &[TextLinkSpan],
    lines: &NativeLines,
    request: &ExportRequest,
    ctx: &EngineContext<'_>,
) -> DltResult<EncodedOutput> {
    let format = request.format.to_ascii_lowercase();
    if !matches!(format.as_str(), "pdf" | "svg" | "png") {
        return Err(DltError::unsupported(format!("HWPX cannot export to {}", request.format)));
    }
    let mut scene = layout(body, sections, links, lines, ctx)?;
    // The omitted content goes through the page selection with the layout's own diagnostics, and the renderer reports
    // all of them once with its own.
    let mut diagnostics = visible_losses(sections);
    diagnostics.append(&mut unrendered_page_content(package)?);
    diagnostics.append(&mut scene.diagnostics);
    scene.diagnostics = diagnostics;
    designllm_flow::page_select::select_export_pages(&mut scene, body, &format, request.page_indices.as_deref(), WHOLE_DOCUMENT_LOSSES)?;
    // A loss the request allows is reported without blocking, as the DOCX export does.
    page_losses::release_allowed(&mut scene.diagnostics, &request.allowed_losses, is_export_loss);
    ctx.renderer.encode(&scene, &format, &request.profile, ctx.assets)
}

/// The losses of this file: content the visual export does not draw.
fn is_export_loss(code: &str) -> bool {
    code == diag::FLOATING_NOT_RENDERED || code == diag::EXPORT_OMITS || WHOLE_DOCUMENT_LOSSES.contains(&code)
}

/// The hyperlinks of the body text, read from the original package. A HYPERLINK field runs from its `hp:fieldBegin` to
/// the `hp:fieldEnd` of the same paragraph, and its span is the text and inline objects between the two markers. The
/// offsets are the original text's. A paragraph whose text differs from the original text gives no span: an edit moves the
/// text under the offsets even when it keeps the byte length.
pub fn text_link_spans(body: &FlowDocument, package: &Package) -> DltResult<Vec<TextLinkSpan>> {
    let mut sections: BTreeMap<String, Xml<'_>> = BTreeMap::new();
    for part in crate::package::section_parts(package) {
        let source = utf8_part(package, &part)?;
        if source.contains("HYPERLINK") {
            let xml = Xml::parse(source).map_err(|e| diag::malformed(format!("{part}: {e}")))?;
            sections.insert(part, xml);
        }
    }
    let mut paragraphs = Vec::new();
    designllm_flow::tree::for_each_paragraph(body, |p| paragraphs.push(p));
    let mut spans = Vec::new();
    for p in paragraphs {
        // Flow paragraph ids are `hwpx:{part}|{anchor}`; the anchor names the `hp:p` element.
        let Some((part, key)) = p.id.strip_prefix("hwpx:").and_then(|rest| rest.split_once('|')) else { continue };
        let Some(node) = sections.get(part).and_then(|xml| xml.node(key)) else { continue };
        if !is_el(node, NS_HP, "p") {
            continue;
        }
        let (fields, text) = hyperlink_fields(node);
        let lp = LogicalParagraph::new(p);
        if fields.is_empty() || lp.text != text {
            continue;
        }
        for (url, bytes) in fields {
            if let Some((start, end)) = grapheme_span(&lp, bytes) {
                spans.push(TextLinkSpan { paragraph_id: p.id.clone(), start, end, url });
            }
        }
    }
    Ok(spans)
}

/// The HYPERLINK fields of a paragraph element, with their URLs and byte ranges in the logical text, and that text. The text
/// is the one `section.rs` projects (`run_items`): the `hp:t` text, and one U+FFFC for each tab, line break and inline object.
/// Field markers and other hidden content add nothing. A `hp:fieldEnd` closes the open `hp:fieldBegin` whose `id` its
/// `beginIDRef` names, so crossed fields keep their own ranges; an end without that reference, or with one that no open
/// begin carries, closes the last open field.
fn hyperlink_fields(paragraph: XNode<'_, '_>) -> (Vec<(String, Range<usize>)>, String) {
    let mut text = String::new();
    let mut open: Vec<(Option<&str>, Option<String>, usize)> = Vec::new();
    let mut fields = Vec::new();
    for run in paragraph.children().filter(|c| is_el(*c, NS_HP, "run")) {
        for item in run_items(run) {
            match item {
                RunItem::Text(piece) => text.push_str(piece),
                RunItem::Atom { .. } => text.push(OBJECT_REPLACEMENT),
                RunItem::Hidden { node, in_t: false } if node.is_element() && node.tag_name().name() == "ctrl" => {
                    for marker in node.children().filter(|c| c.is_element()) {
                        match marker.tag_name().name() {
                            "fieldBegin" => {
                                let url = if marker.attribute("type") == Some("HYPERLINK") { field_url(marker) } else { None };
                                open.push((marker.attribute("id"), url, text.len()));
                            }
                            "fieldEnd" => {
                                let named = marker.attribute("beginIDRef").and_then(|id| open.iter().rposition(|(begin, ..)| *begin == Some(id)));
                                let Some(index) = named.or_else(|| open.len().checked_sub(1)) else { continue };
                                if let (_, Some(url), start) = open.remove(index) {
                                    fields.push((url, start..text.len()));
                                }
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
        }
    }
    (fields, text)
}

/// The URL of a HYPERLINK field, read from its `Command` parameter: the text up to the first `;`, with a backslash taking
/// the next character, and with the surrounding whitespace removed. A blank result gives `None`. This is the HWPX field
/// rule. It differs from the HWP rule (`rhwp` `command_uri`, used by `hwp/src/lib.rs`) in that `command_uri` returns the URL
/// as a `String`, empty for an empty command, where this returns `None`. The command text is all of its text children (see
/// `direct_text`: a comment or an element inside it adds nothing). The escape and trim steps match `command_uri` in rhwp 0.9.0 (`model/hyperlink.rs`).
fn field_url(field: XNode<'_, '_>) -> Option<String> {
    let command = field.descendants().find(|n| n.is_element() && n.tag_name().name() == "stringParam" && n.attribute("name") == Some("Command"))?;
    let command = direct_text(command);
    let mut url = String::new();
    let mut chars = command.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                if let Some(next) = chars.next() {
                    url.push(next);
                }
            }
            ';' => break,
            _ => url.push(c),
        }
    }
    let url = url.trim();
    (!url.is_empty()).then(|| url.to_string())
}

#[cfg(test)]
mod projection_tests {
    //! The text that [`hyperlink_fields`] reads from an `hp:p` is the logical text of the flow paragraph that
    //! `section.rs` projects from it (`Parser::parse_para`), for the HWPX fixtures, the blank template, and paragraphs
    //! with every kind of run item.
    use super::*;
    use designllm_contracts::{sha256, AssetRef, AssetStore, ErrorCode};

    struct Assets;

    impl AssetStore for Assets {
        fn put(&self, bytes: &[u8], mime: &str) -> DltResult<AssetRef> {
            Ok(AssetRef { sha256: sha256(bytes), mime: mime.to_string(), bytes: bytes.len() as u64 })
        }
        fn read(&self, _asset: &AssetRef) -> DltResult<Vec<u8>> {
            Err(DltError::new(ErrorCode::NotFound, "the projection test stores no asset"))
        }
        fn lookup(&self, _hash: &str) -> DltResult<AssetRef> {
            Err(DltError::new(ErrorCode::NotFound, "the projection test stores no asset"))
        }
    }

    const SECTION: &str = "Contents/section0.xml";
    /// The last child of an `hp:run` of the common fixture; the cases put their run items in its place.
    const TEXT_RUN: &str = "<hp:t>어울림</hp:t>";
    const OBJ: char = OBJECT_REPLACEMENT;
    /// The digest of the dumps of all inputs, the same before and after the projection was shared (48-formats20). A change of
    /// the parsed model (`PSection` debug form) or of the export projection changes it; a deliberate one updates it.
    const PROJECTION_DIGEST: &str = "a83d7d6fd27db6c259afa13fe859fea530dd2dfeeb4dbd9a6e77037f18dc8647";

    fn fixture(relative: &str) -> Vec<u8> {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../fixtures").join(relative);
        std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
    }

    /// The common fixture with [`TEXT_RUN`] replaced by `items`.
    fn with_items(items: &str) -> Vec<u8> {
        let source = fixture("hwpx/opaque-image-preserve.hwpx");
        let mut package = Package::open(&source).expect("package");
        let section = std::str::from_utf8(package.part(SECTION).expect("section")).expect("utf-8").to_string();
        assert!(section.contains(TEXT_RUN));
        package.set_part(SECTION, section.replacen(TEXT_RUN, items, 1).into_bytes()).expect("set part");
        package.encode().expect("encode")
    }

    /// Compares the two projections of every flow paragraph of `bytes` that names an `hp:p`. Returns the texts compared
    /// and a dump of the parsed sections and of the export projection (texts and field ranges).
    fn compare(bytes: &[u8]) -> (Vec<String>, String) {
        let (package, loaded) = crate::doc::load_bytes(bytes, &Assets).expect("load");
        let mut sections: BTreeMap<String, Xml<'_>> = BTreeMap::new();
        for part in crate::package::section_parts(&package) {
            let xml = Xml::parse(utf8_part(&package, &part).expect("utf-8")).expect("xml");
            sections.insert(part, xml);
        }
        let mut texts = Vec::new();
        let mut dump = format!("{:?}\n", loaded.sections);
        designllm_flow::tree::for_each_paragraph(&loaded.flow, |p| {
            let Some((part, key)) = p.id.strip_prefix("hwpx:").and_then(|rest| rest.split_once('|')) else { return };
            let Some(node) = sections.get(part).and_then(|xml| xml.node(key)) else { return };
            if !is_el(node, NS_HP, "p") {
                return;
            }
            let (fields, text) = hyperlink_fields(node);
            assert_eq!(LogicalParagraph::new(p).text, text, "paragraph {}", p.id);
            dump.push_str(&format!("{}\t{text}\t{fields:?}\n", p.id));
            texts.push(text);
        });
        assert!(!texts.is_empty(), "no paragraph compared");
        (texts, dump)
    }

    #[test]
    fn the_export_text_of_every_paragraph_is_its_flow_projection() {
        let tab = r#"<hp:tab width="4000" leader="0" type="1"/>"#;
        let field = |id: u32, command: &str| {
            format!(
                r#"<hp:ctrl><hp:fieldBegin id="{id}" type="HYPERLINK" name="" editable="0" dirty="0" zOrder="0" fieldid="{id}"><hp:parameters cnt="1" name=""><hp:stringParam name="Command">{command}</hp:stringParam></hp:parameters></hp:fieldBegin></hp:ctrl>"#
            )
        };
        let end = |id: u32| format!(r#"<hp:ctrl><hp:fieldEnd beginIDRef="{id}" fieldid="{id}"/></hp:ctrl>"#);
        // (run items, the paragraph's logical text)
        let cases: Vec<(String, String)> = vec![
            (format!("<hp:t>가{tab}나<hp:lineBreak/>다</hp:t>"), format!("가{OBJ}나{OBJ}다")),
            ("<hp:t>앞<!-- note -->뒤<?pi x?></hp:t><!-- run note --><hp:t/><hp:t></hp:t>".into(), "앞뒤".into()),
            (
                format!("<hp:t>가나</hp:t>{}<hp:t>다{tab}라</hp:t>{}{}<hp:t>마</hp:t>{}", field(901, r"https://example.com/a\;b;1;0;0;"), end(901), field(902, "plain"), end(902)),
                format!("가나다{OBJ}라마"),
            ),
            (r##"<hp:t>가<hp:markpenBegin color="#FFFF00"/>나<hp:markpenEnd/></hp:t>"##.into(), "가나".into()),
            (
                r#"<hp:t>가</hp:t><hp:equation id="5"><hp:pos treatAsChar="1"/></hp:equation><hp:rect id="6"><hp:pos treatAsChar="0"/></hp:rect><hp:t>나</hp:t>"#.into(),
                format!("가{OBJ}나"),
            ),
            ("<hp:t>가</hp:t>직접<hp:t>나</hp:t>".into(), "가나".into()),
        ];
        let blank = crate::template::blank_package(crate::template::A4_WIDTH_HU, crate::template::A4_HEIGHT_HU, crate::template::DEFAULT_MARGINS_HU).expect("blank");
        let mut dumps = String::new();
        let mut compared = 0;
        for bytes in [fixture("hwpx/opaque-image-preserve.hwpx"), fixture("_sources/rhwp/test-image.hwpx"), blank] {
            let (texts, dump) = compare(&bytes);
            compared += texts.len();
            dumps.push_str(&dump);
        }
        for (items, expected) in &cases {
            let (texts, dump) = compare(&with_items(items));
            assert!(texts.contains(expected), "{items}: {expected:?} not among {texts:?}");
            compared += texts.len();
            dumps.push_str(&dump);
        }
        assert_eq!(compared, 41, "paragraphs compared");
        assert_eq!(sha256(dumps.as_bytes()), PROJECTION_DIGEST, "the parsed sections and the export projection are those of 48-formats20");
    }

    /// A field end closes the begin its `beginIDRef` names, also when fields cross; an end without the reference closes
    /// the last open field.
    #[test]
    fn a_field_end_closes_the_begin_it_names() {
        let begin = |id: u32, url: &str| {
            format!(r#"<hp:ctrl><hp:fieldBegin id="{id}" type="HYPERLINK"><hp:parameters><hp:stringParam name="Command">{url};1;0;0;</hp:stringParam></hp:parameters></hp:fieldBegin></hp:ctrl>"#)
        };
        let end = |reference: &str| format!(r#"<hp:ctrl><hp:fieldEnd{reference}/></hp:ctrl>"#);
        let fields_of = |runs: String| {
            let source = format!(r#"<hp:p xmlns:hp="{NS_HP}"><hp:run>{runs}</hp:run></hp:p>"#);
            let xml = Xml::parse(&source).expect("xml");
            hyperlink_fields(xml.doc.root_element())
        };
        let (a, b) = ("https://example.com/a", "https://example.com/b");
        // 가 [a 나 [b 다 a] 라 b] 마: a covers 나다, b covers 다라.
        let crossed = format!(
            "<hp:t>가</hp:t>{}<hp:t>나</hp:t>{}<hp:t>다</hp:t>{}<hp:t>라</hp:t>{}<hp:t>마</hp:t>",
            begin(1, a),
            begin(2, b),
            end(r#" beginIDRef="1""#),
            end(r#" beginIDRef="2""#)
        );
        let (fields, text) = fields_of(crossed);
        assert_eq!(text, "가나다라마");
        assert_eq!(fields, vec![(a.to_string(), 3..9), (b.to_string(), 6..12)]);
        // Without references the ends close the last open field: b covers 다, a covers 나다라.
        let unnamed = format!("<hp:t>가</hp:t>{}<hp:t>나</hp:t>{}<hp:t>다</hp:t>{}<hp:t>라</hp:t>{}<hp:t>마</hp:t>", begin(1, a), begin(2, b), end(""), end(""));
        assert_eq!(fields_of(unnamed).0, vec![(b.to_string(), 6..9), (a.to_string(), 3..12)]);
        // A reference that no open begin carries closes the last open field.
        let unknown = format!("{}<hp:t>나</hp:t>{}", begin(1, a), end(r#" beginIDRef="7""#));
        assert_eq!(fields_of(unknown).0, vec![(a.to_string(), 0..3)]);
    }
}

