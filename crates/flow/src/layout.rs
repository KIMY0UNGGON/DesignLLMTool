//! Paged layout of a FlowDocument into a RenderScene.
//!
//! Model interpretation (shared with format adapters):
//! - `TextRun::style` is the effective run style; `Paragraph::style_ref` names the paragraph
//!   style used for named-style propagation only.
//! - `ParagraphFormat.line_spacing`: 0 = single (font ascent+descent+line gap);
//!   > 0 = exact baseline-to-baseline pitch in EMU. With `line_spacing_percent` it is the pitch derived from the
//!   percentage (`line_spacing::refresh_pitch`); a proportional paragraph without one gets it here
//!   (`line_spacing::layout_pitch`).
//! - `ParagraphFormat.indent`: first-line indent in EMU; negative = hanging indent
//!   (first line at the left edge, following lines indented by |indent|).
//! - `Table.column_widths` is the grid; cells use HTML spans (covered positions have no cell).
//! - Block images use `bounds.width/height`, aligned by `style.align`; too large images are
//!   scaled to fit with a diagnostic.
//! - Each section starts on a new page. A paragraph with `format.page_break_before` starts a new page too, unless
//!   nothing is placed on the current page yet; its space before is dropped there, as after an automatic move.
//!   Such a flag in a table cell has no effect. Text-only table rows may split at line boundaries.
//!   With `LayoutOptions::page_break_carriers` (HWPX), a listed paragraph with the flag and no content right in front of
//!   a table or an image carries that object's break: it starts the page and takes no height.
//!
//! Output conventions used by the renderer's hit testing:
//! - Text primitive per (line, run piece); node_id = run id; origin = baseline start.
//! - SourceRegion per (line, run) with logical_range in paragraph grapheme units and absolute
//!   grapheme_rects (one per logical grapheme, line-box height).
//! - Empty lines get a zero-width region with an empty range; paragraph/table blocks and cells
//!   get regions without logical_range; images and opaque blocks get node regions.
use crate::linebreak::{break_between, is_space};
use crate::text::{is_forced_break, LogicalParagraph};
use crate::text_links::TextLinkSpan;
use crate::tree::{diag, page_setup_problem};
use designllm_contracts::*;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::ops::Range;
use unicode_segmentation::UnicodeSegmentation;

pub const LAYOUT_ENGINE: &str = "designllm-flow";
pub const LAYOUT_VERSION: &str = "1";
/// Default tab stop interval (0.5 inch).
pub const DEFAULT_TAB_STOP: i64 = EMU_PER_INCH / 2;
/// Table cell padding (left/right 0.075 inch, top/bottom 0.5 mm).
pub const CELL_PADDING_X: i64 = 68_580;
pub const CELL_PADDING_Y: i64 = 18_000;
const MAX_NESTING: usize = 16;

/// Lay out every section into pages (see module docs for the interpretation of the model).
pub fn layout_flow(document: &FlowDocument, context: &EngineContext<'_>) -> DltResult<RenderScene> {
    layout_flow_with_links(document, &[], context)
}

/// [`layout_flow`] with the hyperlinks of text ranges (see `text_links`). A span with an empty range or with a URL that
/// `check_interaction_url` refuses gives no link region.
pub fn layout_flow_with_links(document: &FlowDocument, links: &[TextLinkSpan], context: &EngineContext<'_>) -> DltResult<RenderScene> {
    layout_flow_folding(document, links, Folding::Off, context)
}

/// Readings of the model that differ between source formats. The default is the DOCX and HWP reading: Word draws an
/// empty paragraph as a line.
#[derive(Debug, Clone, Copy, Default)]
pub struct LayoutOptions<'a> {
    /// HWPX: ids of the page-break carriers ([`is_page_break_carrier`]) that have no paragraph of their own in the
    /// file: their break is kept on the paragraph of the object after them (`designllm_hwpx` writer `folded_carriers`),
    /// so they start the page and take no line. Every other paragraph, and every paragraph when this is `None`, keeps
    /// its line.
    pub page_break_carriers: Option<&'a HashSet<&'a str>>,
}

/// Which page-break carriers take no line.
#[derive(Debug, Clone, Copy)]
enum Folding<'a> {
    Off,
    Listed(&'a HashSet<&'a str>),
}

/// [`layout_flow_with_links`] with the readings of `options`.
pub fn layout_flow_with_options(document: &FlowDocument, links: &[TextLinkSpan], options: LayoutOptions<'_>, context: &EngineContext<'_>) -> DltResult<RenderScene> {
    let folding = options.page_break_carriers.map_or(Folding::Off, Folding::Listed);
    layout_flow_folding(document, links, folding, context)
}

/// HWPX: [`layout_flow_with_links`] where the page-break carriers in `folded` take no line
/// ([`LayoutOptions::page_break_carriers`]).
pub fn layout_flow_with_carriers(document: &FlowDocument, links: &[TextLinkSpan], folded: &HashSet<&str>, context: &EngineContext<'_>) -> DltResult<RenderScene> {
    layout_flow_folding(document, links, Folding::Listed(folded), context)
}

fn layout_flow_folding(document: &FlowDocument, links: &[TextLinkSpan], folding: Folding<'_>, context: &EngineContext<'_>) -> DltResult<RenderScene> {
    // Contract v1.2 content is refused, not laid out without it (P0).
    crate::guard::refuse_v12("Flow layouts", crate::guard::document_v12(document).into_iter().map(|f| (None, f)).collect())?;
    let mut text_links: BTreeMap<Id, Vec<TextLinkSpan>> = BTreeMap::new();
    for span in links.iter().filter(|s| s.start < s.end && check_interaction_url(&s.url).is_ok()) {
        text_links.entry(span.paragraph_id.clone()).or_default().push(span.clone());
    }
    layout_document(document, context.renderer, text_links, folding)
}

/// The layout of `document` after its v1.2 content has been refused (see [`layout_flow`]).
fn layout_document(document: &FlowDocument, renderer: &dyn RenderProvider, text_links: BTreeMap<Id, Vec<TextLinkSpan>>, folding: Folding<'_>) -> DltResult<RenderScene> {
    let mut link_targets = BTreeMap::new();
    for section in &document.sections { open_url_targets(&section.blocks, &mut link_targets)?; }
    let mut l = Layouter { renderer, pages: Vec::new(), diagnostics: Vec::new(), seen: BTreeSet::new(), sections: Vec::new(), depth: 0, links: Vec::new(), link_targets, text_links, folding };
    if document.sections.is_empty() {
        l.diag("empty_document", "The document has no sections; one blank page is shown", None, false);
        let setup = PageSetup::default();
        l.new_page("empty", setup.size);
    }
    for section in &document.sections { l.section(section)?; }
    Ok(l.finish())
}

/// Whether `blocks[index]` is a page-break carrier: a paragraph with `page_break_before` and no content, right in
/// front of a table or an image. HWPX reads a picture's or table's break this way (the model keeps a page break on
/// paragraphs only) and saves it back on the object's paragraph; its layout gives a folded carrier no line
/// ([`LayoutOptions::page_break_carriers`]). "No content" is no runs at all, as the reader gives a carrier back: an
/// empty run would not survive the save. An index past the blocks is no carrier.
pub fn is_page_break_carrier(blocks: &[FlowBlock], index: usize) -> bool {
    matches!(blocks.get(index), Some(FlowBlock::Paragraph(p)) if p.format.page_break_before && p.runs.is_empty())
        && matches!(blocks.get(index + 1), Some(FlowBlock::Table(_) | FlowBlock::Image(_)))
}

/// Fingerprint of the font faces actually referenced by glyphs in the pages.
pub fn scene_font_fingerprint(pages: &[RenderPage]) -> String {
    let mut faces = BTreeSet::new();
    for page in pages {
        for primitive in &page.primitives {
            if let Primitive::Text { shaped, .. } = primitive {
                for face in &shaped.fonts_used { faces.insert(face_identity(face)); }
            }
        }
    }
    sha256(faces.into_iter().collect::<Vec<_>>().join("\n").as_bytes())
}

/// Stable identity of a face: file hash, face index, PostScript name and variation settings.
pub fn face_identity(face: &FontFaceRef) -> String {
    let variations: Vec<String> = face.variations.iter().map(|(k, v)| format!("{k}={v}")).collect();
    format!("{}#{}:{};{}", face.file_sha256, face.face_index, face.postscript_name, variations.join(","))
}

#[derive(Default)]
struct Unit {
    height: i64,
    gap_before: i64,
    gap_after: i64,
    items: Vec<Primitive>,
    regions: Vec<SourceRegion>,
    /// Block ids (paragraph/table) whose extent includes this unit.
    blocks: Vec<Id>,
    /// Outline of a table row or row fragment; its stroke at the page edges is checked in `place`.
    outline: Option<TableOutline>,
    /// Link rectangles of hyperlinked text in the unit (see `line_unit`).
    text_links: Vec<TextLink>,
}

/// The link rectangle of a hyperlinked text range on one line, with the paragraph it belongs to and the URL.
struct TextLink { node_id: Id, bounds: Rect, url: String }

/// Outer outline of a table unit in EMU: the table width and the stroke painted on its cell edges.
#[derive(Clone, Copy)]
struct TableOutline { width: i64, stroke: i64 }

/// A table row taller than the page, kept whole until it is placed: where it starts decides how much of it fits the first fragment.
struct RowSplit { table_id: String, style: Style, cells: Vec<CellLayout>, height: i64, outline: TableOutline, inset: i64 }

impl RowSplit {
    fn into_unit(self) -> Unit { row_unit(&self.table_id, &self.style, self.cells, self.height, self.outline) }
}

/// What a block is for the page placement: a unit, or a tall table row that is split where it is placed.
enum Piece { Unit(Unit), Row(RowSplit) }

/// The unit of a table row that is not split: its cell borders and the cell content, in row coordinates.
fn row_unit(table_id: &str, style: &Style, cells: Vec<CellLayout>, height: i64, outline: TableOutline) -> Unit {
    let mut unit = Unit { height, blocks: vec![table_id.to_string()], outline: Some(outline), ..Default::default() };
    for mut cell in cells {
        unit.items.push(Primitive::Shape { node_id: cell.id, bounds: cell.bounds, shape: "rect".into(), style: style.clone() });
        unit.items.append(&mut cell.items);
        unit.regions.append(&mut cell.regions);
        unit.text_links.append(&mut cell.text_links);
    }
    unit
}

/// `text_lines` holds the line top of each text item in `items`, in order (see `split_table_row`).
struct Column { height: i64, items: Vec<Primitive>, regions: Vec<SourceRegion>, text_lines: Vec<i64>, text_links: Vec<TextLink> }

/// `text_lines` holds the line top of each text item in `items`, in order, in row coordinates.
struct CellLayout { id: Id, bounds: Rect, content_end: i64, items: Vec<Primitive>, regions: Vec<SourceRegion>, text_lines: Vec<i64>, text_links: Vec<TextLink> }

#[derive(Default)]
struct Extents { order: Vec<(usize, Id)>, rects: HashMap<(usize, Id), Rect> }
impl Extents {
    fn add(&mut self, page: usize, id: &str, rect: Rect) {
        let key = (page, id.to_string());
        match self.rects.get_mut(&key) {
            Some(r) => *r = rect_union(*r, rect),
            None => { self.order.push(key.clone()); self.rects.insert(key, rect); }
        }
    }
    fn take(self) -> Vec<(usize, SourceRegion)> {
        let Extents { order, rects } = self;
        order.into_iter().map(|key| {
            let bounds = rects[&key];
            (key.0, SourceRegion { node_id: key.1, bounds, logical_range: None, grapheme_rects: vec![] })
        }).collect()
    }
}

fn rect_union(a: Rect, b: Rect) -> Rect {
    let x0 = a.x.min(b.x);
    let y0 = a.y.min(b.y);
    let x1 = (a.x + a.width).max(b.x + b.width);
    let y1 = (a.y + a.height).max(b.y + b.height);
    Rect { x: x0, y: y0, width: x1 - x0, height: y1 - y0 }
}

// shortcut: non-text content and row-spanned groups stay atomic, when those blocks gain continuation layouts.
/// Line boxes of a cell's content in row coordinates, sorted and without repeats. The lines decide the cuts, not the glyph boxes:
/// with an exact line spacing below the glyph height the glyph boxes overlap their neighbours.
fn cell_lines(cell: &CellLayout) -> Vec<(i64, i64)> {
    let mut lines: Vec<(i64, i64)> = cell.regions.iter().filter(|r| r.logical_range.is_some()).map(|r| (r.bounds.y, r.bounds.y + r.bounds.height)).collect();
    lines.sort_unstable();
    lines.dedup();
    lines
}

/// The spans of one cell's content that successive fragments of a row hold. Each span ends after a whole line: the first span has
/// room `first`, the others `room`. The last span also takes the trailing space of the content when that fits, so no span holds
/// only space. None when a line fits no span.
fn cell_ranges(lines: &[(i64, i64)], content_end: i64, first: i64, room: i64) -> Option<Vec<(i64, i64)>> {
    let mut ranges = Vec::new();
    let (mut start, mut next, mut span) = (CELL_PADDING_Y, 0, first);
    while next < lines.len() {
        let limit = start.saturating_add(span);
        let fit = next + lines[next..].partition_point(|&(_, bottom)| bottom <= limit);
        if fit == next {
            return None;
        }
        let end = if fit == lines.len() && content_end <= limit { content_end } else { lines[fit - 1].1 };
        ranges.push((start, end));
        (start, next, span) = (end, fit, room);
    }
    Some(ranges)
}

/// The spans of each cell for the fragments of a row of `height`: the first fragment has room `first_room` of the page, the others
/// `room`. None when the row is not text-only, a cell has a line outside its content, a line fits no span, or no cell has a line.
/// A row whose lines all fit one fragment still gets one span per cell: its end space stops at that fragment's end and is not carried
/// to the next page.
fn table_row_breaks(cells: &[CellLayout], height: i64, inset: i64, first_room: i64, room: i64) -> Option<Vec<Vec<(i64, i64)>>> {
    let first = first_room.saturating_sub(inset.saturating_mul(2));
    let span = room.saturating_sub(inset.saturating_mul(2));
    if span <= 0 || height <= room || cells.iter().flat_map(|c| &c.items).any(|p| !matches!(p, Primitive::Text { .. })) {
        return None;
    }
    let mut parts = Vec::with_capacity(cells.len());
    let mut has_lines = false;
    for cell in cells {
        let lines = cell_lines(cell);
        has_lines |= !lines.is_empty();
        if lines.iter().any(|&(top, bottom)| top < CELL_PADDING_Y || bottom > cell.content_end) {
            return None;
        }
        parts.push(cell_ranges(&lines, cell.content_end, first, span)?);
    }
    has_lines.then_some(parts)
}

/// Stroke width the renderer paints for a table border (render paint::parse_color): 0 when the color is empty, `none`
/// or `transparent`, otherwise the stroke width.
fn painted_stroke(style: &Style) -> i64 {
    let color = style.stroke.trim();
    if color.is_empty() || color.eq_ignore_ascii_case("none") || color.eq_ignore_ascii_case("transparent") { 0 } else { style.stroke_width.max(0) }
}

/// The span of a row fragment that holds `y`: the last span starting at or before `y`, and the first span above all of them.
fn span_of(ranges: &[(i64, i64)], y: i64) -> usize {
    ranges.partition_point(|&(start, _)| start <= y).saturating_sub(1)
}

/// Splits a row into one unit per fragment, each with whether a glyph box of its text reaches past the fragment's cell. A text item
/// goes to the span of its line by the line top kept with it: a glyph box can lie outside its line box (a larger run on the same
/// line, or an exact spacing below the glyph height), so its middle does not show the line. A line region goes to the span that
/// holds it. The cell borders are drawn on every fragment.
fn split_table_row(row: RowSplit, parts: &[Vec<(i64, i64)>]) -> Vec<(Unit, bool)> {
    let RowSplit { table_id, style, cells, outline, inset, .. } = row;
    let count = parts.iter().map(Vec::len).max().unwrap_or(0);
    let heights: Vec<i64> = (0..count)
        .map(|page| 2 * inset + parts.iter().filter_map(|ranges| ranges.get(page)).map(|(start, end)| end - start).max().unwrap_or(0))
        .collect();
    let mut units: Vec<Unit> = heights.iter().map(|&height| Unit { height, blocks: vec![table_id.clone()], outline: Some(outline), ..Default::default() }).collect();
    let mut clipped = vec![false; count];
    for (cell, ranges) in cells.into_iter().zip(parts) {
        let mut item_parts: Vec<Vec<Primitive>> = (0..ranges.len()).map(|_| Vec::new()).collect();
        // Every item of a split row is text (`table_row_breaks`), so each one pairs with its line top.
        for (mut item, line_top) in cell.items.into_iter().zip(cell.text_lines) {
            if ranges.is_empty() {
                continue;
            }
            let page = span_of(ranges, line_top);
            translate_all(std::slice::from_mut(&mut item), &mut [], &mut [], 0, inset - ranges[page].0);
            if let Primitive::Text { bounds, .. } = &item {
                if bounds.y < 0 || bounds.y.saturating_add(bounds.height) > heights[page] {
                    clipped[page] = true;
                }
            }
            item_parts[page].push(item);
        }

        let mut region_parts: Vec<Vec<SourceRegion>> = (0..ranges.len()).map(|_| Vec::new()).collect();
        for region in cell.regions {
            if region.logical_range.is_some() {
                if ranges.is_empty() {
                    continue;
                }
                let page = span_of(ranges, region.bounds.y);
                let mut region = region;
                translate_all(&mut [], std::slice::from_mut(&mut region), &mut [], 0, inset - ranges[page].0);
                region_parts[page].push(region);
            } else {
                if region.node_id == cell.id {
                    continue;
                }
                for (page, (start, end)) in ranges.iter().copied().enumerate() {
                    let top = region.bounds.y.max(start);
                    let bottom = (region.bounds.y + region.bounds.height).min(end);
                    if bottom <= top {
                        continue;
                    }
                    let grapheme_rects = region.grapheme_rects.iter().filter_map(|rect| {
                        let grapheme_top = rect.y.max(start);
                        let grapheme_bottom = (rect.y + rect.height).min(end);
                        (grapheme_bottom > grapheme_top).then_some(Rect { y: inset + grapheme_top - start, height: grapheme_bottom - grapheme_top, ..*rect })
                    }).collect();
                    region_parts[page].push(SourceRegion {
                        node_id: region.node_id.clone(),
                        bounds: Rect { y: inset + top - start, height: bottom - top, ..region.bounds },
                        logical_range: region.logical_range.clone(),
                        grapheme_rects,
                    });
                }
            }
        }

        // A link rectangle goes to the span that holds its line, as a line region does.
        let mut link_parts: Vec<Vec<TextLink>> = (0..ranges.len()).map(|_| Vec::new()).collect();
        for mut link in cell.text_links {
            if ranges.is_empty() {
                continue;
            }
            let page = span_of(ranges, link.bounds.y);
            translate_all(&mut [], &mut [], std::slice::from_mut(&mut link), 0, inset - ranges[page].0);
            link_parts[page].push(link);
        }

        for (page, unit) in units.iter_mut().enumerate() {
            let bounds = Rect { x: cell.bounds.x, y: 0, width: cell.bounds.width, height: unit.height };
            unit.items.push(Primitive::Shape { node_id: cell.id.clone(), bounds, shape: "rect".into(), style: style.clone() });
            unit.regions.push(SourceRegion { node_id: cell.id.clone(), bounds, logical_range: None, grapheme_rects: vec![] });
            if page < ranges.len() {
                unit.items.append(&mut item_parts[page]);
                unit.regions.append(&mut region_parts[page]);
                unit.text_links.append(&mut link_parts[page]);
            }
        }
    }
    units.into_iter().zip(clipped).collect()
}

/// Splits a row whose first fragment has `first_room` of the page and whose others have `room`. A row that cannot be split is given
/// back unchanged.
fn split_row(row: RowSplit, first_room: i64, room: i64) -> Result<Vec<(Unit, bool)>, RowSplit> {
    match table_row_breaks(&row.cells, row.height, row.inset, first_room, room) {
        Some(parts) => Ok(split_table_row(row, &parts)),
        None => Err(row),
    }
}

struct Cursor { y: i64, pending_gap: i64, has_content: bool, after_break: bool }

struct Layouter<'r> {
    renderer: &'r dyn RenderProvider,
    pages: Vec<RenderPage>,
    diagnostics: Vec<Diagnostic>,
    seen: BTreeSet<(String, Option<Id>, String)>,
    sections: Vec<Value>,
    depth: usize,
    /// `RenderScene.links`: one region per placed fragment of a linked node.
    links: Vec<SceneLink>,
    /// The `OpenUrl` target of each flow image node that has one, by node id.
    link_targets: BTreeMap<Id, String>,
    /// The accepted hyperlink spans of each paragraph, by paragraph id (see `layout_flow_with_links`).
    text_links: BTreeMap<Id, Vec<TextLinkSpan>>,
    folding: Folding<'r>,
}

struct RunShape { display: String, placeholder: bool, shaped: ShapedText, bounds: Vec<usize> }

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind { Text, Space, Tab, Break, Atom }

#[derive(Clone, Debug)]
struct PieceX { run: usize, bytes: Range<usize>, x0: i64, x1: i64 }

#[derive(Clone, Debug)]
struct Cluster { index: u32, kind: Kind, width: i64, pieces: Vec<PieceX>, run: usize, text: String }

struct Line { start: usize, end: usize, forced: bool }

impl<'r> Layouter<'r> {
    fn diag(&mut self, code: &str, message: impl Into<String>, node_id: Option<Id>, blocking: bool) {
        let message = message.into();
        if self.seen.insert((code.to_string(), node_id.clone(), message.clone())) {
            self.diagnostics.push(diag(code, message, node_id, blocking));
        }
    }

    /// An overflow diagnostic with its measures (P1 `Diagnostic.params`): `text` (the content's extent)
    /// and `box` (the room it had) in EMU along `axis` (width | height), and `node`; the numbers are in
    /// the message too. Deduplicated on the plain message as before: one per code and node.
    #[allow(clippy::too_many_arguments)]
    fn overflow(&mut self, code: &str, what: &str, node_id: Option<Id>, blocking: bool, axis: &str, text: i64, room: i64) {
        if !self.seen.insert((code.to_string(), node_id.clone(), what.to_string())) { return; }
        let mut d = diag(code, format!("{what} ({axis} {text} EMU > {room} EMU)"), node_id.clone(), blocking).with_param("text", text).with_param("box", room).with_param("axis", axis);
        if let Some(node) = node_id { d = d.with_param("node", node); }
        self.diagnostics.push(d);
    }

    fn new_page(&mut self, section_id: &str, size: Size) {
        let index = self.pages.len() as u32;
        let local = self.pages.iter().filter(|p| p.id.starts_with(&format!("{section_id}/page/"))).count();
        self.pages.push(RenderPage { id: format!("{section_id}/page/{local}"), page_index: index, size, primitives: vec![], source_map: vec![] });
    }

    fn finish(self) -> RenderScene {
        let font_fingerprint = scene_font_fingerprint(&self.pages);
        let mut metadata = BTreeMap::new();
        metadata.insert("layout_engine".to_string(), json!(LAYOUT_ENGINE));
        metadata.insert("layout_version".to_string(), json!(LAYOUT_VERSION));
        metadata.insert("layout_origin".to_string(), json!("fresh"));
        metadata.insert("total_pages".to_string(), json!(self.pages.len()));
        metadata.insert("sections".to_string(), Value::Array(self.sections));
        RenderScene { pages: self.pages, font_fingerprint, diagnostics: self.diagnostics, metadata, links: self.links }
    }

    fn shape(&mut self, text: &str, style: &Style, node: &str) -> DltResult<ShapedText> {
        let shaped = self.renderer.shape_text(text, style)?;
        let bounds = grapheme_bounds(text);
        if shaped.caret_x.len() != bounds.len() {
            return Err(DltError::new(ErrorCode::Internal, format!("Renderer returned {} caret stops for {} graphemes", shaped.caret_x.len(), bounds.len() - 1)));
        }
        if !text.trim().is_empty() && !shaped.fonts_used.is_empty() {
            // Any family name of a drawn face counts (localized names such as 맑은 고딕 too).
            if !shaped.fonts_used.iter().any(|f| designllm_render::face_matches_family(f, &style.font_family)) {
                let used: BTreeSet<&str> = shaped.fonts_used.iter().map(|f| f.family.as_str()).collect();
                let used = used.into_iter().collect::<Vec<_>>().join(", ");
                self.diag(diag_codes::FONT_SUBSTITUTED, format!("Font '{}' is not available; rendered with {used}", style.font_family), None, false);
            }
        }
        if !shaped.missing_glyphs.is_empty() {
            self.diag("missing_glyphs", format!("No available font has glyphs for {:?}", shaped.missing_glyphs), Some(node.to_string()), true);
        }
        Ok(shaped)
    }

    fn section(&mut self, section: &Section) -> DltResult<()> {
        let setup = match page_setup_problem(&section.setup) {
            Some(problem) => {
                self.diag("invalid_page_setup", format!("{problem}; the default A4 setup is used for display"), Some(section.id.clone()), true);
                PageSetup::default()
            }
            None => section.setup.clone(),
        };
        let content = Rect { x: setup.margin_left, y: setup.margin_top, width: setup.size.width - setup.margin_left - setup.margin_right, height: setup.size.height - setup.margin_top - setup.margin_bottom };
        let first_page = self.pages.len();
        self.new_page(&section.id, setup.size);
        let mut cursor = Cursor { y: content.y, pending_gap: 0, has_content: false, after_break: false };
        let mut extents = Extents::default();
        for (block_index, block) in section.blocks.iter().enumerate() {
            let starts_page = matches!(block, FlowBlock::Paragraph(p) if p.format.page_break_before);
            let carrier = is_page_break_carrier(&section.blocks, block_index)
                && match (self.folding, block) {
                    (Folding::Listed(ids), FlowBlock::Paragraph(p)) => ids.contains(p.id.as_str()),
                    _ => false,
                };
            for (index, piece) in self.block_pieces(block, content.width, Some(content.height))?.into_iter().enumerate() {
                if index == 0 && starts_page { self.break_page(&mut cursor, content, &section.id, setup.size); }
                match piece {
                    Piece::Unit(unit) if carrier => self.place_carrier(unit, &mut cursor, content, &section.id, setup.size, &mut extents),
                    Piece::Unit(unit) => self.place(unit, &mut cursor, content, &section.id, setup.size, &mut extents),
                    Piece::Row(row) => self.place_row(row, &mut cursor, content, &section.id, setup.size, &mut extents),
                }
            }
        }
        for (page, region) in extents.take() { self.pages[page].source_map.push(region); }
        self.sections.push(json!({"id": section.id, "first_page": first_page, "page_count": self.pages.len() - first_page}));
        Ok(())
    }

    /// Starts a new page before a paragraph with `page_break_before`, unless nothing is placed on the current page yet.
    /// The cursor is then at the top of the page with no leading space, as after an automatic move.
    fn break_page(&mut self, cur: &mut Cursor, content: Rect, section_id: &str, size: Size) {
        if cur.has_content {
            self.new_page(section_id, size);
            *cur = Cursor { y: content.y, pending_gap: 0, has_content: false, after_break: true };
        }
    }

    /// Places the line of a folded page-break carrier (`LayoutOptions::page_break_carriers`) with no height and no space
    /// before, so the object after it is at the top of the page the carrier starts, also at the top of a section, where
    /// no break runs. The page still counts as empty: an object too tall for it does not leave the carrier alone on a
    /// page of its own.
    fn place_carrier(&mut self, mut unit: Unit, cur: &mut Cursor, content: Rect, section_id: &str, size: Size, extents: &mut Extents) {
        unit.height = 0;
        unit.gap_before = 0;
        for region in &mut unit.regions { region.bounds.height = 0; }
        let has_content = cur.has_content;
        self.place(unit, cur, content, section_id, size, extents);
        cur.has_content = has_content;
    }

    fn place(&mut self, mut unit: Unit, cur: &mut Cursor, content: Rect, section_id: &str, size: Size, extents: &mut Extents) {
        let gap = if cur.has_content { cur.pending_gap + unit.gap_before } else if cur.after_break { 0 } else { unit.gap_before };
        let mut top = cur.y + gap;
        if top + unit.height > content.y + content.height {
            // The first unit of a section already starts a fresh page: a leading space that does not fit
            // is dropped there instead of placing the unit off the page without a diagnostic.
            if cur.has_content {
                self.new_page(section_id, size);
                cur.after_break = true;
            } else if gap > 0 {
                // No page is started here, so this space is lost on the section's first page. Report it as a
                // blocking loss on its paragraph, with the space and the room left for it (overflow measures).
                let node = unit.blocks.first().cloned();
                let what = format!("Space before paragraph {} is dropped at the section top; it does not fit the page", node.as_deref().unwrap_or("?"));
                let room = (content.height - unit.height).max(0);
                self.overflow("leading_space_overflow", &what, node, true, "height", gap, room);
            }
            top = content.y;
        }
        if unit.height > content.height {
            let node = unit.blocks.first().cloned().or_else(|| unit.regions.first().map(|r| r.node_id.clone()));
            self.overflow("content_overflow", "Content taller than the page content area is cut at the page edge", node, true, "height", unit.height, content.height);
        }
        if let Some(outline) = unit.outline {
            let node = unit.blocks.first().cloned();
            let page = self.pages.len() - 1;
            self.table_border_check(outline, top, unit.height, content, size, node, page);
        }
        translate_all(&mut unit.items, &mut unit.regions, &mut unit.text_links, content.x, top);
        let page_index = self.pages.len() - 1;
        self.record_links(&unit.regions, page_index);
        let page_id = self.pages[page_index].id.clone();
        self.links.extend(unit.text_links.drain(..).map(|link| SceneLink { page_id: page_id.clone(), node_id: link.node_id, bounds: link.bounds, url: link.url }));
        let page = &mut self.pages[page_index];
        page.primitives.append(&mut unit.items);
        page.source_map.append(&mut unit.regions);
        for id in &unit.blocks { extents.add(page_index, id, Rect { x: content.x, y: top, width: content.width, height: unit.height }); }
        cur.y = top + unit.height;
        cur.pending_gap = unit.gap_after;
        cur.has_content = true;
    }

    /// Adds a link region for each region of a linked node in `regions`, which is on the page at `page_index`. Each placed unit adds
    /// its own regions, so a node split over pages gets one region per page.
    fn record_links(&mut self, regions: &[SourceRegion], page_index: usize) {
        if self.link_targets.is_empty() { return; }
        let page_id = &self.pages[page_index].id;
        for region in regions {
            if let Some(url) = self.link_targets.get(&region.node_id) {
                self.links.push(SceneLink { page_id: page_id.clone(), node_id: region.node_id.clone(), bounds: region.bounds, url: url.clone() });
            }
        }
    }

    /// Places a row taller than the page. Its first fragment fills the room left on the current page, as the lines of a paragraph
    /// do; when no line fits there, the row starts on the next page instead. Each fragment is placed like any unit, so a page break
    /// falls between fragments only where the room runs out. A row that cannot be split is placed whole.
    fn place_row(&mut self, row: RowSplit, cur: &mut Cursor, content: Rect, section_id: &str, size: Size, extents: &mut Extents) {
        let gap = if cur.has_content { cur.pending_gap } else { 0 };
        let room = (content.y + content.height).saturating_sub(cur.y + gap);
        let table_id = row.table_id.clone();
        let split = match split_row(row, room, content.height) {
            Err(row) if cur.has_content => split_row(row, content.height, content.height),
            split => split,
        };
        match split {
            Ok(fragments) => {
                for (unit, clipped) in fragments {
                    self.place(unit, cur, content, section_id, size, extents);
                    if clipped {
                        let page = self.pages.len() - 1;
                        let message = format!("Text in a split table row on page {} extends past its cell: the line spacing is smaller than the text height", page + 1);
                        self.diag("table_text_overflow", message, Some(table_id.clone()), true);
                    }
                }
            }
            Err(row) => self.place(row.into_unit(), cur, content, section_id, size, extents),
        }
    }

    /// Blocking `table_border_overflow` for each page edge that cuts half of a table's stroke. The stroke is painted on
    /// both sides of a cell edge, so its outer half lies outside the page where the edge touches the page edge. Nested
    /// tables are laid out inside their cells and are not checked here.
    fn table_border_check(&mut self, outline: TableOutline, top: i64, height: i64, content: Rect, size: Size, node: Option<Id>, page: usize) {
        let half = outline.stroke.saturating_add(1) / 2;
        let rooms = [
            ("top", "height", top),
            ("bottom", "height", size.height.saturating_sub(top.saturating_add(height))),
            ("left", "width", content.x),
            ("right", "width", size.width.saturating_sub(content.x.saturating_add(outline.width))),
        ];
        for (side, axis, room) in rooms {
            if half > room {
                let what = format!("Table border on the {side} page edge is cut on page {}: half of its stroke lies outside the page", page + 1);
                self.overflow("table_border_overflow", &what, node.clone(), true, axis, half, room);
            }
        }
    }

    /// The pieces of a block as the page placement takes them: a table keeps its tall rows whole until they are placed.
    fn block_pieces(&mut self, block: &FlowBlock, width: i64, max_height: Option<i64>) -> DltResult<Vec<Piece>> {
        match block {
            FlowBlock::Table(t) => self.table_pieces(t, width, max_height),
            _ => Ok(self.block_units(block, width, max_height)?.into_iter().map(Piece::Unit).collect()),
        }
    }

    fn block_units(&mut self, block: &FlowBlock, width: i64, max_height: Option<i64>) -> DltResult<Vec<Unit>> {
        match block {
            FlowBlock::Paragraph(p) => self.paragraph_units(p, width),
            FlowBlock::Table(t) => self.table_units(t, width, max_height),
            FlowBlock::Image(node) => Ok(vec![self.image_unit(node, width, max_height)]),
            FlowBlock::Opaque { id, label } => Ok(vec![self.opaque_unit(id, label, width)?]),
        }
    }

    fn column(&mut self, blocks: &[FlowBlock], width: i64, max_height: Option<i64>) -> DltResult<Column> {
        let mut units: Vec<(Unit, Option<&Node>)> = Vec::new();
        for block in blocks {
            let (block_units, image) = match block {
                FlowBlock::Image(node) => (vec![self.image_unit(node, width, None)], Some(node)),
                _ => (self.block_units(block, width, max_height)?, None),
            };
            units.extend(block_units.into_iter().map(|unit| (unit, image)));
        }

        if let Some(max_height) = max_height {
            let (mut y, mut pending, mut image_height, mut image_count) = (0i64, 0i64, 0i64, 0i64);
            let mut oversized_image = false;
            let mut first = true;
            for (unit, image) in &units {
                let top = y + if first { unit.gap_before } else { pending + unit.gap_before };
                y = top + unit.height;
                pending = unit.gap_after;
                if image.is_some() {
                    image_height += unit.height;
                    image_count += 1;
                    oversized_image |= unit.height > max_height;
                }
                first = false;
            }
            let total_height = y + pending;
            let fixed_height = total_height.saturating_sub(image_height).max(0);
            let image_room = max_height.saturating_sub(fixed_height).min(max_height);
            if (total_height > max_height || oversized_image) && image_count > 0 && image_room >= image_count && image_height > image_room {
                let extra_room = image_room - image_count;
                for (unit, image) in &mut units {
                    let Some(node) = *image else { continue };
                    let target = 1 + (i128::from(unit.height) * i128::from(extra_room) / i128::from(image_height)) as i64;
                    *unit = self.image_unit(node, width, Some(target));
                }
            }
        }

        let mut y = 0;
        let mut pending = 0;
        let mut first = true;
        let mut items = Vec::new();
        let mut regions = Vec::new();
        let mut text_lines = Vec::new();
        let mut text_links = Vec::new();
        let mut extents = Extents::default();
        for (mut unit, _) in units {
            let top = y + if first { unit.gap_before } else { pending + unit.gap_before };
            // A paragraph line is one unit, so its text items lie on the line at `top`; other units keep their text in their box.
            text_lines.extend(unit.items.iter().filter(|p| matches!(p, Primitive::Text { .. })).map(|_| top));
            translate_all(&mut unit.items, &mut unit.regions, &mut unit.text_links, 0, top);
            items.append(&mut unit.items);
            regions.append(&mut unit.regions);
            text_links.append(&mut unit.text_links);
            for id in &unit.blocks { extents.add(0, id, Rect { x: 0, y: top, width, height: unit.height }); }
            y = top + unit.height;
            pending = unit.gap_after;
            first = false;
        }
        regions.extend(extents.take().into_iter().map(|(_, r)| r));
        Ok(Column { height: y + pending, items, regions, text_lines, text_links })
    }

    // ------------------------------------------------------------------ paragraphs

    fn paragraph_units(&mut self, p: &Paragraph, width: i64) -> DltResult<Vec<Unit>> {
        let lp = LogicalParagraph::new(p);
        let mut runs: Vec<RunShape> = Vec::with_capacity(p.runs.len());
        for run in &p.runs {
            let placeholder = run.protected && (run.text.is_empty() || run.text == "\u{FFFC}");
            let display = if placeholder { String::new() } else { run.text.clone() };
            let shaped = self.shape(&display, &run.style, &run.id)?;
            let bounds = grapheme_bounds(&display);
            if placeholder {
                self.diag("inline_object_placeholder", "An inline object is shown as a placeholder box", Some(run.id.clone()), true);
            }
            runs.push(RunShape { display, placeholder, shaped, bounds });
        }
        if lp.text.chars().any(is_rtl) {
            self.diag("bidi_unsupported", "Right-to-left text is laid out left-to-right", Some(p.id.clone()), true);
        }
        let default_style = Style::default();
        let empty_shape = if p.runs.is_empty() { Some(self.shape("", &default_style, &p.id)?) } else { None };
        let clusters = build_clusters(&lp, p, &runs);
        let lines = self.break_lines(&clusters, &p.format, width, &p.id);
        let mut units = Vec::with_capacity(lines.len());
        let line_count = lines.len();
        for (line_no, line) in lines.iter().enumerate() {
            let mut unit = self.line_unit(p, &lp, &runs, &clusters, line, line_no, line_count, width, empty_shape.as_ref());
            if line_no == 0 { unit.gap_before = p.format.space_before; }
            if line_no + 1 == line_count { unit.gap_after = p.format.space_after; }
            unit.blocks.push(p.id.clone());
            units.push(unit);
        }
        Ok(units)
    }

    fn break_lines(&mut self, clusters: &[Cluster], format: &ParagraphFormat, width: i64, pid: &str) -> Vec<Line> {
        let mut lines = Vec::new();
        let n = clusters.len();
        let mut i = 0;
        while i < n {
            let left = line_left(format, lines.len());
            let avail = (width - left).max(1);
            let start = i;
            let mut x = 0i64;
            let mut last_op: Option<usize> = None;
            let mut end = n;
            let mut forced = false;
            let mut j = i;
            while j < n {
                let c = &clusters[j];
                if c.kind == Kind::Break { end = j + 1; forced = true; break; }
                let w = cluster_advance(c, left + x);
                if c.kind == Kind::Space { x += w; j += 1; if j < n && break_between(&c.text, &clusters[j].text) { last_op = Some(j); } continue; }
                if x + w > avail && j > start {
                    end = match last_op { Some(op) if op > start => op, _ => j };
                    break;
                }
                x += w;
                j += 1;
                if j < n && break_between(&c.text, &clusters[j].text) { last_op = Some(j); }
            }
            if j >= n && !forced { end = n; }
            let content: i64 = line_width(clusters, start, end, left);
            if content > avail {
                self.overflow("text_overflow", "A word is wider than the line and extends past the margin", Some(pid.to_string()), false, "width", content, avail);
            }
            lines.push(Line { start, end, forced });
            i = end;
        }
        if lines.is_empty() || lines.last().map(|l| l.forced).unwrap_or(false) {
            lines.push(Line { start: n, end: n, forced: false });
        }
        lines
    }

    #[allow(clippy::too_many_arguments)]
    fn line_unit(&mut self, p: &Paragraph, lp: &LogicalParagraph, runs: &[RunShape], clusters: &[Cluster], line: &Line, line_no: usize, line_count: usize, width: i64, empty_shape: Option<&ShapedText>) -> Unit {
        let left = line_left(&p.format, line_no);
        let avail = (width - left).max(1);
        let slice = &clusters[line.start..line.end];
        // Line metrics from the runs on the line, or the insertion run for an empty line.
        let mut metric_runs: Vec<usize> = slice.iter().flat_map(|c| c.pieces.iter().map(|pc| pc.run)).collect();
        metric_runs.sort_unstable();
        metric_runs.dedup();
        let anchor_run = if metric_runs.is_empty() {
            lp.insertion_point(line.start as u32).map(|(r, _)| r).or(if p.runs.is_empty() { None } else { Some(nearest_run(lp, line.start, p.runs.len())) })
        } else { None };
        if let Some(r) = anchor_run { metric_runs.push(r); }
        let (mut asc, mut desc, mut gap) = (0i64, 0i64, 0i64);
        for &r in &metric_runs {
            let s = &runs[r].shaped;
            asc = asc.max(s.ascent);
            desc = desc.max(s.descent);
            gap = gap.max(s.line_gap);
        }
        if metric_runs.is_empty() {
            if let Some(s) = empty_shape { asc = s.ascent; desc = s.descent; gap = s.line_gap; }
        }
        let pitch = crate::line_spacing::layout_pitch(p);
        let (height, baseline) = if pitch > 0 {
            let h = pitch;
            (h, (h - (asc + desc)) / 2 + asc)
        } else {
            (asc + desc + gap, gap / 2 + asc)
        };
        // Horizontal positions.
        let mut widths: Vec<i64> = Vec::with_capacity(slice.len());
        let mut x = 0i64;
        for c in slice { let w = cluster_advance(c, left + x); widths.push(w); x += w; }
        let mut trailing = slice.len();
        while trailing > 0 && matches!(slice[trailing - 1].kind, Kind::Space | Kind::Break) { trailing -= 1; }
        let content: i64 = widths[..trailing].iter().sum();
        let last_line = line_no + 1 == line_count || line.forced;
        let mut offset = match p.format.align.as_str() {
            "center" => (avail - content) / 2,
            "right" => avail - content,
            _ => 0,
        }.max(0);
        if p.format.align == "justify" && !last_line && avail > content && trailing > 1 {
            let spaces: Vec<usize> = (0..trailing).filter(|&k| slice[k].kind == Kind::Space).collect();
            let targets: Vec<usize> = if spaces.is_empty() { (0..trailing - 1).collect() } else { spaces };
            let extra = avail - content;
            let each = extra / targets.len() as i64;
            let mut rest = extra % targets.len() as i64;
            for k in targets {
                widths[k] += each + if rest > 0 { rest -= 1; 1 } else { 0 };
            }
            offset = 0;
        }
        let mut xs = Vec::with_capacity(slice.len());
        let mut pen = left + offset;
        for w in &widths { xs.push(pen); pen += *w; }
        // Trailing spaces and the break hang into the margin; clamp them to the text area so
        // carets and regions stay inside it.
        let right = (left + avail).max(xs.get(trailing).copied().unwrap_or(0));
        for k in trailing..slice.len() {
            let start = if k == trailing { xs[k].min(right) } else { xs[k - 1] + widths[k - 1] };
            xs[k] = start;
            widths[k] = widths[k].min((right - start).max(0));
        }

        let mut unit = Unit { height, ..Default::default() };
        // Text primitives: consecutive pieces of the same run.
        let mut group: Vec<(PieceX, i64, i64)> = Vec::new();
        let mut group_run: Option<usize> = None;
        for (k, c) in slice.iter().enumerate() {
            let mut px = xs[k];
            let last_piece = c.pieces.len().saturating_sub(1);
            for (pi, piece) in c.pieces.iter().enumerate() {
                let natural = piece.x1 - piece.x0;
                let w = if c.kind == Kind::Tab || c.kind == Kind::Atom { widths[k] } else if pi == last_piece { xs[k] + widths[k] - px } else { natural };
                if group_run != Some(piece.run) || c.kind == Kind::Atom {
                    if let Some(r) = group_run.take() { self.emit_group(p, &runs[r], r, &group, baseline, &mut unit.items); }
                    group.clear();
                    group_run = Some(piece.run);
                }
                group.push((piece.clone(), px, px + w));
                px += w;
            }
            if c.kind == Kind::Atom {
                if let Some(r) = group_run.take() { self.emit_group(p, &runs[r], r, &group, baseline, &mut unit.items); }
                group.clear();
            }
        }
        if let Some(r) = group_run.take() { self.emit_group(p, &runs[r], r, &group, baseline, &mut unit.items); }

        // Source regions: consecutive clusters of the same run.
        let mut k = 0;
        while k < slice.len() {
            let run = slice[k].run;
            let mut e = k + 1;
            while e < slice.len() && slice[e].run == run { e += 1; }
            let rects: Vec<Rect> = (k..e).map(|m| Rect { x: xs[m], y: 0, width: widths[m], height }).collect();
            let x0 = xs[k];
            let x1 = xs[e - 1] + widths[e - 1];
            unit.regions.push(SourceRegion {
                node_id: p.runs[run].id.clone(),
                bounds: Rect { x: x0, y: 0, width: x1 - x0, height },
                logical_range: Some(TextRange { paragraph_id: p.id.clone(), start: slice[k].index, end: slice[e - 1].index + 1 }),
                grapheme_rects: rects,
            });
            k = e;
        }
        // Link rectangles: each span gets one rectangle on this line, from its first to its last grapheme on the line.
        if let Some(spans) = self.text_links.get(&p.id) {
            for span in spans {
                let inside: Vec<usize> = (0..slice.len()).filter(|&k| slice[k].index >= span.start && slice[k].index < span.end).collect();
                if let (Some(&first), Some(&last)) = (inside.first(), inside.last()) {
                    let width = xs[last] + widths[last] - xs[first];
                    if width > 0 && height > 0 {
                        unit.text_links.push(TextLink { node_id: p.id.clone(), bounds: Rect { x: xs[first], y: 0, width, height }, url: span.url.clone() });
                    }
                }
            }
        }
        if slice.is_empty() {
            let x = left + match p.format.align.as_str() { "center" => avail / 2, "right" => avail, _ => 0 };
            let node = anchor_run.map(|r| p.runs[r].id.clone()).unwrap_or_else(|| p.id.clone());
            unit.regions.push(SourceRegion {
                node_id: node,
                bounds: Rect { x, y: 0, width: 0, height },
                logical_range: Some(TextRange { paragraph_id: p.id.clone(), start: line.start as u32, end: line.start as u32 }),
                grapheme_rects: vec![],
            });
        }
        unit
    }

    fn emit_group(&mut self, p: &Paragraph, rs: &RunShape, run_index: usize, group: &[(PieceX, i64, i64)], baseline: i64, out: &mut Vec<Primitive>) {
        if group.is_empty() { return; }
        let run = &p.runs[run_index];
        let shaped = &rs.shaped;
        if rs.placeholder {
            let (x0, x1) = (group[0].1, group[group.len() - 1].2);
            let style = Style { fill: "none".into(), stroke: "#9AA3AF".into(), stroke_width: EMU_PER_PT / 2, ..run.style.clone() };
            out.push(Primitive::Shape { node_id: run.id.clone(), bounds: Rect { x: x0, y: baseline - shaped.ascent, width: (x1 - x0).max(1), height: (shaped.ascent + shaped.descent).max(1) }, shape: "rect".into(), style });
            return;
        }
        let b_start = group[0].0.bytes.start;
        let b_end = group[group.len() - 1].0.bytes.end;
        let text = &rs.display[b_start..b_end];
        let line_x = |byte: usize| -> i64 {
            for (piece, lx0, lx1) in group {
                if byte == piece.bytes.start { return *lx0; }
                if byte == piece.bytes.end { return *lx1; }
                if byte > piece.bytes.start && byte < piece.bytes.end {
                    return (*lx0 + (x_at(rs, byte) - piece.x0)).min(*lx1);
                }
            }
            group[group.len() - 1].2
        };
        let origin_x = group[0].1;
        let local = grapheme_bounds(text);
        let caret_x: Vec<i64> = local.iter().map(|b| line_x(b_start + b) - origin_x).collect();
        let to_local = |byte: usize| -> u32 {
            let rel = byte.saturating_sub(b_start).min(text.len());
            match local.binary_search(&rel) { Ok(k) => k as u32, Err(k) => k.saturating_sub(1) as u32 }
        };
        let mut glyphs = Vec::new();
        let mut keys = BTreeSet::new();
        for g in &shaped.glyphs {
            let Some(&gb) = rs.bounds.get(g.grapheme_start as usize) else { continue };
            if gb < b_start || gb >= b_end { continue; }
            let Some((piece, lx0, _)) = group.iter().find(|(pc, _, _)| gb >= pc.bytes.start && gb < pc.bytes.end) else { continue };
            let gx = lx0 + (g.position.x - piece.x0) - origin_x;
            let end_byte = rs.bounds.get(g.grapheme_end as usize).copied().unwrap_or(rs.display.len());
            let gs = to_local(gb);
            let ge = if end_byte >= b_end { (local.len() - 1) as u32 } else { to_local(end_byte).max(gs + 1) };
            keys.insert(g.font_key.clone());
            glyphs.push(GlyphMetric { glyph_id: g.glyph_id, font_key: g.font_key.clone(), position: Point { x: gx, y: g.position.y }, advance_x: g.advance_x, grapheme_start: gs, grapheme_end: ge });
        }
        if glyphs.is_empty() && text.chars().all(|c| c.is_control() || is_space(c)) { return; }
        let width = line_x(b_end) - origin_x;
        let grapheme_rects: Vec<Rect> = caret_x.windows(2).map(|w| Rect { x: w[0], y: -shaped.ascent, width: w[1] - w[0], height: shaped.ascent + shaped.descent }).collect();
        let fonts_used: Vec<FontFaceRef> = shaped.fonts_used.iter().filter(|f| keys.contains(&f.key)).cloned().collect();
        let missing_glyphs: Vec<String> = shaped.missing_glyphs.iter().filter(|m| text.contains(m.as_str())).cloned().collect();
        let sub = ShapedText {
            size: Size { width, height: shaped.size.height },
            baseline: shaped.baseline, ascent: shaped.ascent, descent: shaped.descent, line_gap: shaped.line_gap, cap_height: shaped.cap_height,
            glyphs, caret_x, grapheme_rects, fonts_used, missing_glyphs,
        };
        out.push(Primitive::Text {
            node_id: run.id.clone(),
            bounds: Rect { x: origin_x, y: baseline - shaped.ascent, width, height: shaped.ascent + shaped.descent },
            origin: Point { x: origin_x, y: baseline },
            text: text.to_string(),
            style: run.style.clone(),
            shaped: sub,
        });
    }

    // ------------------------------------------------------------------ tables

    fn table_pieces(&mut self, t: &Table, width: i64, max_height: Option<i64>) -> DltResult<Vec<Piece>> {
        if self.depth >= MAX_NESTING { return Err(DltError::unsupported(format!("Tables nested deeper than {MAX_NESTING} levels are not supported"))); }
        let nrows = t.rows.len();
        if nrows == 0 {
            self.diag("empty_table", "A table without rows is not shown", Some(t.id.clone()), false);
            return Ok(vec![]);
        }
        let mut cols: Vec<i64> = t.column_widths.iter().map(|w| (*w).max(1)).collect();
        let mut problem = t.column_widths.iter().any(|w| *w <= 0);
        if cols.is_empty() {
            let n = t.rows.iter().map(|r| r.iter().map(|c| c.column_span.max(1) as usize).sum::<usize>()).max().unwrap_or(1).max(1);
            cols = vec![(width / n as i64).max(1); n];
            problem = true;
        }
        let mut occupied = vec![vec![false; cols.len()]; nrows];
        struct Placed<'t> { row: usize, col: usize, rs: usize, cs: usize, cell: &'t TableCell }
        let mut placed: Vec<Placed> = Vec::new();
        for (r, row) in t.rows.iter().enumerate() {
            let mut c = 0usize;
            for cell in row {
                while c < cols.len() && occupied[r][c] { c += 1; }
                let cs = cell.column_span.max(1) as usize;
                let want_rs = cell.row_span.max(1) as usize;
                let rs = want_rs.min(nrows - r);
                if cell.column_span == 0 || cell.row_span == 0 || want_rs != rs { problem = true; }
                while c + cs > cols.len() {
                    problem = true;
                    let w = *cols.last().unwrap_or(&1);
                    cols.push(w);
                    for o in occupied.iter_mut() { o.push(false); }
                }
                for rr in r..r + rs { for cc in c..c + cs { occupied[rr][cc] = true; } }
                placed.push(Placed { row: r, col: c, rs, cs, cell });
                c += cs;
            }
        }
        if problem {
            self.diag("table_grid_mismatch", "Table cells do not match the column grid; extra grid columns were added for display", Some(t.id.clone()), false);
        }
        let mut col_x = vec![0i64; cols.len() + 1];
        for (i, w) in cols.iter().enumerate() { col_x[i + 1] = col_x[i] + w; }
        let outline = TableOutline { width: col_x[cols.len()], stroke: painted_stroke(&t.style) };
        if col_x[cols.len()] > width {
            self.overflow("table_overflow", "The table is wider than the text area", Some(t.id.clone()), false, "width", col_x[cols.len()], width);
        }
        self.depth += 1;
        let mut contents = Vec::with_capacity(placed.len());
        for pc in &placed {
            let inner = (col_x[pc.col + pc.cs] - col_x[pc.col] - 2 * CELL_PADDING_X).max(1);
            let cell_max_height = max_height.map(|height| height.saturating_sub(2 * CELL_PADDING_Y).max(1));
            let column = self.column(&pc.cell.blocks, inner, cell_max_height);
            match column { Ok(c) => contents.push(c), Err(e) => { self.depth -= 1; return Err(e); } }
        }
        self.depth -= 1;
        let mut row_h = vec![2 * CELL_PADDING_Y + 1; nrows];
        for (pc, content) in placed.iter().zip(&contents) {
            if pc.rs == 1 { row_h[pc.row] = row_h[pc.row].max(content.height + 2 * CELL_PADDING_Y); }
        }
        for (pc, content) in placed.iter().zip(&contents) {
            if pc.rs > 1 {
                let need = content.height + 2 * CELL_PADDING_Y;
                let have: i64 = row_h[pc.row..pc.row + pc.rs].iter().sum();
                if need > have { row_h[pc.row + pc.rs - 1] += need - have; }
            }
        }
        let mut row_y = vec![0i64; nrows + 1];
        for r in 0..nrows { row_y[r + 1] = row_y[r] + row_h[r]; }
        let mut pieces = Vec::new();
        let mut r = 0;
        let mut contents: Vec<Option<Column>> = contents.into_iter().map(Some).collect();
        while r < nrows {
            let mut end = r + 1;
            loop {
                let reach = placed.iter().filter(|pc| pc.row >= r && pc.row < end).map(|pc| pc.row + pc.rs).max().unwrap_or(end);
                if reach > end { end = reach; } else { break; }
            }
            let y0 = row_y[r];
            let height = row_y[end] - y0;
            let mut cells = Vec::new();
            for (k, pc) in placed.iter().enumerate() {
                if pc.row < r || pc.row >= end { continue; }
                let rect = Rect { x: col_x[pc.col], y: row_y[pc.row] - y0, width: col_x[pc.col + pc.cs] - col_x[pc.col], height: row_y[pc.row + pc.rs] - row_y[pc.row] };
                let mut cell = CellLayout {
                    id: pc.cell.id.clone(), bounds: rect, content_end: rect.y + CELL_PADDING_Y, items: vec![],
                    regions: vec![SourceRegion { node_id: pc.cell.id.clone(), bounds: rect, logical_range: None, grapheme_rects: vec![] }],
                    text_lines: vec![], text_links: vec![],
                };
                if let Some(mut content) = contents[k].take() {
                    cell.content_end += content.height;
                    translate_all(&mut content.items, &mut content.regions, &mut content.text_links, rect.x + CELL_PADDING_X, rect.y + CELL_PADDING_Y);
                    cell.text_lines = content.text_lines.iter().map(|top| top + rect.y + CELL_PADDING_Y).collect();
                    cell.items.append(&mut content.items);
                    cell.regions.append(&mut content.regions);
                    cell.text_links.append(&mut content.text_links);
                }
                cells.push(cell);
            }
            // A row taller than the page is split where it is placed (`place_row`); any other row is one unit.
            if end == r + 1 {
                if let Some(max_height) = max_height {
                    if height > max_height {
                        let inset = CELL_PADDING_Y.saturating_add(painted_stroke(&t.style).saturating_add(1) / 2);
                        pieces.push(Piece::Row(RowSplit { table_id: t.id.clone(), style: t.style.clone(), cells, height, outline, inset }));
                        r = end;
                        continue;
                    }
                }
            }
            pieces.push(Piece::Unit(row_unit(&t.id, &t.style, cells, height, outline)));
            r = end;
        }
        Ok(pieces)
    }

    /// The units of a table in a context without pages (the content of a cell). Its rows are not split: a nested table is not a
    /// continuation target, so a row taller than the room stays one unit, and the page reports it as an overflow.
    fn table_units(&mut self, t: &Table, width: i64, max_height: Option<i64>) -> DltResult<Vec<Unit>> {
        Ok(self.table_pieces(t, width, max_height)?.into_iter().map(|piece| match piece {
            Piece::Unit(unit) => unit,
            Piece::Row(row) => row.into_unit(),
        }).collect())
    }

    // ------------------------------------------------------------------ images, opaque

    fn image_unit(&mut self, node: &Node, width: i64, max_height: Option<i64>) -> Unit {
        let (mut w, mut h) = (node.bounds.width, node.bounds.height);
        let valid = w > 0 && h > 0;
        if !valid {
            self.diag("image_invalid_size", "An image has no valid size; a placeholder is shown", Some(node.id.clone()), true);
            w = width.min(50 * EMU_PER_MM);
            h = w;
        }
        if w > width {
            h = mul_div(h, width, w).max(1);
            w = width;
            self.diag("image_scaled_to_fit", "An image wider than the text area is shown scaled to fit", Some(node.id.clone()), false);
        }
        if let Some(max_h) = max_height {
            if h > max_h {
                w = mul_div(w, max_h, h).max(1);
                h = max_h;
                self.diag("image_scaled_to_fit", "An image taller than the page area is shown scaled to fit", Some(node.id.clone()), false);
            }
        }
        let x = match node.style.align.as_str() { "center" => (width - w) / 2, "right" => width - w, _ => 0 }.max(0);
        let bounds = Rect { x, y: 0, width: w, height: h };
        let mut unit = Unit { height: h, ..Default::default() };
        match (&node.asset, valid) {
            (Some(asset), true) => unit.items.push(Primitive::Image { node_id: node.id.clone(), bounds, asset: asset.clone(), fit: None, image_crop_pixel_rect: None, style: None }),
            _ => {
                if node.asset.is_none() { self.diag("image_missing_asset", "An image has no asset; a placeholder is shown", Some(node.id.clone()), true); }
                let style = Style { fill: "#F3F4F6".into(), stroke: "#9CA3AF".into(), stroke_width: EMU_PER_PT / 2, ..Style::default() };
                unit.items.push(Primitive::Shape { node_id: node.id.clone(), bounds, shape: "rect".into(), style });
            }
        }
        unit.regions.push(SourceRegion { node_id: node.id.clone(), bounds, logical_range: None, grapheme_rects: vec![] });
        unit
    }

    fn opaque_unit(&mut self, id: &str, label: &str, width: i64) -> DltResult<Unit> {
        let pad = 2 * EMU_PER_MM;
        let style = Style { font_size: 9 * EMU_PER_PT, color: "#4B5563".into(), fill: "#F3F4F6".into(), stroke: "#9CA3AF".into(), stroke_width: EMU_PER_PT / 2, ..Style::default() };
        let text = if label.trim().is_empty() { "unsupported content".to_string() } else { label.to_string() };
        let shaped = self.shape(&text, &style, id)?;
        let height = shaped.ascent + shaped.descent + 2 * pad;
        let bounds = Rect { x: 0, y: 0, width, height };
        self.diag("opaque_not_rendered", format!("Preserved content '{text}' is shown as a placeholder"), Some(id.to_string()), true);
        let mut unit = Unit { height, ..Default::default() };
        unit.items.push(Primitive::Shape { node_id: id.to_string(), bounds, shape: "rect".into(), style: style.clone() });
        let text_w = shaped.size.width;
        unit.items.push(Primitive::Text {
            node_id: id.to_string(),
            bounds: Rect { x: pad, y: pad, width: text_w, height: shaped.ascent + shaped.descent },
            origin: Point { x: pad, y: pad + shaped.ascent },
            text,
            style,
            shaped,
        });
        unit.regions.push(SourceRegion { node_id: id.to_string(), bounds, logical_range: None, grapheme_rects: vec![] });
        Ok(unit)
    }
}

fn is_rtl(c: char) -> bool {
    let u = c as u32;
    (0x0590..=0x08FF).contains(&u) || (0xFB1D..=0xFDFF).contains(&u) || (0xFE70..=0xFEFF).contains(&u) || (0x10800..=0x10FFF).contains(&u) || (0x1E800..=0x1EFFF).contains(&u)
}

/// Grapheme boundary byte offsets of `text`, including 0 and `text.len()`.
pub fn grapheme_bounds(text: &str) -> Vec<usize> {
    let mut v: Vec<usize> = text.grapheme_indices(true).map(|(i, _)| i).collect();
    v.push(text.len());
    v
}

fn x_at(rs: &RunShape, byte: usize) -> i64 {
    let b = &rs.bounds;
    let cx = &rs.shaped.caret_x;
    match b.binary_search(&byte) {
        Ok(k) => cx[k],
        Err(k) => {
            if k == 0 || k >= b.len() { return *cx.last().unwrap_or(&0); }
            let (a0, a1) = (b[k - 1], b[k]);
            let (x0, x1) = (cx[k - 1], cx[k]);
            x0 + mul_div(x1 - x0, (byte - a0) as i64, (a1 - a0) as i64)
        }
    }
}

fn mul_div(a: i64, b: i64, c: i64) -> i64 {
    if c == 0 { return 0; }
    let n = i128::from(a) * i128::from(b);
    let c = i128::from(c);
    let q = n / c;
    let r = n % c;
    (q + if 2 * r.abs() >= c.abs() { n.signum() * c.signum() } else { 0 }) as i64
}

fn build_clusters(lp: &LogicalParagraph, p: &Paragraph, runs: &[RunShape]) -> Vec<Cluster> {
    lp.graphemes.iter().enumerate().map(|(index, g)| {
        let text = lp.text[g.logical.clone()].to_string();
        if g.protected {
            let r = g.pieces[0].run;
            let rs = &runs[r];
            let width = if rs.placeholder { p.runs[r].style.font_size.max(1) } else { rs.shaped.size.width };
            return Cluster { index: index as u32, kind: Kind::Atom, width, pieces: vec![PieceX { run: r, bytes: 0..rs.display.len(), x0: 0, x1: width }], run: r, text };
        }
        let pieces: Vec<PieceX> = g.pieces.iter().map(|pc| {
            let rs = &runs[pc.run];
            PieceX { run: pc.run, bytes: pc.bytes.clone(), x0: x_at(rs, pc.bytes.start), x1: x_at(rs, pc.bytes.end) }
        }).collect();
        let kind = if is_forced_break(&text) { Kind::Break } else if text == "\t" { Kind::Tab } else if text.chars().all(is_space) { Kind::Space } else { Kind::Text };
        let width = match kind { Kind::Break | Kind::Tab => 0, _ => pieces.iter().map(|pc| pc.x1 - pc.x0).sum() };
        Cluster { index: index as u32, kind, width, run: g.pieces[0].run, pieces, text }
    }).collect()
}

/// Advance of a cluster at line-relative position `x` (tabs jump to the next tab stop).
fn cluster_advance(c: &Cluster, x: i64) -> i64 {
    if c.kind == Kind::Tab {
        let next = (x.div_euclid(DEFAULT_TAB_STOP) + 1) * DEFAULT_TAB_STOP;
        return next - x;
    }
    c.width
}

fn line_left(format: &ParagraphFormat, line_no: usize) -> i64 {
    if line_no == 0 { format.indent.max(0) } else { (-format.indent).max(0) }
}

fn line_width(clusters: &[Cluster], start: usize, end: usize, left: i64) -> i64 {
    let mut x = 0;
    let mut content = 0;
    for c in &clusters[start..end] {
        x += cluster_advance(c, left + x);
        if !matches!(c.kind, Kind::Space | Kind::Break) { content = x; }
    }
    content
}

fn nearest_run(lp: &LogicalParagraph, index: usize, run_count: usize) -> usize {
    if index > 0 { if let Some(g) = lp.graphemes.get(index - 1) { return g.pieces.last().map(|p| p.run).unwrap_or(0); } }
    lp.graphemes.get(index).and_then(|g| g.pieces.first()).map(|p| p.run).unwrap_or(0).min(run_count.saturating_sub(1))
}

fn translate_all(items: &mut [Primitive], regions: &mut [SourceRegion], links: &mut [TextLink], dx: i64, dy: i64) {
    for item in items.iter_mut() {
        match item {
            Primitive::Text { bounds, origin, .. } => { move_rect(bounds, dx, dy); origin.x += dx; origin.y += dy; }
            // Primitive::Path (contract v1.2) is drawn inside its bounds, so moving the bounds moves it.
            Primitive::Shape { bounds, .. } | Primitive::Image { bounds, .. } | Primitive::Path { bounds, .. } => move_rect(bounds, dx, dy),
            Primitive::Line { from, to, .. } => { from.x += dx; from.y += dy; to.x += dx; to.y += dy; }
        }
    }
    for region in regions.iter_mut() {
        move_rect(&mut region.bounds, dx, dy);
        for r in region.grapheme_rects.iter_mut() { move_rect(r, dx, dy); }
    }
    for link in links.iter_mut() { move_rect(&mut link.bounds, dx, dy); }
}

fn move_rect(r: &mut Rect, dx: i64, dy: i64) { r.x += dx; r.y += dy; }

/// The `OpenUrl` target of each flow image node that has one, by node id. Block images and images in table cells are drawn; a
/// node's children are not, so they are not read.
fn open_url_targets(blocks: &[FlowBlock], out: &mut BTreeMap<Id, String>) -> DltResult<()> {
    for block in blocks {
        match block {
            FlowBlock::Image(node) => {
                let mut urls = node.interactions.iter().filter_map(|interaction| match interaction {
                    Interaction::OpenUrl { url } => Some(url),
                    _ => None,
                });
                let Some(url) = urls.next() else { continue };
                if urls.next().is_some() {
                    return Err(DltError::invalid(format!("flow image {} has more than one OpenUrl interaction", node.id)));
                }
                out.insert(node.id.clone(), url.clone());
            }
            FlowBlock::Table(table) => {
                for cell in table.rows.iter().flatten() { open_url_targets(&cell.blocks, out)?; }
            }
            FlowBlock::Paragraph(_) | FlowBlock::Opaque { .. } => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod link_tests {
    //! Link regions of linked flow images. These call `layout_document`, the layout after the v1.2 guard, because the public
    //! `layout_flow` refuses `OpenUrl` (P0); tests/pdf_links.rs covers the public behavior.
    use super::*;
    use designllm_render::{png_from_rgba, Renderer};
    use std::path::PathBuf;
    use std::sync::Mutex;

    const URL: &str = "https://example.com/portfolio?lang=ko";

    #[derive(Default)]
    struct Assets(Mutex<BTreeMap<String, Vec<u8>>>);
    impl AssetStore for Assets {
        fn put(&self, bytes: &[u8], mime: &str) -> DltResult<AssetRef> {
            let asset = AssetRef { sha256: sha256(bytes), mime: mime.into(), bytes: bytes.len() as u64 };
            self.0.lock().unwrap().insert(asset.sha256.clone(), bytes.to_vec());
            Ok(asset)
        }
        fn read(&self, asset: &AssetRef) -> DltResult<Vec<u8>> {
            self.0.lock().unwrap().get(&asset.sha256).cloned().ok_or_else(|| DltError::new(ErrorCode::NotFound, "asset"))
        }
        fn lookup(&self, hash: &str) -> DltResult<AssetRef> { Err(DltError::new(ErrorCode::NotFound, hash)) }
    }

    fn renderer() -> Renderer { Renderer::bundled_only(&PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../assets/fonts")).unwrap() }

    fn picture(assets: &Assets) -> AssetRef { assets.put(&png_from_rgba(2, 2, &[200u8; 16]).unwrap(), "image/png").unwrap() }

    fn image(id: &str, width: i64, height: i64, asset: Option<AssetRef>, url: Option<&str>) -> FlowBlock {
        FlowBlock::Image(Node {
            id: id.into(), kind: NodeKind::Image, bounds: Rect { x: 0, y: 0, width, height }, text: String::new(), style: Style::default(), asset,
            children: vec![], layout: None, interactions: url.map(|url| Interaction::OpenUrl { url: url.into() }).into_iter().collect(),
            variants: Default::default(), ext: Default::default(),
        })
    }

    fn paragraph(id: &str) -> FlowBlock {
        FlowBlock::Paragraph(Paragraph { id: id.into(), runs: vec![TextRun::new(format!("{id}-r"), "본문")], style_ref: None, format: ParagraphFormat::default() })
    }

    fn document(blocks: Vec<FlowBlock>) -> FlowDocument {
        FlowDocument { sections: vec![Section { id: "s1".into(), setup: PageSetup::default(), blocks }], styles: Default::default() }
    }

    /// The drawn bounds of the picture or placeholder of `node_id` on page `page`.
    fn drawn(scene: &RenderScene, page: usize, node_id: &str) -> Rect {
        scene.pages[page].primitives.iter().find_map(|primitive| match primitive {
            Primitive::Image { node_id: id, bounds, .. } | Primitive::Shape { node_id: id, bounds, .. } if id.as_str() == node_id => Some(*bounds),
            _ => None,
        }).unwrap()
    }

    #[test]
    fn a_linked_image_gets_one_region_where_it_is_drawn() {
        let assets = Assets::default();
        let blocks = vec![paragraph("p1"), image("img", 40 * EMU_PER_MM, 30 * EMU_PER_MM, Some(picture(&assets)), Some(URL)), paragraph("p2")];
        let scene = layout_document(&document(blocks), &renderer(), BTreeMap::new(), Folding::Off).unwrap();
        assert_eq!(scene.links.len(), 1, "{:?}", scene.links);
        let link = &scene.links[0];
        assert_eq!((link.page_id.as_str(), link.node_id.as_str(), link.url.as_str()), ("s1/page/0", "img", URL));
        assert_eq!(link.bounds, drawn(&scene, 0, "img"));
        assert_eq!(link.bounds.x, PageSetup::default().margin_left, "a left aligned image starts at the left margin");
        scene.check_links().unwrap();
    }

    #[test]
    fn a_linked_image_on_a_later_page_names_that_page() {
        let assets = Assets::default();
        let blocks = vec![
            image("tall", 40 * EMU_PER_MM, 200 * EMU_PER_MM, Some(picture(&assets)), None),
            image("linked", 40 * EMU_PER_MM, 100 * EMU_PER_MM, Some(picture(&assets)), Some(URL)),
        ];
        let scene = layout_document(&document(blocks), &renderer(), BTreeMap::new(), Folding::Off).unwrap();
        assert_eq!(scene.pages.len(), 2);
        assert_eq!(scene.links.len(), 1, "{:?}", scene.links);
        assert_eq!(scene.links[0].page_id, "s1/page/1");
        assert_eq!(scene.links[0].bounds, drawn(&scene, 1, "linked"));
        assert_eq!(scene.links[0].bounds.y, PageSetup::default().margin_top, "the second page starts at the top margin");
    }

    #[test]
    fn a_linked_image_in_a_table_cell_is_linked_where_the_cell_draws_it() {
        let assets = Assets::default();
        let cell = TableCell { id: "cell".into(), blocks: vec![image("cell-img", 30 * EMU_PER_MM, 20 * EMU_PER_MM, Some(picture(&assets)), Some(URL))], column_span: 1, row_span: 1 };
        let table = Table { id: "t".into(), column_widths: vec![170 * EMU_PER_MM], rows: vec![vec![cell]], style: Style::default() };
        let scene = layout_document(&document(vec![FlowBlock::Table(table)]), &renderer(), BTreeMap::new(), Folding::Off).unwrap();
        assert_eq!(scene.links.len(), 1, "{:?}", scene.links);
        assert_eq!((scene.links[0].page_id.as_str(), scene.links[0].node_id.as_str()), ("s1/page/0", "cell-img"));
        assert_eq!(scene.links[0].bounds, drawn(&scene, 0, "cell-img"));
        scene.check_links().unwrap();
    }

    #[test]
    fn a_linked_image_without_an_asset_links_its_placeholder() {
        let scene = layout_document(&document(vec![image("img", 40 * EMU_PER_MM, 30 * EMU_PER_MM, None, Some(URL))]), &renderer(), BTreeMap::new(), Folding::Off).unwrap();
        assert_eq!(scene.links.len(), 1, "{:?}", scene.links);
        assert_eq!(scene.links[0].bounds, drawn(&scene, 0, "img"));
    }

    #[test]
    fn a_linked_scene_writes_uri_annotations_into_the_pdf() {
        let assets = Assets::default();
        let renderer = renderer();
        let scene = layout_document(&document(vec![image("img", 40 * EMU_PER_MM, 30 * EMU_PER_MM, Some(picture(&assets)), Some(URL))]), &renderer, BTreeMap::new(), Folding::Off).unwrap();
        let output = renderer.encode(&scene, "pdf", &RenderProfile::default(), &assets).unwrap();
        for needle in ["/Annots", "/Link", "/URI", URL] {
            assert!(output.bytes.windows(needle.len()).any(|window| window == needle.as_bytes()), "the PDF lacks {needle}");
        }
    }

    #[test]
    fn two_open_urls_on_one_image_are_refused() {
        let FlowBlock::Image(mut node) = image("img", 40 * EMU_PER_MM, 30 * EMU_PER_MM, None, Some(URL)) else { unreachable!() };
        node.interactions.push(Interaction::OpenUrl { url: "mailto:a@example.com".into() });
        let error = layout_document(&document(vec![FlowBlock::Image(node)]), &renderer(), BTreeMap::new(), Folding::Off).unwrap_err();
        assert_eq!(error.code, ErrorCode::InvalidArgument, "{}", error.message);
    }
}
