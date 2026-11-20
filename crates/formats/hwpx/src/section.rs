//! Section model (`Contents/sectionN.xml`) with native keys for every element.
//!
//! The flow model is built from these parsed blocks. Each block keeps the anchors it needs for
//! writing back, and each unmodeled subtree is recorded as an opaque region with its anchor.
use std::collections::{BTreeMap, HashSet};

use designllm_contracts::{
    AssetRef, FlowBlock, Node, NodeKind, Paragraph, PageSetup, ParagraphFormat, Rect, Section, Size, Style,
    Table, TableCell, TextRun, EMU_PER_HWPUNIT,
};
use roxmltree::Node as XNode;

use crate::header::{child_el, child_els, descendant_els, is_el, paragraph_font_size, NativeLine, PHeader};
use crate::text::{LINE_BREAK_CHAR, OBJECT_CHAR, TAB_CHAR};
use crate::xmlx::{declared_namespace_attributes, Xml, NS_HC, NS_HP, NS_HS};

/// Binary item resolved through the manifest: href, media type, and asset reference.
#[derive(Debug, Clone)]
pub struct BinRes {
    pub asset: AssetRef,
}

/// Binary items by manifest id (for `hc:img/@binaryItemIDRef`).
pub type Bins = BTreeMap<String, BinRes>;

/// An unmodeled subtree that must stay byte-identical.
#[derive(Debug, Clone)]
pub struct OpaqueRec {
    pub key: String,
    pub label: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AtomKind {
    Tab,
    LineBreak,
    /// Inline object (picture, table, equation, drawing object treated as a character).
    Object,
}

#[derive(Debug, Clone)]
pub struct PHidden {
    pub key: String,
    /// Written inside `hp:t` rather than directly inside `hp:run`.
    pub in_t: bool,
}

#[derive(Debug, Clone)]
pub struct PAtom {
    pub key: String,
    pub kind: AtomKind,
    /// Local name of the object element (`pic`, `tbl`, `tab`, ...).
    pub local: String,
    pub char_pr: u32,
    pub in_t: bool,
    /// Unmodeled nodes that follow this atom and are written with it.
    pub hidden_after: Vec<PHidden>,
}

#[derive(Debug, Clone)]
pub struct PPara {
    pub key: String,
    pub id: String,
    pub para_pr: u32,
    pub base_char_pr: u32,
    /// Distinct character shapes used by the original runs, in document order.
    pub orig_cps: Vec<u32>,
    pub runs: Vec<TextRun>,
    pub atoms: Vec<PAtom>,
    pub hidden_start: Vec<PHidden>,
    pub cache_key: Option<String>,
    pub format: ParagraphFormat,
    pub style_ref: Option<String>,
    /// Rewriting the inline runs would drop markup the model does not carry (attributes or namespace
    /// declarations on `hp:run`/`hp:t`, or text directly inside `hp:run`). Such a paragraph keeps its
    /// runs byte for byte; text and character-style edits to it are refused.
    pub regen_locked: bool,
    /// The `pageBreak` attribute of the host `hp:p` as written (None when absent). A saved flag keeps this spelling.
    pub page_break_attr: Option<String>,
    /// The paragraph that carries the page break of a paragraph whose only content is a picture or a table.
    /// It has no runs and no line-segment cache of its own, and it shares its host `hp:p` with the object that
    /// follows it in the flow. The flow model keeps a page break on a paragraph, so this is where the flag is
    /// carried. The host's shape is shared with that object, so the carrier's format and style cannot be
    /// edited, and its break cannot be cleared.
    pub page_break_carrier: bool,
}

#[derive(Debug, Clone)]
pub struct PCell {
    pub id: String,
    pub col: u32,
    pub col_span: u32,
    pub row_span: u32,
    pub width_hu: i64,
    pub width_key: String,
    pub blocks: Vec<PBlock>,
}

/// What a page break set on the host `hp:p` of a picture or table reads back as: the carrier paragraph's format,
/// style and the host's `pageBreak` spelling. Kept for an object whose host has no break, so a new carrier inserted
/// in front of it can be folded into the host when it matches (writer `plan_section`).
#[derive(Debug, Clone)]
pub struct HostShape {
    pub line: NativeLine,
    pub uniform_line: bool,
    pub format: ParagraphFormat,
    pub style_ref: Option<String>,
    pub page_break_attr: Option<String>,
}

#[derive(Debug, Clone)]
pub struct PTable {
    pub host_key: String,
    pub host_cache: Option<String>,
    /// Set for a table that is the only content of its host, which has no page break.
    pub host_shape: Option<HostShape>,
    pub id: String,
    pub sz_key: Option<String>,
    pub widths_hu: Vec<i64>,
    pub rows: Vec<Vec<PCell>>,
}

#[derive(Debug, Clone)]
pub struct PImage {
    pub key: String,
    pub host_key: String,
    pub host_cache: Option<String>,
    /// Set for a picture that is the only content of its host, which has no page break.
    pub host_shape: Option<HostShape>,
    pub id: String,
    pub width_hu: i64,
    pub height_hu: i64,
    /// `hp:sz`, the displayed size that a resize changes.
    pub sz_key: Option<String>,
    /// `hc:pt1`, `hc:pt2`, `hc:pt3` of `hp:imgRect`: the displayed rectangle's corners.
    pub rect_keys: Option<[String; 3]>,
    /// `hc:img`, `hp:orgSz`, `hp:imgClip`, `hp:imgDim`: the children that describe the shown image.
    /// A figure update replaces them together; they are protected regions otherwise.
    pub figure_keys: Option<[String; 4]>,
    /// The host paragraph holds nothing but this picture (and its caches), so removing the picture
    /// removes no other protected content.
    pub sole_content: bool,
    pub asset: AssetRef,
}

#[derive(Debug, Clone)]
pub struct POpaque {
    pub para_pr: Option<u32>,
    pub key: String,
    pub host_cache: Option<String>,
    pub id: String,
    pub label: String,
}

#[derive(Debug, Clone)]
pub enum PBlock {
    Para(PPara),
    Table(PTable),
    Image(PImage),
    Opaque(POpaque),
}

impl PBlock {
    /// Anchor of the host `hp:p` (or of the opaque element itself).
    pub fn host_key(&self) -> &str {
        match self {
            PBlock::Para(p) => &p.key,
            PBlock::Table(t) => &t.host_key,
            PBlock::Image(i) => &i.host_key,
            PBlock::Opaque(o) => &o.key,
        }
    }

    pub fn id(&self) -> &str {
        match self {
            PBlock::Para(p) => &p.id,
            PBlock::Table(t) => &t.id,
            PBlock::Image(i) => &i.id,
            PBlock::Opaque(o) => &o.id,
        }
    }
}

#[derive(Debug, Clone)]
pub struct PSection {
    pub part: String,
    pub id: String,
    pub key: String,
    pub page_key: String,
    pub margin_key: Option<String>,
    pub setup: PageSetup,
    pub blocks: Vec<PBlock>,
    pub opaque: Vec<OpaqueRec>,
    /// Paragraphs whose line spacing is an approximation (see `NativeLine::pitch`).
    pub approximate_lines: usize,
}

pub fn section_id(part: &str) -> String {
    format!("hwpx:{part}")
}

pub fn node_id(part: &str, key: &str) -> String {
    format!("hwpx:{part}|{key}")
}

/// An element is an inline object when its position says "treat as character".
pub(crate) fn is_inline_object(node: XNode<'_, '_>) -> bool {
    child_el(node, NS_HP, "pos").and_then(|p| p.attribute("treatAsChar")) == Some("1")
}

/// One child of an `hp:run`, as the flow projection reads it. `Parser::parse_para` turns each item into runs, atoms and
/// hidden content, and `export.rs` reads the same items for the text of a paragraph: the text of each `Text` and one
/// character for each `Atom`.
#[derive(Clone, Copy)]
pub(crate) enum RunItem<'x> {
    /// Non-empty text of an `hp:t`.
    Text(&'x str),
    /// One protected atom that shows `display`: a `tab` or `lineBreak` element of an `hp:t` (`in_t`), or an inline object
    /// directly in the run (`is_inline_object`).
    Atom { node: XNode<'x, 'x>, kind: AtomKind, display: &'static str, in_t: bool },
    /// The section properties (`hp:secPr`) directly in the run.
    SectionProps(XNode<'x, 'x>),
    /// Hidden content: comments, processing instructions and unmodeled elements. `in_t` marks the ones inside an `hp:t`.
    Hidden { node: XNode<'x, 'x>, in_t: bool },
}

/// The children of an `hp:run` in document order, each with its projection. Text directly inside the run is not an item:
/// `run_has_unmodeled_markup` locks such a paragraph instead.
pub(crate) fn run_items<'x>(run: XNode<'x, 'x>) -> Vec<RunItem<'x>> {
    let mut items = Vec::new();
    for child in run.children() {
        if child.is_text() {
            continue;
        }
        if child.is_comment() || child.is_pi() {
            items.push(RunItem::Hidden { node: child, in_t: false });
        } else if is_el(child, NS_HP, "t") {
            for item in child.children() {
                if item.is_text() {
                    let text = item.text().unwrap_or("");
                    if !text.is_empty() {
                        items.push(RunItem::Text(text));
                    }
                } else if item.is_comment() || item.is_pi() {
                    items.push(RunItem::Hidden { node: item, in_t: true });
                } else if item.is_element() {
                    match item.tag_name().name() {
                        "tab" => items.push(RunItem::Atom { node: item, kind: AtomKind::Tab, display: TAB_CHAR, in_t: true }),
                        "lineBreak" => items.push(RunItem::Atom { node: item, kind: AtomKind::LineBreak, display: LINE_BREAK_CHAR, in_t: true }),
                        _ => items.push(RunItem::Hidden { node: item, in_t: true }),
                    }
                }
            }
        } else if child.is_element() && child.tag_name().name() == "secPr" {
            items.push(RunItem::SectionProps(child));
        } else if child.is_element() {
            if is_inline_object(child) {
                items.push(RunItem::Atom { node: child, kind: AtomKind::Object, display: OBJECT_CHAR, in_t: false });
            } else {
                items.push(RunItem::Hidden { node: child, in_t: false });
            }
        }
    }
    items
}

fn label_of(node: XNode<'_, '_>) -> String {
    if node.is_element() {
        node.tag_name().name().to_string()
    } else if node.is_comment() {
        "comment".into()
    } else {
        "processing-instruction".into()
    }
}

struct Parser<'x, 'h> {
    xml: &'x Xml<'x>,
    header: &'h PHeader,
    bins: &'h Bins,
    part: String,
    opaque: Vec<OpaqueRec>,
    approximate_lines: usize,
}

impl<'x, 'h> Parser<'x, 'h> {
    fn record(&mut self, node: XNode<'x, 'x>) -> Result<String, String> {
        let key = self.xml.key(node).ok_or("node without anchor")?.to_string();
        self.opaque.push(OpaqueRec { key: key.clone(), label: label_of(node) });
        Ok(key)
    }

    fn parse_para(&mut self, p: XNode<'x, 'x>) -> Result<PPara, String> {
        let xml = self.xml;
        let key = xml.key(p).ok_or("paragraph without anchor")?.to_string();
        let para_pr = xml.attr_u32(p, "paraPrIDRef").unwrap_or(0);
        let style_id = xml.attr_u32(p, "styleIDRef");
        let header = self.header;
        let shape = header.para_pr(para_pr).ok_or_else(|| format!("paragraph {key} refers to missing paraPr {para_pr}"))?;
        let style_ref = match style_id {
            Some(sid) => Some(
                self.header
                    .style_by_id(sid)
                    .map(|s| s.name.clone())
                    .ok_or_else(|| format!("paragraph {key} refers to missing style {sid}"))?,
            ),
            None => None,
        };
        let mut runs: Vec<TextRun> = Vec::new();
        let mut run_cps: Vec<u32> = Vec::new();
        let mut atoms: Vec<PAtom> = Vec::new();
        let mut hidden_start: Vec<PHidden> = Vec::new();
        let mut base_char_pr: Option<u32> = None;
        let mut orig_cps: Vec<u32> = Vec::new();
        let mut cache_key = None;
        let mut regen_locked = false;

        for child in p.children() {
            if is_el(child, NS_HP, "run") {
                let cp = xml.attr_u32(child, "charPrIDRef").unwrap_or(0);
                if carries_unmodeled_markup(xml, child, &["charPrIDRef"]) || run_has_unmodeled_markup(xml, child) {
                    regen_locked = true;
                }
                if base_char_pr.is_none() {
                    base_char_pr = Some(cp);
                }
                if !orig_cps.contains(&cp) {
                    orig_cps.push(cp);
                }
                let style = self
                    .header
                    .char_pr(cp)
                    .map(|c| c.style.clone())
                    .ok_or_else(|| format!("run in {key} refers to missing charPr {cp}"))?;
                for item in run_items(child) {
                    match item {
                        RunItem::Text(text) => {
                            let can_merge = run_cps.last() == Some(&cp) && runs.last().map(|r| !r.protected).unwrap_or(false);
                            if can_merge {
                                if let Some(r) = runs.last_mut() {
                                    r.text.push_str(text);
                                }
                            } else {
                                runs.push(text_run(text, style.clone()));
                                run_cps.push(cp);
                            }
                        }
                        RunItem::Atom { node, kind, display, in_t } => {
                            let k = self.record(node)?;
                            runs.push(protected_run(display, style.clone()));
                            run_cps.push(cp);
                            atoms.push(PAtom {
                                key: k,
                                kind,
                                local: node.tag_name().name().to_string(),
                                char_pr: cp,
                                in_t,
                                hidden_after: Vec::new(),
                            });
                        }
                        RunItem::SectionProps(node) => {
                            // The section properties are kept as one hidden node, but the page setup
                            // inside them (hp:pagePr) is modeled and may be edited. Only the other
                            // children are protected regions.
                            for inner in node.children().filter(|c| c.is_element() && c.tag_name().name() != "pagePr") {
                                self.record(inner)?;
                            }
                            let k = self.xml.key(node).ok_or("secPr without anchor")?.to_string();
                            push_hidden(&mut atoms, &mut hidden_start, PHidden { key: k, in_t: false });
                        }
                        RunItem::Hidden { node, in_t } => {
                            let k = self.record(node)?;
                            push_hidden(&mut atoms, &mut hidden_start, PHidden { key: k, in_t });
                        }
                    }
                }
            } else if is_el(child, NS_HP, "linesegarray") {
                cache_key = xml.key(child).map(str::to_string);
            } else if child.is_element() || child.is_comment() || child.is_pi() {
                self.record(child)?;
            }
        }

        // The line pitch of PERCENT spacing scales with the largest font of the paragraph.
        let base_size = base_char_pr.and_then(|cp| self.header.char_pr(cp)).map(|c| c.style.font_size);
        let max_size = paragraph_font_size(&runs, base_size);
        let (line_spacing, approximate) = shape.line.pitch(max_size);
        if approximate {
            self.approximate_lines += 1;
        }
        // The break is set by `pageBreak` on `hp:p` or by the shape's `hh:breakSetting/@pageBreakBefore`.
        let page_break_attr = p.attribute("pageBreak").map(str::to_string);
        let page_break_before = flag_on(page_break_attr.as_deref()) || shape.format.page_break_before;
        let format = ParagraphFormat { line_spacing, page_break_before, ..shape.format.clone() };

        Ok(PPara {
            id: node_id(&self.part, &key),
            key,
            para_pr,
            base_char_pr: base_char_pr.unwrap_or(0),
            orig_cps,
            runs,
            atoms,
            hidden_start,
            cache_key,
            format,
            style_ref,
            regen_locked,
            page_break_attr,
            page_break_carrier: false,
        })
    }

    /// The paragraph that carries the page break of an image-only paragraph (`PPara::page_break_carrier`).
    /// Its pitch is the one an empty paragraph of its shape gets, which is what a save verifies against.
    fn carrier_of(&self, host: &PPara) -> Result<PPara, String> {
        let shape = self
            .header
            .para_pr(host.para_pr)
            .ok_or_else(|| format!("paragraph {} refers to missing paraPr {}", host.key, host.para_pr))?;
        let base_size = self.header.char_pr(host.base_char_pr).map(|c| c.style.font_size);
        let (line_spacing, _) = shape.line.pitch(paragraph_font_size(&[], base_size));
        Ok(PPara {
            id: format!("{}#pb", host.id),
            key: host.key.clone(),
            para_pr: host.para_pr,
            base_char_pr: host.base_char_pr,
            orig_cps: host.orig_cps.clone(),
            runs: Vec::new(),
            atoms: Vec::new(),
            hidden_start: Vec::new(),
            cache_key: None,
            format: ParagraphFormat { line_spacing, ..host.format.clone() },
            style_ref: host.style_ref.clone(),
            regen_locked: true,
            page_break_attr: host.page_break_attr.clone(),
            page_break_carrier: true,
        })
    }

    /// The carrier of the host of a picture or table that has a page break; or, for a host without one, the shape
    /// that a break on it reads back as.
    fn host_break(&self, host: &PPara) -> Result<(Option<PBlock>, Option<HostShape>), String> {
        let carrier = self.carrier_of(host)?;
        if host.format.page_break_before {
            return Ok((Some(PBlock::Para(carrier)), None));
        }
        let para_pr = self.header.para_pr(host.para_pr).ok_or("missing host paraPr")?;
        let shape = HostShape {
            line: para_pr.line.clone(),
            uniform_line: para_pr.uniform_line,
            format: ParagraphFormat { page_break_before: true, ..carrier.format },
            style_ref: carrier.style_ref,
            page_break_attr: carrier.page_break_attr,
        };
        Ok((None, Some(shape)))
    }

    /// Classifies one `hp:p` as paragraphs, a block image, a block table, or an opaque block. A paragraph whose
    /// only content is a picture or a table, with a page break, gives its carrier paragraph in front of the object.
    fn parse_block(&mut self, p: XNode<'x, 'x>, is_section_head: bool) -> Result<Vec<PBlock>, String> {
        let mark = self.opaque.len();
        let para = self.parse_para(p)?;
        let text_present = para.runs.iter().any(|r| !r.protected && !r.text.is_empty());
        if !text_present && para.atoms.len() == 1 && para.atoms[0].kind == AtomKind::Object {
            let atom = &para.atoms[0];
            let object = self.xml.node(&atom.key).ok_or("object anchor lost")?;
            let host_cache = para.cache_key.clone();
            // Markup on the host paragraph, and a page or column break, would be lost with it, so such a
            // picture is not removable. The section's leading secPr/colPr pair on the first paragraph
            // relocates to the next first paragraph on save, so it does not lock a head picture.
            let breaks = flag_on(p.attribute("pageBreak")) || p.attribute("columnBreak") == Some("1");
            let head_keys = if is_section_head { section_head_keys(self.xml, p) } else { Vec::new() };
            let only_relocatable_head = para.hidden_start.iter().all(|h| head_keys.contains(&h.key));
            // The attributes of `hp:p` in the OWPML schema; any other, or a namespace declaration, is markup.
            let host_markup = carries_unmodeled_markup(self.xml, p, &["id", "paraPrIDRef", "styleIDRef", "pageBreak", "columnBreak", "merged"]);
            let sole_content = !breaks
                && !host_markup
                && !para.regen_locked
                && only_relocatable_head
                && para.atoms.iter().all(|a| a.hidden_after.is_empty());
            match atom.local.as_str() {
                "pic" => {
                    if let Some(mut image) = self.parse_image(object, &para.key)? {
                        // The picture element is modeled (its size can change); its unmodeled
                        // children were recorded by parse_image instead.
                        let picture_key = atom.key.clone();
                        self.opaque.retain(|r| r.key != picture_key);
                        image.host_cache = host_cache;
                        image.sole_content = sole_content;
                        let (carrier, host_shape) = self.host_break(&para)?;
                        image.host_shape = host_shape;
                        let mut blocks: Vec<PBlock> = carrier.into_iter().collect();
                        blocks.push(PBlock::Image(image));
                        return Ok(blocks);
                    }
                }
                "tbl" => {
                    if let Some(mut table) = self.parse_table(object, &para.key)? {
                        // The table element is modeled, so its bytes may change; its unmodeled
                        // children were recorded by parse_table instead.
                        let table_key = atom.key.clone();
                        self.opaque.retain(|r| r.key != table_key);
                        table.host_cache = host_cache;
                        let (carrier, host_shape) = self.host_break(&para)?;
                        table.host_shape = host_shape;
                        let mut blocks: Vec<PBlock> = carrier.into_iter().collect();
                        blocks.push(PBlock::Table(table));
                        return Ok(blocks);
                    }
                    // The grid cannot be modeled: the whole host paragraph becomes one opaque block.
                    self.opaque.truncate(mark);
                    self.record(p)?;
                    return Ok(vec![PBlock::Opaque(POpaque {
                        key: para.key.clone(),
                        para_pr: Some(para.para_pr),
                        host_cache,
                        id: para.id.clone(),
                        label: "hp:p (table with unresolved grid)".into(),
                    })]);
                }
                _ => {}
            }
        }
        Ok(vec![PBlock::Para(para)])
    }

    fn parse_image(&mut self, pic: XNode<'x, 'x>, host_key: &str) -> Result<Option<PImage>, String> {
        let xml = self.xml;
        let img = match descendant_els(pic, NS_HC, "img").into_iter().next() {
            Some(img) => img,
            None => return Ok(None),
        };
        let bin_id = img.attribute("binaryItemIDRef").ok_or("hc:img without binaryItemIDRef")?.to_string();
        let bin = self
            .bins
            .get(&bin_id)
            .ok_or_else(|| format!("picture refers to unknown binary item {bin_id}"))?;
        let dim = |n: Option<XNode<'x, 'x>>, attr: &str| -> i64 { n.and_then(|n| xml.attr_i64(n, attr)).unwrap_or(0) };
        let sz = child_el(pic, NS_HP, "sz");
        let org = child_el(pic, NS_HP, "orgSz");
        let mut width = dim(sz, "width");
        let mut height = dim(sz, "height");
        if width <= 0 || height <= 0 {
            width = dim(org, "width");
            height = dim(org, "height");
        }
        let key = xml.key(pic).ok_or("picture without anchor")?.to_string();
        // Unmodeled children are protected; hp:sz and hp:imgRect hold the editable size.
        for child in pic.children().filter(|c| c.is_element()) {
            if !is_el(child, NS_HP, "sz") && !is_el(child, NS_HP, "imgRect") {
                self.record(child)?;
            }
        }
        // The children that describe the shown image are modeled for a figure update only when they
        // are direct children of the picture (the only place where the protected-region record covers them).
        let direct_key = |n: Option<XNode<'x, 'x>>| -> Option<String> {
            n.filter(|n| n.parent().map(|p| p.id()) == Some(pic.id())).and_then(|n| xml.key(n).map(str::to_string))
        };
        let figure_keys = match (
            direct_key(Some(img)),
            direct_key(org),
            direct_key(child_el(pic, NS_HP, "imgClip")),
            direct_key(child_el(pic, NS_HP, "imgDim")),
        ) {
            (Some(a), Some(b), Some(c), Some(d)) => Some([a, b, c, d]),
            _ => None,
        };
        let sz_key = sz.and_then(|s| xml.key(s).map(str::to_string));
        let rect_keys = child_el(pic, NS_HP, "imgRect").and_then(|rect| {
            let points: Vec<String> = ["pt1", "pt2", "pt3"]
                .iter()
                .filter_map(|name| child_el(rect, NS_HC, name).and_then(|p| xml.key(p).map(str::to_string)))
                .collect();
            <[String; 3]>::try_from(points).ok()
        });
        Ok(Some(PImage {
            key,
            host_key: host_key.to_string(),
            host_cache: None,
            host_shape: None,
            id: node_id(&self.part, host_key),
            width_hu: width,
            height_hu: height,
            sz_key,
            rect_keys,
            figure_keys,
            sole_content: false,
            asset: bin.asset.clone(),
        }))
    }

    /// Returns `None` when the grid cannot be resolved to column widths.
    fn parse_table(&mut self, tbl: XNode<'x, 'x>, host_key: &str) -> Result<Option<PTable>, String> {
        let xml = self.xml;
        let key = xml.key(tbl).ok_or("table without anchor")?.to_string();
        let declared_cols = xml.attr_u32(tbl, "colCnt");
        let sz_key = child_el(tbl, NS_HP, "sz").and_then(|s| xml.key(s).map(str::to_string));
        let mut rows: Vec<Vec<PCell>> = Vec::new();
        let mut max_cols = 0u32;
        for tr in child_els(tbl, NS_HP, "tr") {
            let mut row = Vec::new();
            for tc in child_els(tr, NS_HP, "tc") {
                let cell = self.parse_cell(tc)?;
                max_cols = max_cols.max(cell.col + cell.col_span);
                row.push(cell);
            }
            rows.push(row);
        }
        let col_count = declared_cols.unwrap_or(max_cols);
        let mut widths: Vec<Option<i64>> = vec![None; col_count as usize];
        for cell in rows.iter().flatten() {
            let idx = cell.col as usize;
            if cell.col_span == 1 && idx < widths.len() && widths[idx].is_none() {
                widths[idx] = Some(cell.width_hu);
            }
        }
        if widths.is_empty() || widths.iter().any(Option::is_none) {
            return Ok(None);
        }
        // Unmodeled table children keep their bytes: position, margins, and anything else.
        for child in tbl.children() {
            if child.is_element() && !is_el(child, NS_HP, "tr") && !is_el(child, NS_HP, "sz") {
                self.record(child)?;
            }
        }
        Ok(Some(PTable {
            id: node_id(&self.part, &key),
            host_key: host_key.to_string(),
            host_cache: None,
            host_shape: None,
            sz_key,
            widths_hu: widths.into_iter().map(|w| w.unwrap_or(0)).collect(),
            rows,
        }))
    }

    fn parse_cell(&mut self, tc: XNode<'x, 'x>) -> Result<PCell, String> {
        let xml = self.xml;
        let key = xml.key(tc).ok_or("cell without anchor")?.to_string();
        let addr = child_el(tc, NS_HP, "cellAddr");
        let col = addr.and_then(|a| xml.attr_u32(a, "colAddr")).unwrap_or(0);
        let span = child_el(tc, NS_HP, "cellSpan");
        let col_span = span.and_then(|s| xml.attr_u32(s, "colSpan")).unwrap_or(1).max(1);
        let row_span = span.and_then(|s| xml.attr_u32(s, "rowSpan")).unwrap_or(1).max(1);
        let size = child_el(tc, NS_HP, "cellSz").ok_or_else(|| format!("cell {key} has no hp:cellSz"))?;
        let width_hu = xml.attr_i64(size, "width").unwrap_or(0);
        let width_key = xml.key(size).ok_or("cellSz without anchor")?.to_string();
        for child in tc.children() {
            if child.is_element()
                && !is_el(child, NS_HP, "subList")
                && !is_el(child, NS_HP, "cellAddr")
                && !is_el(child, NS_HP, "cellSpan")
                && !is_el(child, NS_HP, "cellSz")
            {
                self.record(child)?;
            }
        }
        let mut blocks = Vec::new();
        if let Some(list) = child_el(tc, NS_HP, "subList") {
            for child in list.children() {
                if is_el(child, NS_HP, "p") {
                    blocks.extend(self.parse_block(child, false)?);
                } else if child.is_element() || child.is_comment() || child.is_pi() {
                    let k = self.record(child)?;
                    blocks.push(PBlock::Opaque(POpaque {
                        id: node_id(&self.part, &k),
                        key: k,
                        para_pr: None,
                        host_cache: None,
                        label: label_of(child),
                    }));
                }
            }
        }
        Ok(PCell { id: node_id(&self.part, &key), col, col_span, row_span, width_hu, width_key, blocks })
    }
}

/// True when rewriting the element would drop markup that the model does not carry: an attribute
/// other than the listed modeled ones, or a namespace declaration written on the element itself.
fn carries_unmodeled_markup<'a>(xml: &Xml<'a>, node: XNode<'_, 'a>, modeled: &[&str]) -> bool {
    if node.attributes().any(|a| a.namespace().is_some() || !modeled.contains(&a.name())) {
        return true;
    }
    let (start, _) = xml.span(node);
    !declared_namespace_attributes(xml.src, start, xml.start_tag_end(node)).is_empty()
}

/// True when the run holds text directly inside it, or an `hp:t` with markup the model does not carry. Rewriting either
/// would drop that markup, so the paragraph is locked (`PPara::regen_locked`).
fn run_has_unmodeled_markup<'a>(xml: &Xml<'a>, run: XNode<'_, 'a>) -> bool {
    run.children().any(|rc| {
        if rc.is_text() {
            !rc.text().unwrap_or("").trim().is_empty()
        } else {
            is_el(rc, NS_HP, "t") && carries_unmodeled_markup(xml, rc, &[])
        }
    })
}

/// Ids of every block of a section, top level and inside table cells.
pub fn block_ids_of(sec: &PSection) -> Vec<String> {
    fn walk(blocks: &[PBlock], out: &mut Vec<String>) {
        for block in blocks {
            out.push(block.id().to_string());
            if let PBlock::Table(t) = block {
                for cell in t.rows.iter().flatten() {
                    walk(&cell.blocks, out);
                }
            }
        }
    }
    let mut out = Vec::new();
    walk(&sec.blocks, &mut out);
    out
}

/// Ids of the paragraphs of a section (top level and inside table cells) whose runs must not be rewritten.
pub fn locked_paragraph_ids(sec: &PSection) -> Vec<String> {
    fn walk(blocks: &[PBlock], out: &mut Vec<String>) {
        for block in blocks {
            match block {
                PBlock::Para(p) => {
                    if p.regen_locked {
                        out.push(p.id.clone());
                    }
                }
                PBlock::Table(t) => {
                    for cell in t.rows.iter().flatten() {
                        walk(&cell.blocks, out);
                    }
                }
                PBlock::Image(_) | PBlock::Opaque(_) => {}
            }
        }
    }
    let mut out = Vec::new();
    walk(&sec.blocks, &mut out);
    out
}

/// Ids of the carrier paragraphs of the top-level blocks (`PPara::page_break_carrier`).
pub fn carrier_ids(sections: &[PSection]) -> HashSet<&str> {
    sections
        .iter()
        .flat_map(|s| s.blocks.iter())
        .filter_map(|b| match b {
            PBlock::Para(p) if p.page_break_carrier => Some(p.id.as_str()),
            _ => None,
        })
        .collect()
}

fn push_hidden(atoms: &mut [PAtom], hidden_start: &mut Vec<PHidden>, hidden: PHidden) {
    match atoms.last_mut() {
        Some(atom) => atom.hidden_after.push(hidden),
        None => hidden_start.push(hidden),
    }
}

fn text_run(text: &str, style: Style) -> TextRun {
    TextRun { style, ..TextRun::new(String::new(), text) }
}

fn protected_run(display: &str, style: Style) -> TextRun {
    TextRun { style, protected: true, ..TextRun::new(String::new(), display) }
}

/// `pageBreak` is an xsd:boolean attribute: `1` and `true` are on, `0`, `false`, and an absent attribute are off.
fn flag_on(value: Option<&str>) -> bool {
    matches!(value, Some("1") | Some("true"))
}

/// Leading `<hp:secPr>` and optional immediately-following `<hp:ctrl>` in the first `<hp:run>` of `p`.
pub(crate) fn leading_section_head<'a>(p: XNode<'a, 'a>) -> Option<(XNode<'a, 'a>, Option<XNode<'a, 'a>>)> {
    let run = child_el(p, NS_HP, "run")?;
    let mut elems = run.children().filter(|c| c.is_element());
    let first = elems.next()?;
    if !is_el(first, NS_HP, "secPr") {
        return None;
    }
    let ctrl = elems.next().filter(|c| is_el(*c, NS_HP, "ctrl"));
    Some((first, ctrl))
}

fn section_head_keys(xml: &Xml<'_>, p: XNode<'_, '_>) -> Vec<String> {
    let Some((sec_pr, ctrl)) = leading_section_head(p) else {
        return Vec::new();
    };
    let mut keys = Vec::new();
    if let Some(k) = xml.key(sec_pr) {
        keys.push(k.to_string());
    }
    if let Some(ctrl_node) = ctrl {
        let is_plain_col_ctrl = child_els(ctrl_node, NS_HP, "colPr").len() == 1
            && ctrl_node.children().filter(|c| c.is_element()).count() == 1;
        if is_plain_col_ctrl {
            if let Some(k) = xml.key(ctrl_node) {
                keys.push(k.to_string());
            }
        }
    }
    keys
}

/// Parses one section part into blocks, page setup, and the list of opaque regions.
pub fn parse_section<'x>(xml: &'x Xml<'x>, part: &str, header: &PHeader, bins: &Bins) -> Result<PSection, String> {
    let root = xml.doc.root_element();
    if !is_el(root, NS_HS, "sec") {
        return Err(format!("{part}: root is not hs:sec"));
    }
    let mut parser = Parser { xml, header, bins, part: part.to_string(), opaque: Vec::new(), approximate_lines: 0 };
    let mut blocks = Vec::new();
    let mut seen_first_p = false;
    for child in root.children() {
        if is_el(child, NS_HP, "p") {
            blocks.extend(parser.parse_block(child, !seen_first_p)?);
            seen_first_p = true;
        } else if child.is_element() {
            let key = parser.record(child)?;
            blocks.push(PBlock::Opaque(POpaque { id: node_id(part, &key), key, para_pr: None, host_cache: None, label: label_of(child) }));
        } else if child.is_comment() || child.is_pi() {
            parser.record(child)?;
        }
    }
    let pagepr = descendant_els(root, NS_HP, "pagePr")
        .into_iter()
        .next()
        .ok_or_else(|| format!("{part}: section has no hp:pagePr"))?;
    let margin = child_el(pagepr, NS_HP, "margin");
    let g = |n: Option<XNode<'x, 'x>>, a: &str| n.and_then(|n| xml.attr_i64(n, a)).unwrap_or(0);
    let setup = PageSetup {
        size: Size { width: g(Some(pagepr), "width") * EMU_PER_HWPUNIT, height: g(Some(pagepr), "height") * EMU_PER_HWPUNIT },
        margin_left: g(margin, "left") * EMU_PER_HWPUNIT,
        margin_top: g(margin, "top") * EMU_PER_HWPUNIT,
        margin_right: g(margin, "right") * EMU_PER_HWPUNIT,
        margin_bottom: g(margin, "bottom") * EMU_PER_HWPUNIT,
    };
    let page_key = xml.key(pagepr).ok_or("pagePr without anchor")?.to_string();
    let margin_key = margin.and_then(|m| xml.key(m).map(str::to_string));
    let key = xml.key(root).ok_or("section root without anchor")?.to_string();
    Ok(PSection {
        part: part.to_string(),
        id: section_id(part),
        key,
        page_key,
        margin_key,
        setup,
        blocks,
        opaque: parser.opaque,
        approximate_lines: parser.approximate_lines,
    })
}

/// Line-segment cache anchors of a block and of every paragraph nested inside it.
pub fn cache_keys(block: &PBlock) -> Vec<String> {
    fn collect(block: &PBlock, out: &mut Vec<String>) {
        match block {
            PBlock::Para(p) => out.extend(p.cache_key.clone()),
            PBlock::Table(t) => {
                out.extend(t.host_cache.clone());
                for cell in t.rows.iter().flatten() {
                    for inner in &cell.blocks {
                        collect(inner, out);
                    }
                }
            }
            PBlock::Image(i) => out.extend(i.host_cache.clone()),
            PBlock::Opaque(o) => out.extend(o.host_cache.clone()),
        }
    }
    let mut out = Vec::new();
    collect(block, &mut out);
    out
}

/// Converts a parsed section to the flow model with stable ids.
pub fn flow_section(sec: &PSection) -> Section {
    Section {
        id: sec.id.clone(),
        setup: sec.setup.clone(),
        blocks: sec.blocks.iter().map(flow_block).collect(),
    }
}

pub fn flow_block(block: &PBlock) -> FlowBlock {
    match block {
        PBlock::Para(p) => FlowBlock::Paragraph(flow_paragraph(p)),
        PBlock::Table(t) => FlowBlock::Table(flow_table(t)),
        PBlock::Image(i) => FlowBlock::Image(flow_image(i)),
        PBlock::Opaque(o) => FlowBlock::Opaque { id: o.id.clone(), label: o.label.clone() },
    }
}

pub fn flow_paragraph(p: &PPara) -> Paragraph {
    let runs = p
        .runs
        .iter()
        .enumerate()
        .map(|(i, r)| TextRun { id: format!("{}#r{i}", p.id), ..r.clone() })
        .collect();
    Paragraph { id: p.id.clone(), runs, style_ref: p.style_ref.clone(), format: p.format.clone() }
}

pub fn flow_table(t: &PTable) -> Table {
    let rows = t
        .rows
        .iter()
        .map(|row| {
            row.iter()
                .map(|c| TableCell {
                    id: c.id.clone(),
                    blocks: c.blocks.iter().map(flow_block).collect(),
                    column_span: c.col_span,
                    row_span: c.row_span,
                })
                .collect()
        })
        .collect();
    Table {
        id: t.id.clone(),
        column_widths: t.widths_hu.iter().map(|w| w * EMU_PER_HWPUNIT).collect(),
        rows,
        style: Style::default(),
    }
}

pub fn flow_image(i: &PImage) -> Node {
    Node {
        id: i.id.clone(),
        kind: NodeKind::Image,
        bounds: Rect { x: 0, y: 0, width: i.width_hu * EMU_PER_HWPUNIT, height: i.height_hu * EMU_PER_HWPUNIT },
        text: String::new(),
        style: Style::default(),
        asset: Some(i.asset.clone()),
        children: Vec::new(),
        layout: None,
        interactions: Vec::new(),
        variants: BTreeMap::new(),
        ext: Default::default(),
    }
}
