//! `word/document.xml` to the shared flow model, with native records for every node.
//!
//! Classification is conservative. A run is editable only when it holds plain text, tabs, and line
//! breaks. Anything else becomes a protected inline atom whose XML bytes are kept, and a block the
//! engine cannot model becomes an opaque block. Nothing is dropped silently.

use std::collections::{BTreeMap, BTreeSet};

use designllm_contracts::{
    new_id, sha256, AssetRef, Diagnostic, DltError, DltResult, FlowBlock, FlowDocument, Id, NodeKind, PageSetup, Paragraph,
    ParagraphFormat, Rect, Section, Size, Style, Table, TableCell, TextRun,
};
use designllm_contracts::page_losses::{PAGE_BACKGROUND_NOT_RENDERED, PAGE_BORDER_NOT_RENDERED};
use roxmltree::Node;
use serde_json::Value;

use designllm_package::xml::direct_text;

use crate::diag;
use crate::model::{self, Rec, Records};
use crate::props::{ParaProps, RunProps};
use crate::styles::StyleTable;
use crate::tree::{self, elements, is_ns, is_w, qkey, w_attr, w_text, Tree, NS_A, NS_M, NS_MC, NS_PIC, NS_R, NS_WP};
use crate::units;

/// Object-replacement character: the one-grapheme placeholder for an inline object.
pub const OBJECT_CHAR: &str = "\u{FFFC}";

/// A protected child of a paragraph (or a protected opaque block) with its byte hash in the original part.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AtomRec {
    pub qname: String,
    pub occ: u32,
    pub sha: String,
}

/// VML, the legacy drawing markup of `w:pict`.
const NS_V: &str = "urn:schemas-microsoft-com:vml";

/// A section break found in a paragraph's `w:pPr` (or the body-level `w:sectPr`).
pub struct SectBreak {
    pub setup: PageSetup,
    pub path: Vec<usize>,
    pub has_header_footer: bool,
    pub has_page_borders: bool,
}

/// Result of parsing one document part.
pub struct Parsed {
    pub document: FlowDocument,
    pub records: Records,
    /// Owner block or paragraph id to its protected children.
    pub atoms: BTreeMap<Id, Vec<AtomRec>>,
    /// Deep canonical form of every block at its original path (the writer's change check).
    pub canon_by_path: BTreeMap<Vec<usize>, Value>,
    /// Canonical form of every section setup at its `sectPr` path.
    pub section_by_path: BTreeMap<Vec<usize>, Value>,
    pub diagnostics: Vec<Diagnostic>,
    /// Image payloads referenced by block pictures, keyed by SHA-256 hex, with their MIME type.
    pub image_bytes: BTreeMap<String, (String, Vec<u8>)>,
    /// Block pictures of the original part, keyed by their original path.
    pub pictures: BTreeMap<Vec<usize>, OriginalPicture>,
}

/// A block picture of the original package: its payload, the relationship its `a:blip` uses, and the part.
#[derive(Clone, Debug)]
pub struct OriginalPicture {
    pub sha: String,
    pub rel_id: String,
    pub part: String,
    /// Original block-picture description; absent and empty are equivalent to an empty model alt string.
    pub descr: Option<String>,
    /// Set when giving the picture new bytes would silently keep a setting the model does not represent.
    pub replace_problem: Option<String>,
}

/// Parses `document_bytes`.
///
/// `rels` maps relationship ids of the document part to package part names. `read_part` returns the
/// payload of a package part, or `None` when it is missing.
pub fn parse_document(
    document_bytes: &[u8],
    styles: &StyleTable,
    rels: &BTreeMap<String, String>,
    read_part: &dyn Fn(&str) -> Option<Vec<u8>>,
) -> DltResult<Parsed> {
    let tree = Tree::parse(document_bytes)?;
    let root = tree.root();
    if !is_w(root, "document") {
        return Err(tree::malformed("word/document.xml root is not w:document"));
    }
    let (body_index, body) = elements(root)
        .into_iter()
        .enumerate()
        .find(|(_, n)| is_w(*n, "body"))
        .ok_or_else(|| tree::malformed("w:body is missing"))?;
    let mut parser = Parser {
        styles,
        rels,
        read_part,
        records: Records::new(),
        atoms: BTreeMap::new(),
        canon_by_path: BTreeMap::new(),
        section_by_path: BTreeMap::new(),
        diagnostics: Vec::new(),
        image_bytes: BTreeMap::new(),
        pictures: BTreeMap::new(),
    };
    if elements(root).into_iter().any(|n| is_w(n, "background") && paints_background(n)) {
        parser.diagnostics.push(diag::info(
            PAGE_BACKGROUND_NOT_RENDERED,
            "the page background (w:background) is preserved in the package and not drawn in the preview",
            None,
        ));
    }
    let sections = parser.parse_body(&tree, body, vec![body_index])?;
    let style_map: BTreeMap<String, Style> = styles.styles.keys().map(|id| (id.clone(), styles.model_style(id))).collect();
    Ok(Parsed {
        document: FlowDocument { sections, styles: style_map },
        records: parser.records,
        atoms: parser.atoms,
        canon_by_path: parser.canon_by_path,
        section_by_path: parser.section_by_path,
        diagnostics: parser.diagnostics,
        image_bytes: parser.image_bytes,
        pictures: parser.pictures,
    })
}

struct Parser<'a> {
    styles: &'a StyleTable,
    rels: &'a BTreeMap<String, String>,
    read_part: &'a dyn Fn(&str) -> Option<Vec<u8>>,
    records: Records,
    atoms: BTreeMap<Id, Vec<AtomRec>>,
    canon_by_path: BTreeMap<Vec<usize>, Value>,
    section_by_path: BTreeMap<Vec<usize>, Value>,
    diagnostics: Vec<Diagnostic>,
    image_bytes: BTreeMap<String, (String, Vec<u8>)>,
    pictures: BTreeMap<Vec<usize>, OriginalPicture>,
}

/// Page setup from a `w:sectPr`. Missing values take the contract defaults (A4, 20 mm margins).
fn setup_from(sect: Node<'_, '_>) -> PageSetup {
    let mut setup = PageSetup::default();
    for child in elements(sect) {
        if is_w(child, "pgSz") {
            let w = w_attr(child, "w").and_then(units::parse_int);
            let h = w_attr(child, "h").and_then(units::parse_int);
            if let (Some(w), Some(h)) = (w, h) {
                if let (Ok(w), Ok(h)) = (units::twips_to_emu(w), units::twips_to_emu(h)) {
                    setup.size = Size { width: w, height: h };
                }
            }
        } else if is_w(child, "pgMar") {
            let get = |name: &str| w_attr(child, name).and_then(units::parse_int).and_then(|t| units::twips_to_emu(t).ok());
            if let Some(v) = get("left") {
                setup.margin_left = v;
            }
            if let Some(v) = get("top") {
                setup.margin_top = v;
            }
            if let Some(v) = get("right") {
                setup.margin_right = v;
            }
            if let Some(v) = get("bottom") {
                setup.margin_bottom = v;
            }
        }
    }
    setup
}

fn has_header_or_footer(sect: Node<'_, '_>) -> bool {
    elements(sect).into_iter().any(|c| is_w(c, "headerReference") || is_w(c, "footerReference"))
}

/// Whether the `w:pgBorders` of a `w:sectPr` draws a line on a side of the page (`w:val` other than `none` and `nil`).
fn has_page_borders(sect: Node<'_, '_>) -> bool {
    elements(sect).into_iter().filter(|c| is_w(*c, "pgBorders")).flat_map(elements).any(|side| {
        matches!(side.tag_name().name(), "top" | "left" | "bottom" | "right") && !matches!(w_attr(side, "val"), None | Some("none") | Some("nil"))
    })
}

/// Whether a `w:background` paints the page: a drawing inside it, or a color that is not white. `w:color` is the color
/// Word shows (a theme color is written with it); a theme color alone counts unless it is the light background
/// (`background1`, `light1`), which is white in the default theme.
fn paints_background(background: Node<'_, '_>) -> bool {
    if background.children().any(|c| c.is_element()) {
        return true;
    }
    match (w_attr(background, "color"), w_attr(background, "themeColor")) {
        (Some(color), _) => !color.eq_ignore_ascii_case("auto") && !color.eq_ignore_ascii_case("FFFFFF"),
        (None, Some(theme)) => !matches!(theme, "background1" | "light1"),
        (None, None) => false,
    }
}

/// Whether an object is placed on the page rather than in the line: a DrawingML anchor (`wp:anchor`), or a VML shape with
/// `position:absolute` or `position:relative` in its style (VML places such a shape from its anchor, not in the line).
fn is_floating(node: Node<'_, '_>) -> bool {
    node.descendants().any(|n| {
        let positioned = || {
            n.attribute("style").is_some_and(|style| {
                style.split(';').filter_map(|d| d.split_once(':')).any(|(key, value)| {
                    key.trim().eq_ignore_ascii_case("position") && matches!(value.trim().to_ascii_lowercase().as_str(), "absolute" | "relative")
                })
            })
        };
        is_ns(n, NS_WP, "anchor") || (n.is_element() && n.tag_name().namespace() == Some(NS_V) && positioned())
    })
}

fn mime_for(part: &str) -> &'static str {
    let ext = part.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "bmp" => "image/bmp",
        "tif" | "tiff" => "image/tiff",
        "emf" => "image/x-emf",
        "wmf" => "image/x-wmf",
        "svg" => "image/svg+xml",
        _ => "application/octet-stream",
    }
}

/// What giving a picture new bytes would silently keep: a crop, a tiled or non-stretched fill, a rotation or a
/// flip, or an effect on the blip. `None` when the picture is a plain stretch of its image.
fn replace_problem(inline: Node<'_, '_>) -> Option<String> {
    let fill = inline.descendants().find(|n| is_ns(*n, NS_PIC, "blipFill"))?;
    for child in elements(fill) {
        if is_ns(child, NS_A, "blip") {
            if elements(child).into_iter().any(|effect| !is_ns(effect, NS_A, "extLst")) {
                return Some("the picture has an effect on its image".to_string());
            }
        } else if is_ns(child, NS_A, "srcRect") {
            if ["l", "t", "r", "b"].iter().any(|edge| child.attribute(*edge).is_some_and(|v| v.trim() != "0")) {
                return Some("the picture is cropped".to_string());
            }
        } else if !(is_ns(child, NS_A, "stretch") || is_ns(child, NS_A, "extLst")) {
            return Some(format!("the picture fill is not a plain stretch ({})", qkey(child)));
        }
    }
    let xfrm = inline.descendants().find(|n| is_ns(*n, NS_A, "xfrm"))?;
    if xfrm.attribute("rot").is_some_and(|v| v.trim() != "0") {
        return Some("the picture is rotated".to_string());
    }
    if ["flipH", "flipV"].iter().any(|flip| matches!(xfrm.attribute(*flip), Some("1") | Some("true"))) {
        return Some("the picture is flipped".to_string());
    }
    None
}

/// Plain run: `w:rPr` (first, optional), `w:t` without markup, `w:tab`, `w:cr`, and text-wrapping `w:br`.
fn is_plain_run(run: Node<'_, '_>) -> bool {
    for (index, child) in elements(run).into_iter().enumerate() {
        if is_w(child, "rPr") {
            if index != 0 {
                return false;
            }
        } else if is_w(child, "t") {
            if child.children().any(|c| c.is_element()) {
                return false;
            }
        } else if is_w(child, "tab") || is_w(child, "cr") {
            continue;
        } else if is_w(child, "br") {
            match w_attr(child, "type") {
                None | Some("textWrapping") => {}
                Some(_) => return false,
            }
        } else {
            return false;
        }
    }
    true
}

fn plain_run_text(run: Node<'_, '_>) -> String {
    let mut out = String::new();
    for child in elements(run) {
        if is_w(child, "t") {
            out.push_str(&direct_text(child));
        } else if is_w(child, "tab") {
            out.push('\t');
        } else if is_w(child, "br") || is_w(child, "cr") {
            out.push('\n');
        }
    }
    out
}

fn contains_object(node: Node<'_, '_>) -> bool {
    node.descendants().any(|n| {
        is_w(n, "drawing")
            || is_w(n, "pict")
            || is_w(n, "object")
            || is_w(n, "sym")
            || is_w(n, "footnoteReference")
            || is_w(n, "endnoteReference")
            || is_ns(n, NS_M, "oMath")
            || is_ns(n, NS_M, "oMathPara")
    })
}

fn placeholder_if_empty(text: String) -> String {
    if text.is_empty() {
        OBJECT_CHAR.to_string()
    } else {
        text
    }
}

/// Display text of a protected atom, and the informational diagnostic it carries (code, message).
fn atom_display(child: Node<'_, '_>) -> (String, Option<(&'static str, String)>) {
    let key = qkey(child);
    let text = w_text(child);
    match key.as_str() {
        "w:r" => {
            if child.descendants().any(|n| is_w(n, "drawing") || is_w(n, "pict") || is_w(n, "object")) {
                let diagnostic = if is_floating(child) {
                    (diag::FLOATING_OBJECT_NOT_RENDERED, "floating picture kept in the package and not drawn".to_string())
                } else {
                    (diag::INLINE_IMAGE_PLACEHOLDER, "inline picture inside a text paragraph is drawn as a placeholder".to_string())
                };
                return (placeholder_if_empty(text), Some(diagnostic));
            }
            if contains_object(child) {
                let diagnostic = (diag::PROTECTED_INLINE, "object kept as protected inline content".to_string());
                return (placeholder_if_empty(text), Some(diagnostic));
            }
            (text, None)
        }
        "w:ins" | "w:moveTo" => (
            text,
            Some((diag::TRACKED_CHANGE, "inserted text is kept as protected inline content".to_string())),
        ),
        "w:del" | "w:moveFrom" => (
            String::new(),
            Some((diag::TRACKED_CHANGE, "deleted text is preserved and hidden in the accepted view".to_string())),
        ),
        "w:bookmarkStart" | "w:bookmarkEnd" | "w:commentRangeStart" | "w:commentRangeEnd" | "w:proofErr" | "w:permStart"
        | "w:permEnd" => (String::new(), None),
        "mc:AlternateContent" => {
            let choice = elements(child)
                .into_iter()
                .filter(|n| is_ns(*n, NS_MC, "Choice"))
                .map(w_text)
                .find(|t| !t.is_empty());
            let fallback = elements(child)
                .into_iter()
                .filter(|n| is_ns(*n, NS_MC, "Fallback"))
                .map(w_text)
                .find(|t| !t.is_empty());
            let display = placeholder_if_empty(choice.or(fallback).unwrap_or_default());
            if is_floating(child) {
                let message = "floating object in mc:AlternateContent kept in the package and not drawn".to_string();
                return (display, Some((diag::FLOATING_OBJECT_NOT_RENDERED, message)));
            }
            (display, Some((diag::PROTECTED_INLINE, "mc:AlternateContent is kept as protected inline content".to_string())))
        }
        "m:oMath" | "m:oMathPara" => {
            let math: String = child.descendants().filter(|n| is_ns(*n, NS_M, "t")).filter_map(|n| n.text()).collect();
            (
                placeholder_if_empty(math),
                Some((diag::PROTECTED_INLINE, "equation is kept as protected inline content".to_string())),
            )
        }
        _ => (text, Some((diag::PROTECTED_INLINE, format!("{key} is kept as protected inline content")))),
    }
}

/// Vertical-merge state of a cell in the original table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum VMerge {
    None,
    Restart,
    Continue,
}

/// Cell layout computed before any content is parsed.
struct CellShape<'n, 'i> {
    node: Node<'n, 'i>,
    /// Child index of the cell inside its row.
    child_index: usize,
    start: usize,
    span: usize,
    vmerge: VMerge,
}

struct TableShape<'n, 'i> {
    grid: Vec<i64>,
    /// Per row: the row child index and its cells.
    rows: Vec<(usize, Vec<CellShape<'n, 'i>>)>,
    style: Style,
}

impl<'a> Parser<'a> {
    fn parse_body(&mut self, t: &Tree<'_>, body: Node<'_, '_>, body_path: Vec<usize>) -> DltResult<Vec<Section>> {
        let children = elements(body);
        let count = children.len();
        let mut sections = Vec::new();
        let mut blocks: Vec<FlowBlock> = Vec::new();
        let mut closed_by_body_sect = false;
        for (index, child) in children.into_iter().enumerate() {
            let mut path = body_path.clone();
            path.push(index);
            if is_w(child, "sectPr") {
                if index + 1 != count {
                    return Err(DltError::unsupported("a body-level w:sectPr must be the last body element"));
                }
                let setup = setup_from(child);
                let (has_hf, has_borders) = (has_header_or_footer(child), has_page_borders(child));
                let section = self.make_section(setup, std::mem::take(&mut blocks), Some(path), has_hf, has_borders);
                sections.push(section);
                closed_by_body_sect = true;
                continue;
            }
            if is_w(child, "p") {
                let (block, section_break) = self.parse_paragraph(t, child, path)?;
                blocks.push(block);
                if let Some(SectBreak { setup, path, has_header_footer, has_page_borders }) = section_break {
                    let section = self.make_section(setup, std::mem::take(&mut blocks), Some(path), has_header_footer, has_page_borders);
                    sections.push(section);
                }
            } else if is_w(child, "tbl") {
                blocks.push(self.parse_table(t, child, path)?);
            } else {
                let block = self.opaque_block(t, child, path);
                // A body-level mc:AlternateContent with an anchored drawing is a floating object: Word places it from
                // its anchor, not where the opaque block's placeholder is drawn.
                if let (true, FlowBlock::Opaque { id, .. }) = (is_ns(child, NS_MC, "AlternateContent") && is_floating(child), &block) {
                    let message = "floating object in a body-level mc:AlternateContent kept in the package and not drawn";
                    self.diagnostics.push(diag::info(diag::FLOATING_OBJECT_NOT_RENDERED, message, Some(id)));
                }
                blocks.push(block);
            }
        }
        if !closed_by_body_sect {
            let section = self.make_section(PageSetup::default(), blocks, None, false, false);
            sections.push(section);
        }
        Ok(sections)
    }

    fn make_section(&mut self, setup: PageSetup, blocks: Vec<FlowBlock>, sect_path: Option<Vec<usize>>, has_hf: bool, has_borders: bool) -> Section {
        let id = new_id();
        let section = Section { id: id.clone(), setup, blocks };
        self.records.insert(id.clone(), Rec::Section { sect: sect_path.clone() });
        if let Some(path) = sect_path {
            self.section_by_path.insert(path, model::canon_section(&section));
        }
        if has_hf {
            self.diagnostics.push(diag::info(
                diag::HEADER_FOOTER_NOT_RENDERED,
                "headers and footers are preserved in the package and not drawn in the preview",
                Some(&id),
            ));
        }
        if has_borders {
            self.diagnostics.push(diag::info(
                PAGE_BORDER_NOT_RENDERED,
                "page borders (w:pgBorders) are preserved in the package and not drawn in the preview",
                Some(&id),
            ));
        }
        section
    }

    /// Parses a paragraph. Returns the block and the section break when the paragraph carries a `w:sectPr`.
    fn parse_paragraph(&mut self, t: &Tree<'_>, p: Node<'_, '_>, path: Vec<usize>) -> DltResult<(FlowBlock, Option<SectBreak>)> {
        let kids = elements(p);
        let ppr_index = kids.iter().position(|n| is_w(*n, "pPr"));
        let ppr = ppr_index.map(|i| kids[i]);
        let mut section_break = None;
        if let (Some(ppr_node), Some(ppr_pos)) = (ppr, ppr_index) {
            let sect = elements(ppr_node).into_iter().enumerate().find(|(_, n)| is_w(*n, "sectPr"));
            if let Some((sect_index, sect_node)) = sect {
                let mut sect_path = path.clone();
                sect_path.push(ppr_pos);
                sect_path.push(sect_index);
                section_break = Some(SectBreak {
                    setup: setup_from(sect_node),
                    path: sect_path,
                    has_header_footer: has_header_or_footer(sect_node),
                    has_page_borders: has_page_borders(sect_node),
                });
            }
        }
        let pstyle = ppr
            .and_then(|n| elements(n).into_iter().find(|c| is_w(*c, "pStyle")))
            .and_then(|c| w_attr(c, "val"))
            .map(str::to_string);
        if section_break.is_none() {
            if let Some(block) = self.try_image_paragraph(p, &path, ppr) {
                return Ok((block, None));
            }
        }

        let para_id = new_id();
        let direct_pp = ParaProps::parse(ppr);
        let format: ParagraphFormat = self.styles.para_format(pstyle.as_deref(), &direct_pp);
        let mut runs: Vec<TextRun> = Vec::new();
        let mut atoms: Vec<AtomRec> = Vec::new();
        // Zero-width markers are not model runs (flow counts every protected run as one grapheme).
        // They are written back at their original place, before the next run or after the last one.
        let mut pending_lead: Vec<usize> = Vec::new();
        for (index, child) in kids.iter().enumerate() {
            if is_w(*child, "pPr") {
                continue;
            }
            let key = qkey(*child);
            if is_w(*child, "r") && is_plain_run(*child) {
                let run_id = new_id();
                let rpr = elements(*child).into_iter().find(|n| is_w(*n, "rPr"));
                let direct = RunProps::parse(rpr);
                let rstyle = rpr
                    .and_then(|r| elements(r).into_iter().find(|c| is_w(*c, "rStyle")))
                    .and_then(|c| w_attr(c, "val"))
                    .map(str::to_string);
                let style = self.styles.run_style(pstyle.as_deref(), rstyle.as_deref(), &direct);
                let names: BTreeSet<String> = direct.direct();
                let text = plain_run_text(*child);
                runs.push(TextRun { style, ..TextRun::new(run_id.clone(), text) });
                let lead = std::mem::take(&mut pending_lead);
                self.records.insert(run_id, Rec::Run { origin: Some(index), atom: false, direct: names, rstyle, lead });
                continue;
            }
            let occ = atoms.iter().filter(|a| a.qname == key).count() as u32;
            let sha = sha256(t.raw(&t.span(*child)));
            atoms.push(AtomRec { qname: key, occ, sha });
            let (display, diagnostic) = atom_display(*child);
            if display.is_empty() {
                pending_lead.push(index);
                if let Some((code, message)) = diagnostic {
                    self.diagnostics.push(diag::info(code, message, Some(&para_id)));
                }
                continue;
            }
            let run_id = new_id();
            if let Some((code, message)) = diagnostic {
                self.diagnostics.push(diag::info(code, message, Some(&run_id)));
            }
            runs.push(TextRun { protected: true, ..TextRun::new(run_id.clone(), display) });
            let lead = std::mem::take(&mut pending_lead);
            self.records.insert(
                run_id,
                Rec::Run { origin: Some(index), atom: true, direct: BTreeSet::new(), rstyle: None, lead },
            );
        }

        let paragraph = Paragraph { id: para_id.clone(), runs, style_ref: pstyle, format };
        self.records.insert(
            para_id.clone(),
            Rec::Paragraph { path: Some(path.clone()), direct: direct_pp.direct(), tail: pending_lead },
        );
        if !atoms.is_empty() {
            self.atoms.insert(para_id, atoms);
        }
        let block = FlowBlock::Paragraph(paragraph);
        self.canon_by_path.insert(path, model::canon_block(&block, true));
        Ok((block, section_break))
    }

    /// A paragraph whose only content is one inline picture (no text, no other children) becomes a picture block.
    fn try_image_paragraph(&mut self, p: Node<'_, '_>, path: &[usize], ppr: Option<Node<'_, '_>>) -> Option<FlowBlock> {
        let content: Vec<Node<'_, '_>> = elements(p).into_iter().filter(|n| !is_w(*n, "pPr")).collect();
        if content.len() != 1 || !is_w(content[0], "r") {
            return None;
        }
        let run_children = elements(content[0]);
        if run_children.iter().enumerate().any(|(i, n)| is_w(*n, "rPr") && i != 0) {
            return None;
        }
        let drawings: Vec<Node<'_, '_>> = run_children.iter().copied().filter(|n| !is_w(*n, "rPr")).collect();
        if drawings.len() != 1 || !is_w(drawings[0], "drawing") {
            return None;
        }
        let drawing = drawings[0];
        let inline = elements(drawing).into_iter().find(|n| is_ns(*n, NS_WP, "inline"))?;
        let extent = elements(inline).into_iter().find(|n| is_ns(*n, NS_WP, "extent"))?;
        let cx = extent.attribute("cx").and_then(units::parse_int)?;
        let cy = extent.attribute("cy").and_then(units::parse_int)?;
        if cx <= 0 || cy <= 0 {
            return None;
        }
        let graphic = inline.descendants().find(|n| is_ns(*n, NS_A, "graphicData"))?;
        if graphic.attribute("uri") != Some(NS_PIC) {
            return None;
        }
        let blip = inline.descendants().find(|n| is_ns(*n, NS_A, "blip"))?;
        let rel_id = blip.attribute((NS_R, "embed"))?;
        let part = self.rels.get(rel_id)?.clone();
        let bytes = (self.read_part)(&part)?;
        let sha = sha256(&bytes);
        let mime = mime_for(&part).to_string();
        let asset = AssetRef { sha256: sha.clone(), mime: mime.clone(), bytes: bytes.len() as u64 };
        let descr = inline
            .descendants()
            .find(|n| is_ns(*n, NS_WP, "docPr"))
            .and_then(|n| n.attribute("descr"))
            .map(str::to_string);
        let original = OriginalPicture {
            sha: sha.clone(), rel_id: rel_id.to_string(), part: part.clone(), descr: descr.clone(), replace_problem: replace_problem(inline),
        };
        self.pictures.insert(path.to_vec(), original);
        self.image_bytes.entry(sha).or_insert((mime, bytes));
        let align = ppr
            .and_then(|n| elements(n).into_iter().find(|c| is_w(*c, "jc")))
            .and_then(|c| w_attr(c, "val"))
            .unwrap_or("left")
            .to_string();
        let id = new_id();
        let node = designllm_contracts::Node {
            id: id.clone(),
            kind: NodeKind::Image,
            bounds: Rect { x: 0, y: 0, width: cx, height: cy },
            text: descr.unwrap_or_default(),
            style: Style { align, ..Style::default() },
            asset: Some(asset),
            children: Vec::new(),
            layout: None,
            interactions: Vec::new(),
            variants: BTreeMap::new(),
            ext: Default::default(),
        };
        let block = FlowBlock::Image(node);
        self.records.insert(id, Rec::Image { path: Some(path.to_vec()) });
        self.canon_by_path.insert(path.to_vec(), model::canon_block(&block, true));
        Some(block)
    }

    fn opaque_block(&mut self, t: &Tree<'_>, node: Node<'_, '_>, path: Vec<usize>) -> FlowBlock {
        let id = new_id();
        let label = qkey(node);
        let sha = sha256(t.raw(&t.span(node)));
        self.atoms.insert(id.clone(), vec![AtomRec { qname: label.clone(), occ: 1, sha }]);
        self.records.insert(id.clone(), Rec::Opaque { path: path.clone() });
        self.diagnostics.push(diag::info(diag::PROTECTED_BLOCK, format!("{label} is kept as an opaque block"), Some(&id)));
        let block = FlowBlock::Opaque { id, label };
        self.canon_by_path.insert(path, model::canon_block(&block, true));
        block
    }

    fn parse_table(&mut self, t: &Tree<'_>, tbl: Node<'_, '_>, path: Vec<usize>) -> DltResult<FlowBlock> {
        match table_shape(tbl) {
            Ok(shape) => self.build_table(t, path, shape),
            Err(reason) => {
                let id = new_id();
                let sha = sha256(t.raw(&t.span(tbl)));
                self.atoms.insert(id.clone(), vec![AtomRec { qname: "w:tbl".into(), occ: 1, sha }]);
                self.records.insert(id.clone(), Rec::Opaque { path: path.clone() });
                self.diagnostics.push(diag::info(
                    diag::IRREGULAR_TABLE,
                    format!("table kept as an opaque block: {reason}"),
                    Some(&id),
                ));
                let block = FlowBlock::Opaque { id, label: "w:tbl".into() };
                self.canon_by_path.insert(path, model::canon_block(&block, true));
                Ok(block)
            }
        }
    }

    fn build_table(&mut self, t: &Tree<'_>, path: Vec<usize>, shape: TableShape<'_, '_>) -> DltResult<FlowBlock> {
        let mut rows: Vec<Vec<TableCell>> = Vec::new();
        let row_spans = row_span_table(&shape);
        for (row_index, (tr_child_index, cells)) in shape.rows.iter().enumerate() {
            let mut tr_path = path.clone();
            tr_path.push(*tr_child_index);
            let mut row = Vec::new();
            for (cell_index, cell) in cells.iter().enumerate() {
                if cell.vmerge == VMerge::Continue {
                    // Covered by the cell above: no model cell (flow uses HTML-style spans). The XML is kept
                    // by the writer through the original row.
                    continue;
                }
                let mut cell_path = tr_path.clone();
                cell_path.push(cell.child_index);
                let mut blocks = Vec::new();
                for (child_index, child) in elements(cell.node).into_iter().enumerate() {
                    if is_w(child, "tcPr") {
                        continue;
                    }
                    let mut child_path = cell_path.clone();
                    child_path.push(child_index);
                    if is_w(child, "p") {
                        // A section break inside a table cell has no model; its w:sectPr stays in the XML.
                        let (block, _) = self.parse_paragraph(t, child, child_path)?;
                        blocks.push(block);
                    } else if is_w(child, "tbl") {
                        blocks.push(self.parse_table(t, child, child_path)?);
                    } else {
                        blocks.push(self.opaque_block(t, child, child_path));
                    }
                }
                row.push(TableCell {
                    id: new_id(),
                    blocks,
                    column_span: cell.span as u32,
                    row_span: row_spans[row_index][cell_index],
                });
            }
            rows.push(row);
        }
        let table = Table { id: new_id(), column_widths: shape.grid.clone(), rows, style: shape.style.clone() };
        let hidden = shape.rows.iter().any(|(_, cells)| cells.iter().any(|c| c.vmerge == VMerge::Continue));
        self.records.insert(table.id.clone(), Rec::Table { path: Some(path.clone()), hidden });
        let block = FlowBlock::Table(table);
        self.canon_by_path.insert(path, model::canon_block(&block, true));
        Ok(block)
    }
}

/// Validates the grid, cell spans, and vertical merges without parsing content. Returns the grid widths
/// (EMU) and the cells of every row, or the reason the table cannot be modelled.
fn table_shape<'n, 'i>(tbl: Node<'n, 'i>) -> Result<TableShape<'n, 'i>, String> {
    let grid_node = elements(tbl).into_iter().find(|n| is_w(*n, "tblGrid")).ok_or("no w:tblGrid")?;
    let mut grid = Vec::new();
    for col in elements(grid_node) {
        if !is_w(col, "gridCol") {
            return Err("unexpected child of w:tblGrid".into());
        }
        let twips = w_attr(col, "w").and_then(units::parse_int).ok_or("gridCol without a width")?;
        grid.push(units::twips_to_emu(twips).map_err(|_| "grid width overflows".to_string())?);
    }
    if grid.is_empty() {
        return Err("empty grid".into());
    }
    let mut rows = Vec::new();
    for (row_child_index, tr) in elements(tbl).into_iter().enumerate() {
        if !is_w(tr, "tr") {
            if is_w(tr, "tblPr") || is_w(tr, "tblGrid") {
                continue;
            }
            return Err("unexpected child of w:tbl".into());
        }
        let mut cells = Vec::new();
        let mut column = 0usize;
        for (child_index, tc) in elements(tr).into_iter().enumerate() {
            if is_w(tc, "trPr") || is_w(tc, "tblPrEx") {
                continue;
            }
            if !is_w(tc, "tc") {
                return Err("unexpected child of w:tr".into());
            }
            let tcpr = elements(tc).into_iter().find(|n| is_w(*n, "tcPr"));
            let span = tcpr
                .and_then(|p| elements(p).into_iter().find(|n| is_w(*n, "gridSpan")))
                .and_then(|n| w_attr(n, "val").and_then(units::parse_int))
                .unwrap_or(1);
            if span < 1 {
                return Err("gridSpan below one".into());
            }
            let vmerge = match tcpr.and_then(|p| elements(p).into_iter().find(|n| is_w(*n, "vMerge"))) {
                None => VMerge::None,
                Some(node) => match w_attr(node, "val") {
                    Some("restart") => VMerge::Restart,
                    _ => VMerge::Continue,
                },
            };
            if !elements(tc).into_iter().any(|n| !is_w(n, "tcPr")) {
                return Err("cell without block content".into());
            }
            cells.push(CellShape { node: tc, child_index, start: column, span: span as usize, vmerge });
            column += span as usize;
        }
        if column != grid.len() {
            return Err(format!("row covers {column} grid columns, the grid has {}", grid.len()));
        }
        rows.push((row_child_index, cells));
    }
    if rows.is_empty() {
        return Err("no rows".into());
    }
    let mut active = vec![false; grid.len()];
    for (_, cells) in &rows {
        for cell in cells {
            match cell.vmerge {
                VMerge::Continue => {
                    if !active[cell.start] {
                        return Err("vertical merge continues a cell that does not exist".into());
                    }
                }
                VMerge::Restart | VMerge::None => {
                    let value = cell.vmerge == VMerge::Restart;
                    for flag in active.iter_mut().skip(cell.start).take(cell.span) {
                        *flag = value;
                    }
                }
            }
        }
    }
    Ok(TableShape { grid, rows, style: table_style(tbl) })
}

/// Row span per cell: an origin spans its own row plus the continuation rows below it; a continuation
/// is stored as 0 (covered by the cell above). Other cells span one row.
fn row_span_table(shape: &TableShape<'_, '_>) -> Vec<Vec<u32>> {
    let mut out = Vec::with_capacity(shape.rows.len());
    for (row_index, (_, cells)) in shape.rows.iter().enumerate() {
        let mut row = Vec::with_capacity(cells.len());
        for cell in cells {
            let span = match cell.vmerge {
                VMerge::Continue => 0,
                VMerge::None => 1,
                VMerge::Restart => {
                    let mut count = 1u32;
                    for (_, below) in shape.rows.iter().skip(row_index + 1) {
                        match below.iter().find(|c| c.start == cell.start) {
                            Some(c) if c.vmerge == VMerge::Continue => count += 1,
                            _ => break,
                        }
                    }
                    count
                }
            };
            row.push(span);
        }
        out.push(row);
    }
    out
}

/// Border stroke from `w:tblBorders` of the table, taken from the first border with a usable color.
fn table_style(tbl: Node<'_, '_>) -> Style {
    let mut style = Style::default();
    let Some(pr) = elements(tbl).into_iter().find(|n| is_w(*n, "tblPr")) else { return style };
    let Some(borders) = elements(pr).into_iter().find(|n| is_w(*n, "tblBorders")) else { return style };
    for edge in elements(borders) {
        let color = w_attr(edge, "color").and_then(units::color_model);
        if let (Some(color), Some(size)) = (color, w_attr(edge, "sz").and_then(units::parse_int)) {
            style.stroke = color;
            style.stroke_width = ((size * 12_700) + 4) / 8;
            break;
        }
    }
    style
}
