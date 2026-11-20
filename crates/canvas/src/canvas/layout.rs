//! Scene layout for canvas pages. Output is `RenderPage` primitives plus a `source_map`, with text shaped by the injected
//! renderer (`RenderProvider::shape_text`). The encoder never reshapes text.
//!
//! Model (shared with the HTML export so both mean the same thing):
//! - Child bounds are relative to the parent's border-box origin. Page children are relative to the page origin.
//! - Containers are groups and rectangles. A rectangle is painted, a group is not. A container's children are placed
//!   by its layout: `absolute` at their bounds, `row` and `column` in order with `gap` and `justify` on the main axis,
//!   and `grid` in equal columns (`grid_columns`, `gap` between columns, `row_gap` between rows, `grid_span` per child).
//!   Padding per side (`padding_sides`, else `padding`) applies to the flow and grid modes.
//! - `align_self` on a child overrides the container's `align` on the cross axis (flow) and inside its cell (grid).
//! - Every node with a layout sizes itself: `fixed` uses the bounds, `fill` takes the space the parent offers, and
//!   `hug` takes the content size (children for containers, the text for text, the bounds for shapes and images without
//!   text). Min and max then clamp the result. Fill children in a flow share the remaining main-axis space equally.
//! - Hidden nodes take no space and draw nothing.
//! - Breakpoints use the page width in CSS pixels (96 dpi). Every matching breakpoint overrides `mode`, `gap`, `padding`,
//!   `align`, `grid_columns`, and `justify` in ascending `min_width_css_px` order.
//! - Text wraps at spaces and between CJK ideographs and kana. Other graphemes break only when a word cannot fit.
//!   Hangul and Latin text breaks at spaces only, which is the conservative choice.

use crate::common::{is_ideograph_or_kana, is_space, viewport_px};
use designllm_contracts::{
    json_hash, AlignSelf, CanvasDocument, CanvasTable, Diagnostic, DltError, DltResult, ErrorCode, FontFaceRef, Interaction, Justify, LayoutSpec,
    Node, NodeKind, Point, Primitive, RenderPage, RenderProvider, Rect, SceneLink, ShapedText, Size, SourceRegion, Style, TextListKind,
    TextRange,
};
use designllm_render::text_block_offset;
use std::collections::{BTreeMap, BTreeSet};
use unicode_segmentation::UnicodeSegmentation;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    Absolute,
    Row,
    Column,
    Grid,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Sizing {
    Fixed,
    Fill,
    Hug,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Align {
    Start,
    Center,
    End,
    Stretch,
}

/// Layout values after breakpoints are applied. Sizing and limits come from the base spec.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Eff {
    pub mode: Mode,
    pub width: Sizing,
    pub height: Sizing,
    /// Gap between children on the main axis (between columns in grid mode).
    pub gap: i64,
    /// Padding per side in EMU: top, right, bottom, left.
    pub pads: [i64; 4],
    /// Gap between grid rows. Follows `gap` unless the spec sets it.
    pub row_gap: i64,
    /// Number of grid columns (grid mode).
    pub grid_columns: u32,
    pub justify: Justify,
    pub align: Align,
    pub min_w: Option<i64>,
    pub max_w: Option<i64>,
    pub min_h: Option<i64>,
    pub max_h: Option<i64>,
}

fn parse_mode(text: &str) -> Mode {
    match text {
        "row" => Mode::Row,
        "column" => Mode::Column,
        "grid" => Mode::Grid,
        _ => Mode::Absolute,
    }
}

fn parse_sizing(text: &str) -> Sizing {
    match text {
        "fill" => Sizing::Fill,
        "hug" => Sizing::Hug,
        _ => Sizing::Fixed,
    }
}

fn parse_align(text: &str) -> Align {
    match text {
        "center" => Align::Center,
        "end" => Align::End,
        "stretch" => Align::Stretch,
        _ => Align::Start,
    }
}

/// `align_self` of a child. `Auto` (and no value) defers to the container's `align`.
fn item_align(spec: &LayoutSpec) -> Option<Align> {
    match spec.align_self? {
        AlignSelf::Auto => None,
        AlignSelf::Start => Some(Align::Start),
        AlignSelf::Center => Some(Align::Center),
        AlignSelf::End => Some(Align::End),
        AlignSelf::Stretch => Some(Align::Stretch),
    }
}

/// Defaults for a node without a layout: absolute, fixed, no padding.
pub(crate) fn absolute_default() -> Eff {
    Eff {
        mode: Mode::Absolute,
        width: Sizing::Fixed,
        height: Sizing::Fixed,
        gap: 0,
        pads: [0; 4],
        row_gap: 0,
        grid_columns: 1,
        justify: Justify::Start,
        align: Align::Start,
        min_w: None,
        max_w: None,
        min_h: None,
        max_h: None,
    }
}

fn base(spec: &LayoutSpec) -> Eff {
    Eff {
        mode: parse_mode(&spec.mode),
        width: parse_sizing(&spec.width_mode),
        height: parse_sizing(&spec.height_mode),
        gap: spec.gap,
        pads: spec.padding_sides.unwrap_or([spec.padding; 4]),
        row_gap: spec.row_gap.unwrap_or(spec.gap),
        grid_columns: spec.grid_columns.unwrap_or(1),
        justify: spec.justify.unwrap_or(Justify::Start),
        align: parse_align(&spec.align),
        min_w: spec.min_width,
        max_w: spec.max_width,
        min_h: spec.min_height,
        max_h: spec.max_height,
    }
}

/// Effective layout for a viewport width in CSS pixels. Matching breakpoints apply in ascending order.
pub(crate) fn effective(spec: &LayoutSpec, viewport: u32) -> Eff {
    let mut eff = base(spec);
    let mut breakpoints: Vec<_> = spec.breakpoints.iter().collect();
    breakpoints.sort_by_key(|b| b.min_width_css_px);
    for bp in breakpoints.into_iter().filter(|b| b.min_width_css_px <= viewport) {
        if let Some(mode) = &bp.mode {
            eff.mode = parse_mode(mode);
        }
        if let Some(gap) = bp.gap {
            eff.gap = gap;
        }
        if let Some(padding) = bp.padding {
            eff.pads = [padding; 4];
        }
        if let Some(align) = &bp.align {
            eff.align = parse_align(align);
        }
        if let Some(columns) = bp.grid_columns {
            eff.grid_columns = columns;
        }
        if let Some(justify) = bp.justify {
            eff.justify = justify;
        }
    }
    eff.row_gap = spec.row_gap.unwrap_or(eff.gap);
    eff
}

fn is_container(kind: NodeKind) -> bool {
    matches!(kind, NodeKind::Group | NodeKind::Rectangle)
}

fn is_hidden(node: &Node) -> bool {
    node.ext.hidden == Some(true)
}

/// Children that take part in layout. Hidden nodes take no space.
fn visible(children: &[Node]) -> Vec<&Node> {
    children.iter().filter(|c| !is_hidden(c)).collect()
}

fn has_text(node: &Node) -> bool {
    (node.kind == NodeKind::Text && node.ext.list.is_some())
        || (!node.text.is_empty() && matches!(node.kind, NodeKind::Text | NodeKind::Rectangle | NodeKind::Ellipse))
}

fn eff_of(node: &Node, viewport: u32) -> Eff {
    node.layout.as_ref().map_or_else(absolute_default, |spec| effective(spec, viewport))
}

fn base_of(node: &Node) -> Eff {
    node.layout.as_ref().map_or_else(absolute_default, base)
}

fn sizing_of(node: &Node, width: bool) -> Sizing {
    match &node.layout {
        Some(spec) => parse_sizing(if width { &spec.width_mode } else { &spec.height_mode }),
        None => Sizing::Fixed,
    }
}

fn item_align_of(node: &Node) -> Option<Align> {
    node.layout.as_ref().and_then(item_align)
}

fn span_of(node: &Node) -> usize {
    node.layout.as_ref().and_then(|spec| spec.grid_span).map_or(1, |span| span.max(1) as usize)
}

fn clamp(value: i64, min: Option<i64>, max: Option<i64>) -> i64 {
    let mut v = value;
    if let Some(m) = min {
        v = v.max(m);
    }
    if let Some(m) = max {
        v = v.min(m);
    }
    v
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Frame {
    pub x: i64,
    pub y: i64,
    pub w: i64,
    pub h: i64,
}

impl Frame {
    fn rect(self) -> Rect {
        Rect { x: self.x, y: self.y, width: self.w, height: self.h }
    }
}

/// The content box of a frame, inside the padding.
fn inner_frame(frame: Frame, pads: [i64; 4]) -> Frame {
    let [top, right, bottom, left] = pads;
    Frame { x: frame.x + left, y: frame.y + top, w: (frame.w - left - right).max(0), h: (frame.h - top - bottom).max(0) }
}

fn checked_inner_frame(frame: Frame, pads: [i64; 4]) -> DltResult<Frame> {
    let [top, right, bottom, left] = pads;
    let x = frame.x.checked_add(left).ok_or_else(|| DltError::invalid("table inner x position overflows EMU"))?;
    let y = frame.y.checked_add(top).ok_or_else(|| DltError::invalid("table inner y position overflows EMU"))?;
    let w = frame.w.checked_sub(left).and_then(|width| width.checked_sub(right))
        .ok_or_else(|| DltError::invalid("table inner width overflows EMU"))?.max(0);
    let h = frame.h.checked_sub(top).and_then(|height| height.checked_sub(bottom))
        .ok_or_else(|| DltError::invalid("table inner height overflows EMU"))?.max(0);
    Ok(Frame { x, y, w, h })
}

fn checked_frame(x: i64, y: i64, w: i64, h: i64) -> DltResult<Frame> {
    if w < 0 || h < 0 {
        return Err(DltError::invalid("layout frame has a negative size"));
    }
    x.checked_add(w).ok_or_else(|| DltError::invalid("layout frame right edge overflows EMU"))?;
    y.checked_add(h).ok_or_else(|| DltError::invalid("layout frame bottom edge overflows EMU"))?;
    Ok(Frame { x, y, w, h })
}

fn clip_link_to_page(bounds: Rect, page: Size, node_id: &str) -> DltResult<Option<Rect>> {
    if page.width <= 0 || page.height <= 0 || bounds.width < 0 || bounds.height < 0 {
        return Err(DltError::new(ErrorCode::Internal, format!("canvas node {node_id} has invalid link bounds")));
    }
    if bounds.width == 0 || bounds.height == 0 { return Ok(None); }
    let right = bounds.x.checked_add(bounds.width).ok_or_else(|| DltError::new(ErrorCode::Internal, format!("canvas node {node_id} link bounds overflow")))?;
    let bottom = bounds.y.checked_add(bounds.height).ok_or_else(|| DltError::new(ErrorCode::Internal, format!("canvas node {node_id} link bounds overflow")))?;
    let (x, y) = (bounds.x.max(0), bounds.y.max(0));
    let (right, bottom) = (right.min(page.width), bottom.min(page.height));
    if right <= x || bottom <= y { return Ok(None); }
    Ok(Some(Rect { x, y, width: right - x, height: bottom - y }))
}

/// Start offset and spacing between children for `justify`. `free` is the main-axis space left over after the children and
/// their gaps. Negative free space overflows for `center` and `end`, and the space-* modes fall back to start, as in CSS.
fn justify_offsets(justify: Justify, free: i64, n: i64, gap: i64) -> (i64, i64) {
    let spare = free.max(0);
    match justify {
        Justify::Start => (0, gap),
        Justify::Center => (free / 2, gap),
        Justify::End => (free, gap),
        Justify::SpaceBetween => {
            if n > 1 {
                (0, gap + spare / (n - 1))
            } else {
                (0, gap)
            }
        }
        Justify::SpaceAround => (spare / (2 * n), gap + spare / n),
        Justify::SpaceEvenly => (spare / (n + 1), gap + spare / (n + 1)),
    }
}

/// Offset of an item inside its cross-axis (or cell) space of `free` size.
fn align_offset(align: Align, free: i64) -> i64 {
    match align {
        Align::Start | Align::Stretch => 0,
        Align::Center => free / 2,
        Align::End => free,
    }
}

/// Grid cell of one child: its index in the visible children, its row/column and span extents.
struct GridCell {
    child: usize,
    col: usize,
    row: usize,
    span: usize,
    row_span: usize,
}

/// Grid tracks for one container at one inner width.
struct GridPlan {
    cells: Vec<GridCell>,
    col_x: Vec<i64>,
    col_w: Vec<i64>,
    row_h: Vec<i64>,
    row_gap: i64,
}

impl GridPlan {
    fn span(&self, col: usize, span: usize) -> DltResult<(i64, i64)> {
        let end = col.checked_add(span).filter(|end| span > 0 && *end <= self.col_x.len())
            .ok_or_else(|| DltError::invalid("grid column span is out of range"))?;
        let x = self.col_x[col];
        let right = self.col_x[end - 1].checked_add(self.col_w[end - 1])
            .ok_or_else(|| DltError::invalid("grid column edge overflows EMU"))?;
        let width = right.checked_sub(x).ok_or_else(|| DltError::invalid("grid column width overflows EMU"))?;
        Ok((x, width))
    }

    fn row_y(&self, row: usize) -> DltResult<i64> {
        let tracks = self.row_h.get(..row).ok_or_else(|| DltError::invalid("grid row is out of range"))?;
        let height = tracks.iter().try_fold(0i64, |sum, height| sum.checked_add(*height))
            .ok_or_else(|| DltError::invalid("grid row position overflows EMU"))?;
        let gaps = self.row_gap.checked_mul(i64::try_from(row).map_err(|_| DltError::invalid("grid row count is out of range"))?)
            .ok_or_else(|| DltError::invalid("grid row gaps overflow EMU"))?;
        height.checked_add(gaps).ok_or_else(|| DltError::invalid("grid row position overflows EMU"))
    }

    fn row_span_height(&self, row: usize, span: usize) -> DltResult<i64> {
        let end = row.checked_add(span).filter(|end| span > 0 && *end <= self.row_h.len())
            .ok_or_else(|| DltError::invalid("grid row span is out of range"))?;
        let tracks = self.row_h.get(row..end).ok_or_else(|| DltError::invalid("grid row span is out of range"))?;
        let height = tracks.iter().try_fold(0i64, |sum, height| sum.checked_add(*height))
            .ok_or_else(|| DltError::invalid("grid row span height overflows EMU"))?;
        let gap_count = i64::try_from(span - 1).map_err(|_| DltError::invalid("grid row span is out of range"))?;
        let gaps = self.row_gap.checked_mul(gap_count).ok_or_else(|| DltError::invalid("grid row span gaps overflow EMU"))?;
        height.checked_add(gaps).ok_or_else(|| DltError::invalid("grid row span height overflows EMU"))
    }

    fn total_height(&self) -> DltResult<i64> {
        let height = self.row_h.iter().try_fold(0i64, |sum, height| sum.checked_add(*height))
            .ok_or_else(|| DltError::invalid("grid height overflows EMU"))?;
        let gap_count = i64::try_from(self.row_h.len().saturating_sub(1))
            .map_err(|_| DltError::invalid("grid row count is out of range"))?;
        let gaps = self.row_gap.checked_mul(gap_count).ok_or_else(|| DltError::invalid("grid row gaps overflow EMU"))?;
        height.checked_add(gaps).ok_or_else(|| DltError::invalid("grid height overflows EMU"))
    }
}

/// Each table row is at least each hug cell's own height in it (its minimum, as made), so text that shrinks back gives the
/// minimum back (68 item 16). A merge keeps the cells' heights, so a merged span is at least the heights its rows had.
fn fit_table_row_minimums(kids: &[&Node], cols: usize, heights: &mut [i64]) {
    for (cells, height) in kids.chunks_exact(cols).zip(heights) {
        for cell in cells {
            if let Some(spec) = cell.layout.as_ref().filter(|spec| spec.height_mode == "hug") {
                *height = (*height).max(clamp(cell.bounds.height, spec.min_height, spec.max_height));
            }
        }
    }
}

/// Splits custom width weights into EMU tracks. Remainder EMUs go to the largest fractional
/// tracks, with column index breaking ties; the result always totals exactly `width`.
fn weighted_column_tracks(width: i64, weights: &[u32]) -> DltResult<(Vec<i64>, Vec<i64>)> {
    if weights.is_empty() || weights.iter().any(|weight| *weight == 0) {
        return Err(DltError::invalid("custom Canvas table column weights must be positive"));
    }
    if width < weights.len() as i64 {
        return Err(DltError::invalid("custom Canvas table would contain a zero-width column"));
    }
    let total_weight = weights.iter().try_fold(0u64, |total, weight| total.checked_add(u64::from(*weight)))
        .ok_or_else(|| DltError::invalid("custom Canvas table column weights overflow"))?;
    let total = width as u128;
    let denominator = u128::from(total_weight);
    let mut widths = Vec::with_capacity(weights.len());
    let mut remainders = Vec::with_capacity(weights.len());
    let mut used = 0i64;
    for (index, weight) in weights.iter().enumerate() {
        let numerator = total * u128::from(*weight);
        let quotient = numerator / denominator;
        let track = i64::try_from(quotient).map_err(|_| DltError::invalid("custom Canvas table column width overflows EMU"))?;
        used = used.checked_add(track).ok_or_else(|| DltError::invalid("custom Canvas table width sum overflows EMU"))?;
        widths.push(track);
        remainders.push((numerator % denominator, index));
    }
    let remainder_count = usize::try_from(width - used).map_err(|_| DltError::invalid("custom Canvas table remainder is invalid"))?;
    remainders.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.cmp(&right.1)));
    for (_, index) in remainders.into_iter().take(remainder_count) {
        widths[index] += 1;
    }
    if widths.iter().any(|track| *track <= 0) {
        return Err(DltError::invalid("custom Canvas table would contain a zero-width column"));
    }
    let mut x = 0i64;
    let mut positions = Vec::with_capacity(widths.len());
    for track in &widths {
        positions.push(x);
        x = x.checked_add(*track).ok_or_else(|| DltError::invalid("custom Canvas table column position overflows EMU"))?;
    }
    if x != width {
        return Err(DltError::invalid("custom Canvas table columns do not fill their frame"));
    }
    Ok((positions, widths))
}

/// Font faces used by the scene, keyed by face key. Fingerprint input.
#[derive(Debug, Default)]
pub(crate) struct FontUse {
    pub faces: BTreeMap<String, FontFaceRef>,
}

pub(crate) fn font_fingerprint(faces: &BTreeMap<String, FontFaceRef>) -> DltResult<String> {
    let list: Vec<(&str, u32, &str, &BTreeMap<String, String>)> = faces
        .values()
        .map(|f| (f.file_sha256.as_str(), f.face_index, f.postscript_name.as_str(), &f.variations))
        .collect();
    json_hash(&list)
}

fn trimmed_width(renderer: &dyn RenderProvider, style: &Style, g: &[&str], start: usize, end: usize) -> DltResult<i64> {
    let mut e = end;
    while e > start && is_space(g[e - 1]) {
        e -= 1;
    }
    if e == start {
        return Ok(0);
    }
    let text = g[start..e].concat();
    Ok(renderer.shape_text(&text, style)?.size.width)
}

/// Breaks one hard-line paragraph `[ps, pe)` into rendered ranges. Each range excludes trailing spaces.
fn wrap_paragraph(
    renderer: &dyn RenderProvider,
    style: &Style,
    g: &[&str],
    ps: usize,
    pe: usize,
    max_w: i64,
) -> DltResult<Vec<(usize, usize)>> {
    if ps == pe {
        return Ok(vec![(ps, ps)]);
    }
    let mut lines = Vec::new();
    let mut s = ps;
    while s < pe {
        if !lines.is_empty() {
            while s < pe && is_space(g[s]) {
                s += 1;
            }
            if s >= pe {
                break;
            }
        }
        // Largest end whose trimmed width fits. The first grapheme is always taken, even when it overflows.
        let mut fit = s + 1;
        let mut e = s + 1;
        while e <= pe {
            let width = trimmed_width(renderer, style, g, s, e)?;
            if width <= max_w || e == s + 1 {
                fit = e;
                e += 1;
            } else {
                break;
            }
        }
        // Prefer the last break opportunity at or before the fit point.
        let line_end = if fit == pe {
            pe
        } else {
            (s + 1..=fit)
                .rev()
                .find(|&o| o == pe || is_space(g[o - 1]) || is_ideograph_or_kana(g[o - 1]) || is_ideograph_or_kana(g[o]))
                .unwrap_or(fit)
        };
        let mut rendered_end = line_end;
        while rendered_end > s && is_space(g[rendered_end - 1]) {
            rendered_end -= 1;
        }
        lines.push((s, rendered_end));
        s = line_end;
    }
    Ok(lines)
}

/// Hard-line paragraphs of a text, as grapheme index ranges `[start, end)`.
fn paragraphs_of(g: &[&str]) -> Vec<(usize, usize)> {
    let mut paragraphs = Vec::new();
    let mut start = 0usize;
    for (i, grapheme) in g.iter().enumerate() {
        if *grapheme == "\n" || *grapheme == "\r\n" {
            paragraphs.push((start, i));
            start = i + 1;
        }
    }
    paragraphs.push((start, g.len()));
    paragraphs
}

#[derive(Debug)]
pub(super) struct ListMarker { pub(super) text: String, pub(super) shaped: ShapedText }

pub(super) struct ListLayout { pub(super) markers: Vec<ListMarker>, pub(super) marker_column_width: i64, pub(super) gap: i64 }

impl ListLayout {
    pub(super) fn gutter_width(&self) -> DltResult<i64> {
        self.marker_column_width.checked_add(self.gap).ok_or_else(|| DltError::invalid("list marker gutter overflows EMU"))
    }
}

fn marker_text(kind: TextListKind, item_index: usize) -> String {
    match kind {
        TextListKind::Bullet => "•".into(),
        TextListKind::Numbered => format!("{}.", item_index + 1),
    }
}

pub(super) fn list_layout(
    renderer: &dyn RenderProvider,
    style: &Style,
    kind: Option<TextListKind>,
    item_count: usize,
) -> DltResult<Option<ListLayout>> {
    let Some(kind) = kind else { return Ok(None) };
    let mut markers = Vec::with_capacity(item_count);
    let mut marker_column_width = 0;
    for item_index in 0..item_count {
        let text = marker_text(kind, item_index);
        let shaped = renderer.shape_text(&text, style)?;
        marker_column_width = marker_column_width.max(shaped.size.width);
        markers.push(ListMarker { text, shaped });
    }
    Ok(Some(ListLayout { markers, marker_column_width, gap: (style.font_size / 2).max(1) }))
}

/// Span width of `span` columns starting at `col`, gaps included.
fn span_width(plan_cols: (&[i64], &[i64]), col: usize, span: usize) -> DltResult<i64> {
    let (col_x, col_w) = plan_cols;
    let end = col.checked_add(span).filter(|end| span > 0 && *end <= col_x.len() && *end <= col_w.len())
        .ok_or_else(|| DltError::invalid("grid column span is out of range"))?;
    let right = col_x[end - 1].checked_add(col_w[end - 1])
        .ok_or_else(|| DltError::invalid("grid column edge overflows EMU"))?;
    right.checked_sub(col_x[col]).ok_or_else(|| DltError::invalid("grid column width overflows EMU"))
}

/// Whether an image draws anything from its style: opacity below 1, a corner radius (clip), or an outer shadow (v1.2).
/// An image without them keeps the v1.1 drawing, so its primitive carries no style.
fn image_look(style: &Style) -> bool {
    style.opacity_milli.is_some_and(|m| m < 1000) || style.corner_radius.is_some_and(|r| r > 0) || !style.shadows.is_empty()
}

struct Painter<'a, 'b> {
    renderer: &'a dyn RenderProvider,
    viewport: u32,
    page_id: String,
    page_size: Size,
    primitives: Vec<Primitive>,
    regions: Vec<SourceRegion>,
    diagnostics: Vec<Diagnostic>,
    fonts: &'b mut FontUse,
    links: Vec<SceneLink>,
    /// Absolute bounds of every node drawn, for node-selection exports (a hidden node is not drawn and has none).
    frames: BTreeMap<String, Rect>,
}

impl<'a, 'b> Painter<'a, 'b> {
    /// Records the faces a run was drawn with and reports its missing glyphs. Returns the English family names of the faces
    /// the run used when none of them has the requested name, so the caller reports the substitution once per node.
    fn note_shaped(&mut self, shaped: &ShapedText, node_id: &str, style: &Style) -> Option<Vec<String>> {
        for face in &shaped.fonts_used {
            self.fonts.faces.entry(face.key.clone()).or_insert_with(|| face.clone());
        }
        if !shaped.missing_glyphs.is_empty() {
            self.diagnostics.push(Diagnostic {
                code: "GLYPH_MISSING".into(),
                message: format!("no glyph for {} grapheme(s)", shaped.missing_glyphs.len()),
                node_id: Some(node_id.to_string()),
                blocking: false,
                params: Default::default(),
            });
        }
        // The run is drawn with the requested font when one of its faces has the requested name in any language (for
        // example the Korean name of Malgun Gothic). A run where no face has it was drawn with substitutes.
        let matched = shaped.fonts_used.iter().any(|face| designllm_render::face_matches_family(face, &style.font_family));
        if shaped.fonts_used.is_empty() || matched {
            return None;
        }
        Some(shaped.fonts_used.iter().map(|face| face.family.clone()).collect())
    }

    /// Size of a text laid out in `width` (None: unwrapped, one line per paragraph). Returns the widest line and the total
    /// height. It follows the same breaking and line heights as `place_text`, so a hugging box fits its text exactly.
    fn measure_text(&self, style: &Style, text: &str, width: Option<i64>, list: Option<TextListKind>) -> DltResult<(i64, i64)> {
        let renderer = self.renderer;
        let g: Vec<&str> = text.graphemes(true).collect();
        let paragraphs = paragraphs_of(&g);
        let list_layout = list_layout(renderer, style, list, paragraphs.len())?;
        let gutter = match &list_layout { Some(layout) => layout.gutter_width()?, None => 0 };
        let content_width = width.map(|w| (w - gutter).max(0));
        let mut max_w = 0i64;
        let mut total_h = 0i64;
        let mut probe: Option<i64> = None;
        for (paragraph_index, (ps, pe)) in paragraphs.into_iter().enumerate() {
            let ranges = match content_width {
                Some(w) => wrap_paragraph(renderer, style, &g, ps, pe, w)?,
                None => {
                    let mut e = pe;
                    while e > ps && is_space(g[e - 1]) {
                        e -= 1;
                    }
                    vec![(ps, e)]
                }
            };
            for (visual_index, (s, e)) in ranges.into_iter().enumerate() {
                let marker_height = if visual_index == 0 {
                    list_layout.as_ref().map_or(0, |layout| layout.markers[paragraph_index].shaped.size.height.max(1))
                } else {
                    0
                };
                let line = g[s..e].concat();
                if line.is_empty() {
                    let h = match probe {
                        Some(h) => h,
                        None => {
                            let h = renderer.shape_text("Hg", style)?.size.height.max(1);
                            probe = Some(h);
                            h
                        }
                    };
                    total_h += h.max(marker_height);
                    continue;
                }
                let shaped = renderer.shape_text(&line, style)?;
                max_w = max_w.max(shaped.size.width);
                total_h += shaped.size.height.max(marker_height).max(1);
            }
        }
        Ok((max_w + gutter, total_h))
    }

    /// Content width of a node for `hug` (without padding).
    fn content_width(&self, node: &Node, eff: &Eff) -> DltResult<i64> {
        let text = has_text(node);
        if !is_container(node.kind) && !text {
            return Ok(node.bounds.width);
        }
        let mut w = 0i64;
        if is_container(node.kind) {
            let kids = visible(&node.children);
            match eff.mode {
                Mode::Absolute => {
                    for child in &kids {
                        let (cw, _) = self.node_size(child, None, None)?;
                        w = w.max(child.bounds.x + cw);
                    }
                }
                Mode::Row => {
                    for child in &kids {
                        let (cw, _) = self.node_size(child, None, None)?;
                        w += cw;
                    }
                    if !kids.is_empty() {
                        w += eff.gap * (kids.len() as i64 - 1);
                    }
                }
                Mode::Column => {
                    for child in &kids {
                        let (cw, _) = self.node_size(child, None, None)?;
                        w = w.max(cw);
                    }
                }
                // Grid width is fixed or fill (validation), so a hugging grid keeps its bounds.
                Mode::Grid => w = node.bounds.width,
            }
        }
        if text {
            let (tw, _) = self.measure_text(&node.style, &node.text, None, node.ext.list)?;
            w = w.max(tw);
        }
        Ok(w)
    }

    /// Content height of a node for `hug`, given its resolved border-box width.
    fn content_height(&self, node: &Node, eff: &Eff, width: i64, pad_h: i64) -> DltResult<i64> {
        let text = has_text(node);
        if !is_container(node.kind) && !text {
            return Ok(node.bounds.height);
        }
        let mut h = 0i64;
        if is_container(node.kind) {
            let kids = visible(&node.children);
            match eff.mode {
                Mode::Absolute => {
                    for child in &kids {
                        let (_, ch) = self.node_size(child, None, None)?;
                        h = h.max(child.bounds.y + ch);
                    }
                }
                Mode::Row => {
                    for child in &kids {
                        let (_, ch) = self.node_size(child, None, None)?;
                        h = h.max(ch);
                    }
                }
                Mode::Column => {
                    // Children that fill the width are measured at the container's content width, so wrapped text is exact.
                    let inner_w = width - pad_h;
                    for child in &kids {
                        let forced = (sizing_of(child, true) == Sizing::Fill).then_some(inner_w);
                        let (_, ch) = self.node_size(child, Some((inner_w, 0)), forced)?;
                        h += ch;
                    }
                    if !kids.is_empty() {
                        h += eff.gap * (kids.len() as i64 - 1);
                    }
                }
                Mode::Grid => {
                    let plan = if let Some(table) = &node.ext.table {
                        self.grid_plan_with_weights(&kids, width - pad_h, eff, table.column_widths.as_deref(), Some(table))?
                    } else {
                        self.grid_plan(&kids, width - pad_h, eff)?
                    };
                    h = plan.total_height()?;
                }
            }
        }
        if text {
            let (_, th) = self.measure_text(&node.style, &node.text, Some(width), node.ext.list)?;
            h = h.max(th);
        }
        Ok(h)
    }

    /// Border-box size of a node. `avail` is the space the parent offers (None means the node takes its bounds for
    /// `fill`, as when measuring content for `hug`). `forced_width` is the width a grid cell or a stretch imposes.
    fn node_size(&self, node: &Node, avail: Option<(i64, i64)>, forced_width: Option<i64>) -> DltResult<(i64, i64)> {
        let Some(spec) = node.layout.as_ref() else {
            return Ok((forced_width.unwrap_or(node.bounds.width), node.bounds.height));
        };
        let base_eff = base(spec);
        let eff = effective(spec, self.viewport);
        let (pad_h, pad_v) = if eff.mode == Mode::Absolute {
            (0, 0)
        } else {
            (eff.pads[1] + eff.pads[3], eff.pads[0] + eff.pads[2])
        };
        let width = match forced_width {
            Some(w) => w,
            None => match base_eff.width {
                Sizing::Fixed => node.bounds.width,
                Sizing::Fill => avail.map_or(node.bounds.width, |a| a.0),
                Sizing::Hug => self.content_width(node, &eff)? + pad_h,
            },
        };
        let width = clamp(width, base_eff.min_w, base_eff.max_w);
        let height = match base_eff.height {
            Sizing::Fixed => node.bounds.height,
            Sizing::Fill => avail.map_or(node.bounds.height, |a| a.1),
            Sizing::Hug => self.content_height(node, &eff, width, pad_h)? + pad_v,
        };
        Ok((width, clamp(height, base_eff.min_h, base_eff.max_h)))
    }

    /// Grid tracks for a container at `inner_w` (the content width). Columns are equal, `gap` apart. Rows take the
    /// height of their tallest child, and `fill` children take the row height.
    fn grid_plan(&self, kids: &[&Node], inner_w: i64, eff: &Eff) -> DltResult<GridPlan> {
        self.grid_plan_with_weights(kids, inner_w, eff, None, None)
    }

    fn grid_plan_with_weights(
        &self,
        kids: &[&Node],
        inner_w: i64,
        eff: &Eff,
        column_weights: Option<&[u32]>,
        table: Option<&CanvasTable>,
    ) -> DltResult<GridPlan> {
        let cols = eff.grid_columns.max(1) as usize;
        let table_grid = table.map(super::model::table_span_grid).transpose().map_err(DltError::invalid)?;
        let (cells, rows) = if let Some(table) = table {
            if table.rows.len().checked_mul(cols) != Some(kids.len()) {
                return Err(DltError::invalid("Canvas table cells do not match their declared grid"));
            }
            let spans = table_grid.as_ref().expect("table span grid was built above");
            let cells = spans.cells.iter().enumerate().filter_map(|(child, position)| match position {
                super::model::TableCellPosition::Anchor { row_span, column_span } => Some(GridCell {
                    child,
                    col: child % cols,
                    row: child / cols,
                    span: *column_span,
                    row_span: *row_span,
                }),
                super::model::TableCellPosition::Covered { .. } => None,
            }).collect::<Vec<_>>();
            (cells, table.rows.len())
        } else {
            let mut cells = Vec::with_capacity(kids.len());
            let (mut row, mut col) = (0usize, 0usize);
            for (child, node) in kids.iter().enumerate() {
                let span = span_of(node).min(cols);
                if col + span > cols {
                    row += 1;
                    col = 0;
                }
                cells.push(GridCell { child, col, row, span, row_span: 1 });
                col += span;
                if col == cols {
                    row += 1;
                    col = 0;
                }
            }
            let rows = cells.iter().map(|cell| cell.row + 1).max().unwrap_or(0);
            (cells, rows)
        };
        let n = cols as i64;
        let gap = eff.gap;
        let gap_total = gap.checked_mul(n - 1).ok_or_else(|| DltError::invalid("grid column gaps overflow EMU"))?;
        let total = inner_w.checked_sub(gap_total).ok_or_else(|| DltError::invalid("grid width overflows EMU"))?.max(0);
        let (col_x, col_w) = if let Some(weights) = column_weights {
            if weights.len() != cols {
                return Err(DltError::invalid("Canvas table column weights do not match its grid columns"));
            }
            weighted_column_tracks(total, weights)?
        } else {
            let mut boundaries = Vec::with_capacity(cols + 1);
            for index in 0..=cols {
                let offset = i128::from(total) * index as i128 / i128::from(n);
                boundaries.push(i64::try_from(offset).map_err(|_| DltError::invalid("grid column position overflows EMU"))?);
            }
            let widths = boundaries.windows(2).map(|pair| {
                pair[1].checked_sub(pair[0]).ok_or_else(|| DltError::invalid("grid column width overflows EMU"))
            }).collect::<DltResult<Vec<_>>>()?;
            (boundaries[..cols].to_vec(), widths)
        };
        let col_x: Vec<i64> = col_x.into_iter().enumerate().map(|(i, x)| {
            let offset = gap.checked_mul(i as i64).ok_or_else(|| DltError::invalid("grid column position overflows EMU"))?;
            x.checked_add(offset).ok_or_else(|| DltError::invalid("grid column position overflows EMU"))
        }).collect::<DltResult<Vec<_>>>()?;
        let mut row_h = vec![0i64; rows];
        let mut fill_h = vec![0i64; rows];
        let mut row_span_heights = Vec::new();
        for cell in &cells {
            let node = kids[cell.child];
            let span_w = span_width((&col_x, &col_w), cell.col, cell.span)?;
            let stretch = item_align_of(node).unwrap_or(eff.align) == Align::Stretch;
            if stretch || sizing_of(node, false) == Sizing::Fill {
                if cell.row_span == 1 {
                    fill_h[cell.row] = fill_h[cell.row].max(node.bounds.height);
                } else {
                    row_span_heights.push((cell.row, cell.row_span, node.bounds.height));
                }
                continue;
            }
            let forced = (stretch || sizing_of(node, true) == Sizing::Fill).then_some(span_w);
            let (_, h) = self.node_size(node, Some((span_w, 0)), forced)?;
            if cell.row_span == 1 {
                row_h[cell.row] = row_h[cell.row].max(h);
            } else {
                row_span_heights.push((cell.row, cell.row_span, h));
            }
        }
        if let Some(spans) = &table_grid {
            let mut anchors_with_covered_text = BTreeSet::new();
            for (child, position) in spans.cells.iter().enumerate() {
                let super::model::TableCellPosition::Covered { anchor_index } = position else { continue };
                let row = child / cols;
                let column = child % cols;
                let node = kids[child];
                if !node.text.is_empty() {
                    anchors_with_covered_text.insert(*anchor_index);
                    // The full span measures this text below. Measuring it at one column here
                    // would preserve the old unmerged wrapping and overstate the merged rows.
                    row_h[row] = row_h[row].max(node.bounds.height);
                    continue;
                }
                let span_w = span_width((&col_x, &col_w), column, 1)?;
                let forced = (item_align_of(node).unwrap_or(eff.align) == Align::Stretch || sizing_of(node, true) == Sizing::Fill)
                    .then_some(span_w);
                let (_, height) = self.node_size(node, Some((span_w, 0)), forced)?;
                row_h[row] = row_h[row].max(height);
            }
            for cell in &cells {
                let has_covered_text = anchors_with_covered_text.contains(&cell.child);
                if !has_covered_text {
                    continue;
                }
                let span_w = span_width((&col_x, &col_w), cell.col, cell.span)?;
                let mut content_height = 0i64;
                for row in cell.row..cell.row + cell.row_span {
                    for column in cell.col..cell.col + cell.span {
                        let node = kids[row * cols + column];
                        if node.text.is_empty() {
                            continue;
                        }
                        let (_, height) = self.measure_text(&node.style, &node.text, Some(span_w), node.ext.list)?;
                        content_height = content_height.checked_add(height)
                            .ok_or_else(|| DltError::invalid("merged table text height overflows EMU"))?;
                    }
                }
                row_span_heights.push((cell.row, cell.row_span, content_height));
            }
        }
        for r in 0..rows {
            if row_h[r] == 0 {
                row_h[r] = fill_h[r];
            }
        }
        // Before the spans take their share, so a span grows only by what its content needs past its rows' minimums.
        if table.is_some() {
            fit_table_row_minimums(kids, cols, &mut row_h);
        }
        for (row, row_span, required) in row_span_heights {
            let end = row.checked_add(row_span).filter(|end| row_span > 0 && *end <= row_h.len())
                .ok_or_else(|| DltError::invalid("grid row span is out of range"))?;
            let current_height = row_h.get(row..end).ok_or_else(|| DltError::invalid("grid row span is out of range"))?
                .iter().try_fold(0i64, |sum, height| sum.checked_add(*height))
                .ok_or_else(|| DltError::invalid("grid row span height overflows EMU"))?;
            let gap_count = i64::try_from(row_span.saturating_sub(1))
                .map_err(|_| DltError::invalid("grid row span is out of range"))?;
            let gap_height = eff.row_gap.checked_mul(gap_count)
                .ok_or_else(|| DltError::invalid("grid row span gaps overflow EMU"))?;
            let current = current_height.checked_add(gap_height)
                .ok_or_else(|| DltError::invalid("grid row span height overflows EMU"))?;
            if required > current {
                let delta = required.checked_sub(current).ok_or_else(|| DltError::invalid("grid row span height overflows EMU"))?;
                let last = &mut row_h[end - 1];
                *last = (*last).checked_add(delta).ok_or_else(|| DltError::invalid("grid row height overflows EMU"))?;
            }
        }
        Ok(GridPlan { cells, col_x, col_w, row_h, row_gap: eff.row_gap })
    }

    fn place_children(&mut self, children: &[Node], frame: Frame, eff: Eff) -> DltResult<()> {
        let kids = visible(children);
        match eff.mode {
            Mode::Absolute => {
                for child in kids {
                    let (w, h) = self.node_size(child, Some((frame.w, frame.h)), None)?;
                    let x = frame.x.checked_add(child.bounds.x).ok_or_else(|| DltError::invalid("absolute child x position overflows EMU"))?;
                    let y = frame.y.checked_add(child.bounds.y).ok_or_else(|| DltError::invalid("absolute child y position overflows EMU"))?;
                    let child_frame = checked_frame(x, y, w, h)?;
                    self.place_node(child, child_frame)?;
                }
                Ok(())
            }
            Mode::Row | Mode::Column => self.place_flow(&kids, frame, eff),
            Mode::Grid => self.place_grid(&kids, frame, eff),
        }
    }

    fn place_flow(&mut self, kids: &[&Node], frame: Frame, eff: Eff) -> DltResult<()> {
        if kids.is_empty() {
            return Ok(());
        }
        let inner = checked_inner_frame(frame, eff.pads)?;
        let row = eff.mode == Mode::Row;
        let (inner_main, inner_cross) = if row { (inner.w, inner.h) } else { (inner.h, inner.w) };
        let n = kids.len();

        let mut main = Vec::with_capacity(n);
        let mut cross = Vec::with_capacity(n);
        let mut fill_main = Vec::with_capacity(n);
        let mut fill_cross = Vec::with_capacity(n);
        for child in kids {
            let (width_mode, height_mode) = (sizing_of(child, true), sizing_of(child, false));
            let (main_mode, cross_mode) = if row { (width_mode, height_mode) } else { (height_mode, width_mode) };
            let filled_cross = cross_mode == Sizing::Fill;
            // A column child that fills the width is measured at that width, so a wrapped height is exact.
            let forced = (!row && filled_cross).then_some(inner.w);
            let (w, h) = self.node_size(child, Some((inner.w, inner.h)), forced)?;
            let (m, c) = if row { (w, h) } else { (h, w) };
            main.push(m);
            cross.push(if filled_cross { inner_cross } else { c });
            fill_main.push(main_mode == Sizing::Fill);
            fill_cross.push(filled_cross);
        }
        let fill_count = fill_main.iter().filter(|f| **f).count() as i64;
        if fill_count > 0 {
            let fixed = main.iter().zip(fill_main.iter()).filter(|(_, fill)| !**fill)
                .try_fold(0i64, |sum, (size, _)| sum.checked_add(*size))
                .ok_or_else(|| DltError::invalid("flow fixed child sizes overflow EMU"))?;
            let gaps = eff.gap.checked_mul(n as i64 - 1).ok_or_else(|| DltError::invalid("flow gaps overflow EMU"))?;
            let remaining = inner_main.checked_sub(fixed).and_then(|value| value.checked_sub(gaps))
                .ok_or_else(|| DltError::invalid("flow remaining space overflows EMU"))?.max(0);
            let share = remaining / fill_count;
            for (m, f) in main.iter_mut().zip(fill_main.iter()) {
                if *f {
                    *m = share;
                }
            }
        }
        if row {
            // A row child that fills the width takes its height at the final width, so wrapped text is exact.
            for (i, child) in kids.iter().enumerate() {
                if fill_main[i] && !fill_cross[i] {
                    cross[i] = self.node_size(child, Some((inner.w, inner.h)), Some(main[i]))?.1;
                }
            }
        }

        let used = main.iter().try_fold(0i64, |sum, value| sum.checked_add(*value))
            .ok_or_else(|| DltError::invalid("flow child sizes overflow EMU"))?;
        let gap_total = eff.gap.checked_mul(n as i64 - 1).ok_or_else(|| DltError::invalid("flow gaps overflow EMU"))?;
        let free = inner_main.checked_sub(used).and_then(|value| value.checked_sub(gap_total))
            .ok_or_else(|| DltError::invalid("flow free space overflows EMU"))?;
        let (start, spacing) = justify_offsets(eff.justify, free, n as i64, eff.gap);
        let mut cursor = (if row { inner.x } else { inner.y }).checked_add(start)
            .ok_or_else(|| DltError::invalid("flow child position overflows EMU"))?;
        let cross_start = if row { inner.y } else { inner.x };
        for (i, child) in kids.iter().enumerate() {
            let align = item_align_of(child).unwrap_or(eff.align);
            let stretched = align == Align::Stretch || fill_cross[i];
            let cross_size = if stretched { inner_cross } else { cross[i] };
            let cross_free = inner_cross.checked_sub(cross_size).ok_or_else(|| DltError::invalid("flow cross-axis size overflows EMU"))?;
            let cross_pos = cross_start.checked_add(align_offset(align, cross_free))
                .ok_or_else(|| DltError::invalid("flow cross-axis position overflows EMU"))?;
            let child_frame = if row {
                checked_frame(cursor, cross_pos, main[i], cross_size)?
            } else {
                checked_frame(cross_pos, cursor, cross_size, main[i])?
            };
            self.place_node(child, child_frame)?;
            cursor = cursor.checked_add(main[i]).and_then(|value| value.checked_add(spacing))
                .ok_or_else(|| DltError::invalid("flow child position overflows EMU"))?;
        }
        Ok(())
    }

    fn place_grid(&mut self, kids: &[&Node], frame: Frame, eff: Eff) -> DltResult<()> {
        if kids.is_empty() {
            return Ok(());
        }
        let inner = checked_inner_frame(frame, eff.pads)?;
        let plan = self.grid_plan(kids, inner.w, &eff)?;
        for cell in &plan.cells {
            let node = kids[cell.child];
            let (span_x, span_w) = plan.span(cell.col, cell.span)?;
            let row_y = plan.row_y(cell.row)?;
            let row_h = plan.row_h[cell.row];
            let align = item_align_of(node).unwrap_or(eff.align);
            let stretch = align == Align::Stretch;
            let width_fill = sizing_of(node, true) == Sizing::Fill;
            let height_fill = sizing_of(node, false) == Sizing::Fill;
            let forced = (stretch || width_fill).then_some(span_w);
            let (w, h) = self.node_size(node, Some((span_w, row_h)), forced)?;
            let h = if stretch || height_fill {
                let limits = base_of(node);
                clamp(row_h, limits.min_h, limits.max_h)
            } else {
                h
            };
            let x_offset = align_offset(align, span_w.checked_sub(w).ok_or_else(|| DltError::invalid("grid child width overflows EMU"))?);
            let y_offset = align_offset(align, row_h.checked_sub(h).ok_or_else(|| DltError::invalid("grid child height overflows EMU"))?);
            let x = inner.x.checked_add(span_x).and_then(|value| value.checked_add(x_offset))
                .ok_or_else(|| DltError::invalid("grid child x position overflows EMU"))?;
            let y = inner.y.checked_add(row_y).and_then(|value| value.checked_add(y_offset))
                .ok_or_else(|| DltError::invalid("grid child y position overflows EMU"))?;
            self.place_node(node, checked_frame(x, y, w, h)?)?;
        }
        Ok(())
    }

    /// Canvas table cells share their row track height and fill their equal-width column track.
    /// Ordinary Grid keeps its per-child sizing and alignment behavior in `place_grid`.
    fn place_table_children(&mut self, children: &[Node], frame: Frame, eff: Eff, table: &CanvasTable, table_id: &str) -> DltResult<()> {
        let kids = visible(children);
        if kids.is_empty() {
            return Ok(());
        }
        let spans = super::model::table_span_grid(table).map_err(DltError::invalid)?;
        let mut span_members = vec![Vec::<&Node>::new(); kids.len()];
        for (child, position) in spans.cells.iter().enumerate() {
            match position {
                super::model::TableCellPosition::Anchor { .. } => span_members[child].push(kids[child]),
                super::model::TableCellPosition::Covered { anchor_index } => span_members[*anchor_index].push(kids[child]),
            }
        }
        let has_populated_span = span_members.iter().any(|members| members.iter().skip(1).any(|member| !member.text.is_empty()));
        let inner = if has_populated_span { checked_inner_frame(frame, eff.pads)? } else { inner_frame(frame, eff.pads) };
        let plan = self.grid_plan_with_weights(&kids, inner.w, &eff, table.column_widths.as_deref(), Some(table))?;
        if has_populated_span {
            let content_height = plan.total_height()?;
            if content_height > inner.h {
                self.diagnostics.push(
                    Diagnostic {
                        code: "TABLE_SPAN_PARENT_OVERFLOW".into(),
                        message: format!("merged table content needs {content_height} EMU but its parent provides {} EMU", inner.h),
                        node_id: Some(table_id.to_owned()),
                        blocking: true,
                        params: Default::default(),
                    }
                    .with_param("content_height", content_height)
                    .with_param("available_height", inner.h),
                );
            }
        }
        for cell in &plan.cells {
            let node = kids[cell.child];
            let (x, width) = plan.span(cell.col, cell.span)?;
            let y_offset = plan.row_y(cell.row)?;
            let height = plan.row_span_height(cell.row, cell.row_span)?;
            let cell_frame = if has_populated_span {
                let x = inner.x.checked_add(x).ok_or_else(|| DltError::invalid("merged table cell x position overflows EMU"))?;
                let y = inner.y.checked_add(y_offset).ok_or_else(|| DltError::invalid("merged table cell y position overflows EMU"))?;
                checked_frame(x, y, width, height)?
            } else {
                Frame { x: inner.x + x, y: inner.y + y_offset, w: width, h: height }
            };
            let members = &span_members[cell.child];
            let populated = members.iter().skip(1).any(|member| !member.text.is_empty());
            if populated {
                let right = cell_frame.x.checked_add(cell_frame.w).ok_or_else(|| DltError::invalid("merged table cell right edge overflows EMU"))?;
                let bottom = cell_frame.y.checked_add(cell_frame.h).ok_or_else(|| DltError::invalid("merged table cell bottom edge overflows EMU"))?;
                if cell_frame.x < 0 || cell_frame.y < 0 || right > self.page_size.width || bottom > self.page_size.height {
                    self.diagnostics.push(Diagnostic {
                        code: "TABLE_SPAN_PAGE_OVERFLOW".into(),
                        message: format!("merged table cell {} extends beyond the page bounds", node.id),
                        node_id: Some(node.id.clone()),
                        blocking: true,
                        params: Default::default(),
                    });
                }
                self.place_merged_table_text(members, cell_frame)?;
            } else {
                self.place_node(node, cell_frame)?;
            }
        }
        Ok(())
    }

    fn place_merged_table_text(&mut self, members: &[&Node], frame: Frame) -> DltResult<()> {
        // Public engine entry points still validate and reject text in covered cells before reaching this private path.
        let Some(anchor) = members.first() else { return Ok(()) };
        self.frames.insert(anchor.id.clone(), frame.rect());
        self.record_node_interactions(anchor, frame)?;
        self.primitives.push(Primitive::Shape {
            node_id: anchor.id.clone(),
            bounds: frame.rect(),
            shape: "rectangle".into(),
            style: anchor.style.clone(),
        });
        if anchor.text.is_empty() {
            self.regions.push(SourceRegion {
                node_id: anchor.id.clone(),
                bounds: frame.rect(),
                logical_range: None,
                grapheme_rects: Vec::new(),
            });
        }
        let mut text_blocks = Vec::new();
        let mut content_height = 0i64;
        for node in members {
            if node.text.is_empty() {
                continue;
            }
            let (_, height) = self.measure_text(&node.style, &node.text, Some(frame.w), node.ext.list)?;
            content_height = content_height.checked_add(height)
                .ok_or_else(|| DltError::invalid("merged table text height overflows EMU"))?;
            text_blocks.push((*node, height));
        }
        let offset = text_block_offset(frame.h, content_height, anchor.style.vertical_align);
        let mut y = frame.y.checked_add(offset).ok_or_else(|| DltError::invalid("merged table text position overflows EMU"))?;
        for (node, height) in text_blocks {
            let text_frame = checked_frame(frame.x, y, frame.w, height)?;
            if node.id != anchor.id {
                self.frames.insert(node.id.clone(), text_frame.rect());
            }
            self.place_text(node, text_frame)?;
            y = text_frame.y.checked_add(text_frame.h).ok_or_else(|| DltError::invalid("merged table text position overflows EMU"))?;
        }
        Ok(())
    }

    fn place_node(&mut self, node: &Node, frame: Frame) -> DltResult<()> {
        self.frames.insert(node.id.clone(), frame.rect());
        self.record_node_interactions(node, frame)?;
        match node.kind {
            NodeKind::Path => {
                let Some(path) = node.ext.path.clone() else {
                    return Err(DltError::invalid(format!("path node {} has no path", node.id)));
                };
                self.primitives.push(Primitive::Path {
                    node_id: node.id.clone(),
                    bounds: frame.rect(),
                    path,
                    style: node.style.clone(),
                });
                self.regions.push(SourceRegion {
                    node_id: node.id.clone(),
                    bounds: frame.rect(),
                    logical_range: None,
                    grapheme_rects: Vec::new(),
                });
                Ok(())
            }
            NodeKind::Group => {
                self.regions.push(SourceRegion {
                    node_id: node.id.clone(),
                    bounds: frame.rect(),
                    logical_range: None,
                    grapheme_rects: Vec::new(),
                });
                let eff = eff_of(node, self.viewport);
                if let Some(table) = &node.ext.table {
                    self.place_table_children(&node.children, frame, eff, table, &node.id)
                } else {
                    self.place_children(&node.children, frame, eff)
                }
            }
            NodeKind::Text => self.place_text(node, frame),
            NodeKind::Rectangle | NodeKind::Ellipse => {
                let shape = if node.kind == NodeKind::Ellipse { "ellipse" } else { "rectangle" };
                self.primitives.push(Primitive::Shape {
                    node_id: node.id.clone(),
                    bounds: frame.rect(),
                    shape: shape.into(),
                    style: node.style.clone(),
                });
                if node.text.is_empty() {
                    self.regions.push(SourceRegion {
                        node_id: node.id.clone(),
                        bounds: frame.rect(),
                        logical_range: None,
                        grapheme_rects: Vec::new(),
                    });
                } else {
                    self.place_text(node, frame)?;
                }
                // A rectangle with children is a container: its children are drawn above its fill and text.
                if !node.children.is_empty() {
                    let eff = eff_of(node, self.viewport);
                    self.place_children(&node.children, frame, eff)?;
                }
                Ok(())
            }
            NodeKind::Line => {
                self.primitives.push(Primitive::Line {
                    node_id: node.id.clone(),
                    from: Point { x: frame.x, y: frame.y },
                    to: Point { x: frame.x + frame.w, y: frame.y + frame.h },
                    style: node.style.clone(),
                    arrow: false,
                });
                self.regions.push(SourceRegion {
                    node_id: node.id.clone(),
                    bounds: frame.rect(),
                    logical_range: None,
                    grapheme_rects: Vec::new(),
                });
                Ok(())
            }
            NodeKind::Image => {
                match &node.asset {
                    // The renderer fits the picture to the frame (`fit`) and applies opacity, corner radius and shadow (`style`).
                    Some(asset) => self.primitives.push(Primitive::Image {
                        node_id: node.id.clone(),
                        bounds: frame.rect(),
                        asset: asset.clone(),
                        fit: node.ext.image_fit,
                        image_crop_pixel_rect: node.ext.image_crop_pixel_rect,
                        style: image_look(&node.style).then(|| node.style.clone()),
                    }),
                    None => self.diagnostics.push(Diagnostic {
                        code: "IMAGE_ASSET".into(),
                        message: "image node has no asset and is not drawn".into(),
                        node_id: Some(node.id.clone()),
                        blocking: false,
                        params: Default::default(),
                    }),
                }
                self.regions.push(SourceRegion {
                    node_id: node.id.clone(),
                    bounds: frame.rect(),
                    logical_range: None,
                    grapheme_rects: Vec::new(),
                });
                Ok(())
            }
            NodeKind::Video => {
                let poster = node.ext.video_poster_asset.as_ref().ok_or_else(|| {
                    DltError::invalid(format!("video node {} has no extracted poster asset", node.id))
                })?;
                self.primitives.push(Primitive::Image {
                    node_id: node.id.clone(),
                    bounds: frame.rect(),
                    asset: poster.clone(),
                    fit: Some(designllm_contracts::ImageFit::Contain),
                    image_crop_pixel_rect: None,
                    style: image_look(&node.style).then(|| node.style.clone()),
                });
                self.regions.push(SourceRegion {
                    node_id: node.id.clone(),
                    bounds: frame.rect(),
                    logical_range: None,
                    grapheme_rects: Vec::new(),
                });
                Ok(())
            }
        }
    }

    fn record_node_interactions(&mut self, node: &Node, frame: Frame) -> DltResult<()> {
        let mut open_url = None;
        for interaction in &node.interactions {
            if let Interaction::OpenUrl { url } = interaction {
                if open_url.replace(url.as_str()).is_some() {
                    return Err(DltError::invalid(format!("canvas node {} has more than one OpenUrl interaction", node.id)));
                }
            }
        }
        if let Some(url) = open_url {
            if let Some(bounds) = clip_link_to_page(frame.rect(), self.page_size, &node.id)? {
                self.links.push(SceneLink { page_id: self.page_id.clone(), node_id: node.id.clone(), bounds, url: url.to_string() });
            }
        }
        Ok(())
    }

    /// Lays out a text node inside its frame: wrapping, alignment, and one `Text` primitive per visual line, plus a
    /// separate marker primitive for each hard paragraph when the node is a list.
    /// Every visual line also gets its own source region, boxed by the line (not by the frame), so caret, hit test
    /// and selection work per line. A line owns the graphemes from its first rendered grapheme up to the next line's
    /// first one. The spaces dropped at a wrap point and the break at a paragraph end belong to the line they trail,
    /// so the regions are contiguous and cover the node text. Each owned grapheme has one rect; an undrawn one gets a
    /// zero-width rect at the end of its line.
    fn place_text(&mut self, node: &Node, frame: Frame) -> DltResult<()> {
        let renderer = self.renderer;
        let style = &node.style;
        let g: Vec<&str> = node.text.graphemes(true).collect();
        let total = g.len();
        let paragraphs = paragraphs_of(&g);
        let list = list_layout(renderer, style, node.ext.list, paragraphs.len())?;
        if total == 0 && list.is_none() {
            // An empty node has one empty line. Its box is the top line of the frame, so a caret there has line height.
            let height = renderer.shape_text("Hg", style)?.size.height.max(1);
            self.regions.push(SourceRegion {
                node_id: node.id.clone(),
                bounds: Rect { x: frame.x, y: frame.y, width: frame.w, height },
                logical_range: Some(TextRange { paragraph_id: node.id.clone(), start: 0, end: 0 }),
                grapheme_rects: Vec::new(),
            });
            return Ok(());
        }
        let content_frame = match &list {
            Some(layout) => {
                let gutter = layout.gutter_width()?;
                let x = frame.x.checked_add(gutter).ok_or_else(|| DltError::invalid("list text origin overflows EMU"))?;
                Frame { x, y: frame.y, w: frame.w.saturating_sub(gutter).max(0), h: frame.h }
            }
            None => frame,
        };

        if style.align == "justify" {
            self.diagnostics.push(Diagnostic {
                code: "TEXT_JUSTIFY_APPROXIMATED".into(),
                message: "justified text is drawn left-aligned".into(),
                node_id: Some(node.id.clone()),
                blocking: false,
                params: Default::default(),
            });
        }

        let mut rects: Vec<Option<Rect>> = vec![None; total];
        // One entry per visual line, in order. Undrawn graphemes and the line regions use them.
        let mut lines: Vec<LineBox> = Vec::new();
        // Vertical alignment places the whole text block inside the frame, by the renderer's rule. Text without it starts at
        // the top, as before.
        let dy = match style.vertical_align {
            Some(align) => {
                let (_, block) = self.measure_text(style, &node.text, Some(frame.w), node.ext.list)?;
                text_block_offset(frame.h, block, Some(align))
            }
            None => 0,
        };
        let top = frame.y + dy;
        let mut y = top;
        // A blank line still takes one line of height, measured from a probe string.
        let mut probe_height: Option<i64> = None;
        // English family names of the faces that replaced the requested font in some line of this node.
        let mut substitutes: BTreeSet<String> = BTreeSet::new();
        for (paragraph_index, (ps, pe)) in paragraphs.into_iter().enumerate() {
            let ranges = wrap_paragraph(renderer, style, &g, ps, pe, content_frame.w)?;
            for (visual_index, (s, e)) in ranges.into_iter().enumerate() {
                let marker = if visual_index == 0 { list.as_ref().map(|layout| &layout.markers[paragraph_index]) } else { None };
                let marker_height = marker.map_or(0, |marker| marker.shaped.size.height.max(1));
                if let (Some(layout), Some(marker)) = (&list, marker) {
                    if let Some(families) = self.note_shaped(&marker.shaped, &node.id, style) {
                        substitutes.extend(families);
                    }
                    let marker_offset = layout.marker_column_width - marker.shaped.size.width;
                    let marker_x = frame.x.checked_add(marker_offset).ok_or_else(|| DltError::invalid("list marker origin overflows EMU"))?;
                    self.primitives.push(Primitive::Text {
                        node_id: node.id.clone(),
                        bounds: Rect { x: marker_x, y, width: marker.shaped.size.width, height: marker_height },
                        origin: Point { x: marker_x, y: y + marker.shaped.baseline },
                        text: marker.text.clone(),
                        style: style.clone(),
                        shaped: marker.shaped.clone(),
                    });
                }
                let text = g[s..e].concat();
                if text.is_empty() {
                    let height = match probe_height {
                        Some(h) => h,
                        None => {
                            let h = renderer.shape_text("Hg", style)?.size.height.max(1);
                            probe_height = Some(h);
                            h
                        }
                    };
                    let height = height.max(marker_height);
                    lines.push(LineBox { first: s, end: s, top: y, height, right: content_frame.x });
                    y += height;
                    continue;
                }
                let shaped = renderer.shape_text(&text, style)?;
                if let Some(families) = self.note_shaped(&shaped, &node.id, style) {
                    substitutes.extend(families);
                }
                let line_w = shaped.size.width;
                let line_h = shaped.size.height.max(marker_height).max(1);
                let origin_x = aligned_x(&content_frame, line_w, &style.align);
                let origin = Point { x: origin_x, y: y + shaped.baseline };
                for (k, index) in (s..e).enumerate() {
                    rects[index] = Some(match shaped.grapheme_rects.get(k) {
                        Some(r) => Rect { x: origin.x + r.x, y, width: r.width, height: line_h },
                        None => Rect { x: origin.x, y, width: 0, height: line_h },
                    });
                }
                let right = rects[e - 1].map_or(origin_x + line_w, |r| r.x + r.width);
                self.primitives.push(Primitive::Text {
                    node_id: node.id.clone(),
                    bounds: Rect { x: content_frame.x, y, width: content_frame.w, height: line_h },
                    origin,
                    text: text.clone(),
                    style: style.clone(),
                    shaped,
                });
                lines.push(LineBox { first: s, end: e, top: y, height: line_h, right });
                y += line_h;
            }
        }
        if !substitutes.is_empty() {
            let families: Vec<String> = substitutes.into_iter().collect();
            self.diagnostics.push(Diagnostic {
                code: designllm_contracts::diag_codes::FONT_SUBSTITUTED.into(),
                message: format!("requested font {} was rendered with {}", style.font_family, families.join(", ")),
                node_id: Some(node.id.clone()),
                blocking: false,
                params: Default::default(),
            });
        }
        // A grapheme that is not drawn (a space at a wrap point, or a hard break) sits at the right edge of the nearest
        // line that ends at or before it. That is the line it trails, so hit tests land on the right side.
        let all_rects: Vec<Rect> = (0..total)
            .map(|index| match rects[index] {
                Some(rect) => rect,
                None => match lines.iter().rev().find(|l| l.end <= index).or(lines.first()) {
                    Some(l) => Rect { x: l.right, y: l.top, width: 0, height: l.height },
                    None => Rect { x: frame.x, y: frame.y, width: 0, height: 0 },
                },
            })
            .collect();
        if y - top > frame.h {
            self.diagnostics.push(
                Diagnostic {
                    code: "TEXT_OVERFLOW".into(),
                    message: format!("text needs {} EMU of height but the box is {} EMU", y - top, frame.h),
                    node_id: Some(node.id.clone()),
                    blocking: false,
                    params: Default::default(),
                }
                .with_param("text", y - top)
                .with_param("box", frame.h)
                .with_param("axis", "height")
                .with_param("node", node.id.clone()),
            );
        }
        if let Some(layout) = &list {
            let marker_right = frame
                .x
                .checked_add(layout.marker_column_width)
                .ok_or_else(|| DltError::invalid("list marker edge overflows EMU"))?;
            let text_right = lines
                .iter()
                .filter(|line| line.end > line.first)
                .map(|line| line.right)
                .max()
                .unwrap_or(frame.x);
            let required = marker_right
                .max(text_right)
                .checked_sub(frame.x)
                .ok_or_else(|| DltError::invalid("list content width overflows EMU"))?;
            if required > frame.w {
                self.diagnostics.push(
                    Diagnostic {
                        code: "TEXT_OVERFLOW".into(),
                        message: format!("text and list markers need {required} EMU of width but the box is {} EMU", frame.w),
                        node_id: Some(node.id.clone()),
                        blocking: false,
                        params: Default::default(),
                    }
                    .with_param("text", required)
                    .with_param("box", frame.w)
                    .with_param("axis", "width")
                    .with_param("node", node.id.clone()),
                );
            }
        }
        // Each line owns the graphemes from its first rendered one up to the next line's first one. The ranges are
        // contiguous and cover the whole text, as the flow engine's line regions do.
        for (n, line) in lines.iter().enumerate() {
            let start = if n == 0 { 0 } else { line.first };
            let end = lines.get(n + 1).map_or(total, |next| next.first);
            self.regions.push(SourceRegion {
                node_id: node.id.clone(),
                bounds: Rect { x: content_frame.x, y: line.top, width: content_frame.w, height: line.height },
                logical_range: Some(TextRange { paragraph_id: node.id.clone(), start: start as u32, end: end as u32 }),
                grapheme_rects: all_rects[start..end].to_vec(),
            });
        }
        Ok(())
    }
}

/// One visual line of a text node while it is laid out.
struct LineBox {
    /// First rendered grapheme (the leading spaces of a continuation line are not rendered).
    first: usize,
    /// One past the last rendered grapheme (trailing spaces are not rendered).
    end: usize,
    /// Top of the line box and its height, in page EMU.
    top: i64,
    height: i64,
    /// Right edge of the rendered text. Undrawn graphemes of the line sit here.
    right: i64,
}

fn aligned_x(frame: &Frame, line_w: i64, align: &str) -> i64 {
    match align {
        "center" => frame.x + (frame.w - line_w).max(0) / 2,
        "right" => frame.x + (frame.w - line_w).max(0),
        _ => frame.x,
    }
}

/// Result of laying out a set of pages.
pub(crate) struct LayoutOutput {
    pub pages: Vec<RenderPage>,
    pub diagnostics: Vec<Diagnostic>,
    pub fonts: BTreeMap<String, FontFaceRef>,
    /// Viewport width in CSS pixels per page ID, used for breakpoints.
    pub viewports: BTreeMap<String, u32>,
    /// Absolute bounds of every node drawn, by node ID (node-selection exports).
    pub frames: BTreeMap<String, Rect>,
    pub links: Vec<SceneLink>,
}

/// Lays out the requested page indices (all pages when `only` is None). `page_index` stays the document index.
pub(crate) fn layout_pages(doc: &CanvasDocument, renderer: &dyn RenderProvider, only: Option<&[u32]>) -> DltResult<LayoutOutput> {
    let mut fonts = FontUse::default();
    let mut pages = Vec::new();
    let mut diagnostics = Vec::new();
    let mut viewports = BTreeMap::new();
    let mut frames = BTreeMap::new();
    let mut links = Vec::new();
    for (index, page) in doc.pages.iter().enumerate() {
        if let Some(list) = only {
            if !list.contains(&(index as u32)) {
                continue;
            }
        }
        let viewport = viewport_px(page.size.width);
        viewports.insert(page.id.clone(), viewport);
        let frame = Frame { x: 0, y: 0, w: page.size.width, h: page.size.height };
        let mut painter = Painter {
            renderer,
            viewport,
            page_id: page.id.clone(),
            page_size: page.size,
            primitives: Vec::new(),
            regions: Vec::new(),
            diagnostics: Vec::new(),
            fonts: &mut fonts,
            frames: BTreeMap::new(),
            links: Vec::new(),
        };
        painter.place_children(&page.nodes, frame, absolute_default())?;
        diagnostics.append(&mut painter.diagnostics);
        frames.append(&mut painter.frames);
        links.append(&mut painter.links);
        pages.push(RenderPage {
            id: page.id.clone(),
            page_index: index as u32,
            size: page.size,
            primitives: painter.primitives,
            source_map: painter.regions,
        });
    }
    // Node-level diagnostics are unique per node and code, so repeated shaping does not repeat them.
    diagnostics.dedup_by(|a, b| a.code == b.code && a.node_id == b.node_id && a.message == b.message);
    Ok(LayoutOutput { pages, diagnostics, fonts: fonts.faces, viewports, frames, links })
}

#[cfg(test)]
mod tests {
    use super::*;
    use designllm_contracts::Breakpoint;

    #[test]
    fn breakpoints_override_only_the_fields_they_set_and_apply_in_order() {
        let spec = LayoutSpec {
            mode: "absolute".into(),
            gap: 10,
            padding: 5,
            breakpoints: vec![
                Breakpoint { min_width_css_px: 600, mode: Some("row".into()), gap: None, padding: None, align: None, ..Default::default() },
                Breakpoint { min_width_css_px: 900, mode: None, gap: Some(40), padding: None, align: Some("center".into()), ..Default::default() },
            ],
            ..Default::default()
        };
        let small = effective(&spec, 320);
        assert_eq!((small.mode, small.gap, small.pads), (Mode::Absolute, 10, [5; 4]));
        let medium = effective(&spec, 700);
        assert_eq!((medium.mode, medium.gap), (Mode::Row, 10));
        let large = effective(&spec, 1200);
        assert_eq!((large.mode, large.gap, large.align), (Mode::Row, 40, Align::Center));
    }

    #[test]
    fn padding_sides_replace_the_uniform_padding_and_row_gap_follows_gap() {
        let spec = LayoutSpec {
            mode: "row".into(),
            gap: 10,
            padding: 5,
            padding_sides: Some([1, 2, 3, 4]),
            ..Default::default()
        };
        let eff = effective(&spec, 1200);
        assert_eq!(eff.pads, [1, 2, 3, 4]);
        assert_eq!(eff.row_gap, 10);
        let with_row_gap = effective(&LayoutSpec { row_gap: Some(7), ..spec }, 1200);
        assert_eq!(with_row_gap.row_gap, 7);
    }

    #[test]
    fn justify_distributes_the_free_space_like_css() {
        // Free space 30 among 3 children with gap 2.
        assert_eq!(justify_offsets(Justify::Start, 30, 3, 2), (0, 2));
        assert_eq!(justify_offsets(Justify::Center, 30, 3, 2), (15, 2));
        assert_eq!(justify_offsets(Justify::End, 30, 3, 2), (30, 2));
        assert_eq!(justify_offsets(Justify::SpaceBetween, 30, 3, 2), (0, 2 + 15));
        assert_eq!(justify_offsets(Justify::SpaceAround, 30, 3, 2), (5, 2 + 10));
        assert_eq!(justify_offsets(Justify::SpaceEvenly, 30, 3, 2), (7, 2 + 7));
        // One child: space-between starts at the beginning, space-around and space-evenly center it.
        assert_eq!(justify_offsets(Justify::SpaceBetween, 30, 1, 2), (0, 2));
        assert_eq!(justify_offsets(Justify::SpaceEvenly, 30, 1, 2), (15, 2 + 15));
        // Negative free space: center and end overflow, the space-* modes start at the beginning.
        assert_eq!(justify_offsets(Justify::Center, -10, 2, 0), (-5, 0));
        assert_eq!(justify_offsets(Justify::SpaceBetween, -10, 2, 0), (0, 0));
    }

    #[test]
    fn grid_columns_are_equal_and_spans_cover_their_columns() {
        let kid = |id: &str, span: u32| Node {
            id: id.into(),
            kind: NodeKind::Rectangle,
            bounds: Rect { x: 0, y: 0, width: 1, height: 100 },
            text: String::new(),
            style: Style::default(),
            asset: None,
            children: vec![],
            layout: Some(LayoutSpec { grid_span: Some(span), ..Default::default() }),
            interactions: vec![],
            variants: BTreeMap::new(),
            ext: Default::default(),
        };
        let a = kid("a", 1);
        let b = kid("b", 2);
        let c = kid("c", 1);
        let kids = vec![&a, &b, &c];
        let eff = Eff { grid_columns: 3, gap: 10, row_gap: 4, ..absolute_default() };
        let renderer = UnitRenderer;
        let painter = Painter { renderer: &renderer, viewport: 1280, page_id: "p".into(), page_size: Size::a4(), primitives: vec![], regions: vec![], diagnostics: vec![], fonts: &mut FontUse::default(), frames: BTreeMap::new(), links: vec![] };
        let plan = painter.grid_plan(&kids, 320, &eff).expect("plan");
        // Columns: 320 - 2 gaps of 10 = 300 split in three: 100 each, starting at 0, 110, 220.
        assert_eq!(plan.col_x, vec![0, 110, 220]);
        assert_eq!(plan.col_w, vec![100, 100, 100]);
        // Child b spans columns 1 and 2: from 110 to 320.
        assert_eq!(plan.span(1, 2).expect("column span"), (110, 210));
        // a and b share row 0, c starts row 1 because b used two columns.
        assert_eq!(plan.cells.iter().map(|cell| (cell.col, cell.row)).collect::<Vec<_>>(), vec![(0, 0), (1, 0), (0, 1)]);
        assert_eq!(plan.row_y(1).expect("row y"), 100 + 4);

        let wide = painter.grid_plan(&kids, i64::MAX, &Eff { grid_columns: 3, gap: 0, ..absolute_default() })
            .expect("near-i64 grid width splits with wide intermediate arithmetic");
        assert_eq!(wide.col_w, vec![i64::MAX / 3, i64::MAX / 3, i64::MAX - 2 * (i64::MAX / 3)]);
    }

    #[test]
    fn canvas_table_row_and_column_spans_place_only_the_anchor() {
        use designllm_contracts::{CanvasTableRow, CanvasTableSpan, Page};

        let cell = |id: &str, text: &str| Node {
            id: id.into(), kind: NodeKind::Rectangle, bounds: Rect { x: 0, y: 0, width: 200_000, height: 36 },
            text: text.into(), style: Style::default(), asset: None, children: vec![],
            layout: Some(LayoutSpec { width_mode: "fill".into(), height_mode: "hug".into(), ..Default::default() }),
            interactions: vec![], variants: BTreeMap::new(), ext: Default::default(),
        };
        let table = Node {
            id: "table".into(), kind: NodeKind::Group, bounds: Rect { x: 0, y: 0, width: 600_000, height: 72 },
            text: String::new(), style: Style::default(), asset: None,
            children: vec![cell("c11", "A"), cell("c12", ""), cell("c13", "B"), cell("c21", ""), cell("c22", ""), cell("c23", "C")],
            layout: Some(LayoutSpec { mode: "grid".into(), height_mode: "hug".into(), grid_columns: Some(3), gap: 0, row_gap: Some(0), ..Default::default() }),
            interactions: vec![], variants: Default::default(),
            ext: designllm_contracts::NodeExt { table: Some(CanvasTable {
                rows: vec![
                    CanvasTableRow { id: "r1".into(), cell_ids: vec!["c11".into(), "c12".into(), "c13".into()] },
                    CanvasTableRow { id: "r2".into(), cell_ids: vec!["c21".into(), "c22".into(), "c23".into()] },
                ],
                header_row_id: None, column_widths: None,
                cell_spans: [("c11".to_owned(), CanvasTableSpan { row_span: 2, column_span: 2 })].into_iter().collect(),
            }), ..Default::default() },
        };
        let document = CanvasDocument {
            pages: vec![Page { nodes: vec![table], ..Page::new("p", Size::slide()) }],
            ..Default::default()
        };
        let output = layout_pages(&document, &UnitRenderer, None).expect("merged table layout");
        let anchor = output.frames.get("c11").expect("anchor cell is laid out");
        let merged_table = output.frames.get("table").expect("table group is laid out");
        assert_eq!((anchor.width, anchor.height), (400_000, 2400));
        let mut unmerged = document.clone();
        unmerged.pages[0].nodes[0].ext.table.as_mut().unwrap().cell_spans.clear();
        let unmerged_output = layout_pages(&unmerged, &UnitRenderer, None).expect("the same table without spans lays out");
        let unmerged_table = unmerged_output.frames.get("table").expect("unmerged table group is laid out");
        assert_eq!(merged_table.height, unmerged_table.height, "rowspan spans the row heights the same input had before merge");
        assert_eq!(unmerged_output.frames.get("c11").unwrap().height, 1200);
        for covered in ["c12", "c21", "c22"] {
            assert!(!output.frames.contains_key(covered), "covered cell {covered} is not rendered or hit-tested");
        }
    }

    /// Task 64-3: an empty table cell hugs the intrinsic height it was created with (`bounds.height`), so a new table of empty
    /// cells has its rows. The cells used to measure 0 as empty containers, and the table had no height.
    #[test]
    fn empty_canvas_table_cells_keep_their_intrinsic_row_height() {
        use designllm_contracts::{CanvasTableRow, Page};

        let row_height = 36 * designllm_contracts::EMU_PER_CSS_PX;
        let cell = |id: &str, text: &str| Node {
            id: id.into(), kind: NodeKind::Rectangle, bounds: Rect { x: 0, y: 0, width: 120 * designllm_contracts::EMU_PER_CSS_PX, height: row_height },
            text: text.into(), style: Style::default(), asset: None, children: vec![],
            layout: Some(LayoutSpec { width_mode: "fill".into(), height_mode: "hug".into(), ..Default::default() }),
            interactions: vec![], variants: BTreeMap::new(), ext: Default::default(),
        };
        let table = Node {
            id: "table".into(), kind: NodeKind::Group, bounds: Rect { x: 0, y: 0, width: 600_000, height: 2 * row_height },
            text: String::new(), style: Style::default(), asset: None,
            children: vec![cell("c11", ""), cell("c12", ""), cell("c21", ""), cell("c22", "A")],
            layout: Some(LayoutSpec { mode: "grid".into(), height_mode: "hug".into(), grid_columns: Some(2), gap: 0, row_gap: Some(0), ..Default::default() }),
            interactions: vec![], variants: Default::default(),
            ext: designllm_contracts::NodeExt { table: Some(CanvasTable {
                rows: vec![
                    CanvasTableRow { id: "r1".into(), cell_ids: vec!["c11".into(), "c12".into()] },
                    CanvasTableRow { id: "r2".into(), cell_ids: vec!["c21".into(), "c22".into()] },
                ],
                header_row_id: None, column_widths: None, cell_spans: Default::default(),
            }), ..Default::default() },
        };
        let document = CanvasDocument {
            pages: vec![Page { nodes: vec![table], ..Page::new("p", Size::slide()) }],
            ..Default::default()
        };
        let output = layout_pages(&document, &UnitRenderer, None).expect("empty table layout");
        for id in ["c11", "c12"] {
            assert_eq!(output.frames.get(id).map(|frame| frame.height), Some(row_height), "empty cell {id} keeps its height");
        }
        assert_eq!(output.frames["c22"].height, row_height, "the populated row keeps its minimum (the cell height as made) with one line (68 item 16)");
        assert_eq!(output.frames["table"].height, 2 * row_height);
    }

    /// A 2 × 2 table of hug cells `cell_height` tall with the given texts (row-major) and spans.
    fn minimum_table(cell_height: i64, texts: [&str; 4], spans: &[(&str, u32, u32)]) -> CanvasDocument {
        use designllm_contracts::{CanvasTableRow, CanvasTableSpan, Page};
        let ids = ["c11", "c12", "c21", "c22"];
        let children = ids.iter().zip(texts).map(|(id, text)| Node {
            id: (*id).into(), kind: NodeKind::Rectangle, bounds: Rect { x: 0, y: 0, width: 200_000, height: cell_height },
            text: text.into(), style: Style::default(), asset: None, children: vec![],
            layout: Some(LayoutSpec { width_mode: "fill".into(), height_mode: "hug".into(), ..Default::default() }),
            interactions: vec![], variants: BTreeMap::new(), ext: Default::default(),
        }).collect();
        let table = Node {
            id: "table".into(), kind: NodeKind::Group, bounds: Rect { x: 0, y: 0, width: 400_000, height: 2 * cell_height },
            text: String::new(), style: Style::default(), asset: None, children,
            layout: Some(LayoutSpec { mode: "grid".into(), height_mode: "hug".into(), grid_columns: Some(2), gap: 0, row_gap: Some(0), ..Default::default() }),
            interactions: vec![], variants: Default::default(),
            ext: designllm_contracts::NodeExt { table: Some(CanvasTable {
                rows: vec![
                    CanvasTableRow { id: "r1".into(), cell_ids: vec!["c11".into(), "c12".into()] },
                    CanvasTableRow { id: "r2".into(), cell_ids: vec!["c21".into(), "c22".into()] },
                ],
                header_row_id: None, column_widths: None,
                cell_spans: spans.iter().map(|(id, row_span, column_span)| ((*id).to_owned(), CanvasTableSpan { row_span: *row_span, column_span: *column_span })).collect(),
            }), ..Default::default() },
        };
        CanvasDocument { pages: vec![Page { nodes: vec![table], ..Page::new("p", Size::slide()) }], ..Default::default() }
    }

    /// 68 item 16: a merged span of one line is as tall as the rows it covers were (their minimums), not one line.
    #[test]
    fn a_one_line_merge_keeps_the_heights_of_the_rows_it_covers() {
        let row = 36 * designllm_contracts::EMU_PER_CSS_PX;
        let across = layout_pages(&minimum_table(row, ["A", "", "", ""], &[("c11", 1, 2)]), &UnitRenderer, None).unwrap();
        assert_eq!((across.frames["c11"].height, across.frames["table"].height), (row, 2 * row), "a 1 × 2 merge keeps its row");
        let down = layout_pages(&minimum_table(row, ["A", "", "", ""], &[("c11", 2, 1)]), &UnitRenderer, None).unwrap();
        assert_eq!((down.frames["c11"].height, down.frames["table"].height), (2 * row, 2 * row), "a 2 × 1 merge keeps both rows");
        let all = layout_pages(&minimum_table(row, ["A", "", "", ""], &[("c11", 2, 2)]), &UnitRenderer, None).unwrap();
        assert_eq!((all.frames["c11"].height, all.frames["table"].height), (2 * row, 2 * row), "a 2 × 2 merge keeps both rows");
    }

    /// 68 item 16: a merged span grows past its rows' minimums only by what its text needs (UnitRenderer: 1,200 EMU a line).
    #[test]
    fn a_merged_span_grows_past_its_rows_minimums_by_its_text_alone() {
        let down = layout_pages(&minimum_table(1_000, ["a\nb\nc", "", "", ""], &[("c11", 2, 1)]), &UnitRenderer, None).unwrap();
        assert_eq!((down.frames["c11"].height, down.frames["table"].height), (3_600, 3_600), "three lines over two 1,000 EMU rows");
        let across = layout_pages(&minimum_table(1_000, ["a\nb", "", "", ""], &[("c11", 1, 2)]), &UnitRenderer, None).unwrap();
        assert_eq!((across.frames["c11"].height, across.frames["table"].height), (2_400, 3_400), "two lines in row 1, row 2 keeps 1,000");
    }

    /// 68 item 16, the other direction: a table whose cells are taller than their text (an existing document's table) opens
    /// at its cells' heights. Before 68 item 16 a row with text hugged its text (one line, 1,200 EMU) below its cells'
    /// height; text taller than the cells still sizes the row as before.
    #[test]
    fn an_existing_table_whose_cells_are_taller_than_their_text_opens_at_the_cell_heights() {
        let tall = 60 * designllm_contracts::EMU_PER_CSS_PX;
        let document = minimum_table(tall, ["A", "B", "", ""], &[]);
        let reopened: CanvasDocument = serde_json::from_str(&serde_json::to_string(&document).unwrap()).unwrap();
        for (name, document) in [("as made", &document), ("saved and reopened", &reopened)] {
            let output = layout_pages(document, &UnitRenderer, None).unwrap();
            assert_eq!(output.frames["c11"].height, tall, "{name}: a one-line row is its cells' height, not 1,200 EMU");
            assert_eq!(output.frames["table"].height, 2 * tall, "{name}");
        }
        assert_eq!(reopened.pages[0].nodes[0].children[0].bounds.height, tall, "layout does not rewrite the stored cell height");
        let short = layout_pages(&minimum_table(1_000, ["a\nb", "", "", ""], &[]), &UnitRenderer, None).unwrap();
        assert_eq!((short.frames["c11"].height, short.frames["table"].height), (2_400, 3_400), "text past the cells' height sizes its row");
    }

    fn populated_span_document(
        page_size: Size,
        table_x: i64,
        table_y: i64,
        table_width: i64,
        table_height: i64,
        table_height_mode: &str,
    ) -> CanvasDocument {
        use designllm_contracts::{CanvasTable, CanvasTableRow, CanvasTableSpan, NodeExt, Page};
        let mut covered_style = Style::default();
        covered_style.color = "#CC0000".into();
        let cell = |id: &str, text: &str, style: Style| Node {
            id: id.into(), kind: NodeKind::Rectangle, bounds: Rect { x: 0, y: 0, width: table_width / 2, height: 36 },
            text: text.into(), style, asset: None, children: vec![],
            layout: Some(LayoutSpec { width_mode: "fill".into(), height_mode: "hug".into(), ..Default::default() }),
            interactions: vec![], variants: BTreeMap::new(), ext: Default::default(),
        };
        let table = Node {
            id: "table".into(), kind: NodeKind::Group,
            bounds: Rect { x: table_x, y: table_y, width: table_width, height: table_height },
            text: String::new(), style: Style::default(), asset: None,
            children: vec![
                cell("c11", "anchor", Style::default()),
                cell("c12", "covered-east", covered_style.clone()),
                cell("c21", "covered-south", Style::default()),
                cell("c22", "covered-corner", Style::default()),
            ],
            layout: Some(LayoutSpec { mode: "grid".into(), height_mode: table_height_mode.into(), grid_columns: Some(2), gap: 0, row_gap: Some(0), ..Default::default() }),
            interactions: vec![], variants: Default::default(),
            ext: NodeExt { table: Some(CanvasTable {
                rows: vec![
                    CanvasTableRow { id: "r1".into(), cell_ids: vec!["c11".into(), "c12".into()] },
                    CanvasTableRow { id: "r2".into(), cell_ids: vec!["c21".into(), "c22".into()] },
                ],
                header_row_id: None, column_widths: None,
                cell_spans: [("c11".into(), CanvasTableSpan { row_span: 2, column_span: 2 })].into_iter().collect(),
            }), ..Default::default() },
        };
        CanvasDocument {
            pages: vec![Page { nodes: vec![table], ..Page::new("p", page_size) }],
            ..Default::default()
        }
    }

    #[test]
    fn private_layout_keeps_populated_covered_text_in_row_major_blocks() {
        let document = populated_span_document(Size::slide(), 0, 0, 400_000, 72, "hug");

        assert!(crate::canvas::model::validate_document(&document).iter().any(|diagnostic| diagnostic.code == "TABLE_SPAN_HIDDEN_DATA"),
            "this private fixture intentionally gives a covered cell a different text color from its anchor");
        assert!(super::super::guard::check_document(&document).is_err(), "covered cells with a different text style remain outside the public allowlist");
        let output = layout_pages(&document, &UnitRenderer, None).expect("private layout path renders its test fixture");
        let default_color = Style::default().color;
        let text_blocks: Vec<(&str, &str, &str)> = output.pages[0].primitives.iter().filter_map(|primitive| match primitive {
            Primitive::Text { node_id, text, style, .. } => Some((node_id.as_str(), text.as_str(), style.color.as_str())),
            _ => None,
        }).collect();
        assert_eq!(text_blocks, vec![
            ("c11", "anchor", default_color.as_str()),
            ("c12", "covered-east", "#CC0000"),
            ("c21", "covered-south", default_color.as_str()),
            ("c22", "covered-corner", default_color.as_str()),
        ]);
        assert_eq!(output.pages[0].primitives.iter().filter(|primitive| matches!(primitive, Primitive::Shape { .. })).count(), 1,
            "the merged span keeps only its anchor shape");
        assert_eq!(output.frames.get("c11").unwrap().height, 4_800, "all four text blocks fit in the span height");
        for covered in ["c12", "c21", "c22"] {
            assert!(output.frames.contains_key(covered), "covered text retains the {covered} source node frame");
            assert!(output.pages[0].source_map.iter().any(|region| {
                region.node_id == covered && region.logical_range.as_ref().is_some_and(|range| range.paragraph_id == covered)
            }), "covered text keeps the {covered} source ID for text ranges");
        }
    }

    #[test]
    fn populated_span_rows_use_full_merged_text_width_without_single_column_overestimation() {
        let px = designllm_contracts::EMU_PER_CSS_PX;
        let mut document = populated_span_document(Size::slide(), 0, 0, 10_000, 72, "hug");
        let table = &mut document.pages[0].nodes[0];
        for cell in &mut table.children {
            cell.style = Style::default();
            cell.bounds.height = 36 * px;
            cell.text.clear();
        }
        table.children[1].text = "1234567890".repeat(1_000);
        table.bounds.height = 72 * px;
        assert!(crate::canvas::model::validate_document(&document).iter().all(|diagnostic| !diagnostic.blocking));

        let merged = layout_pages(&document, &UnitRenderer, None).expect("valid populated span layout");
        let merged_height = merged.frames.get("table").expect("merged table frame").height;
        assert_eq!(merged_height, 1_000 * 1200, "10,000 glyphs need 1,000 lines at the 10-glyph merged width");

        let mut unmerged_document = document;
        unmerged_document.pages[0].nodes[0].ext.table.as_mut().unwrap().cell_spans.clear();
        let unmerged = layout_pages(&unmerged_document, &UnitRenderer, None).expect("unmerged table layout");
        let unmerged_height = unmerged.frames.get("table").expect("unmerged table frame").height;
        assert_eq!(unmerged.frames["c12"].height, 2_000 * 1200, "the same text needs 2,000 lines at the 5-glyph column width");
        assert_eq!(unmerged_height, 2_000 * 1200 + 36 * px, "the unmerged second row is empty");
        assert!(unmerged_height > merged_height, "the unmerged text wraps at the single-column width");
    }

    #[test]
    fn empty_siblings_do_not_enlarge_rows_already_populated_by_text() {
        let mut document = populated_span_document(Size::slide(), 0, 0, 400_000, 72, "hug");
        for cell in &mut document.pages[0].nodes[0].children {
            cell.style = Style::default();
            cell.bounds.height = 36 * designllm_contracts::EMU_PER_CSS_PX;
            cell.text.clear();
        }
        document.pages[0].nodes[0].children[0].text = "A".into();
        let merged = layout_pages(&document, &UnitRenderer, None).unwrap();
        assert_eq!(merged.frames["table"].height, 2 * 36 * designllm_contracts::EMU_PER_CSS_PX, "the one-line span keeps the 36 px minimums of both its rows (68 item 16)");

        let table = &mut document.pages[0].nodes[0];
        table.ext.table.as_mut().unwrap().cell_spans.clear();
        table.children[2].text = "B".into();
        let unmerged = layout_pages(&document, &UnitRenderer, None).unwrap();
        assert_eq!(unmerged.frames["table"].height, 2 * 36 * designllm_contracts::EMU_PER_CSS_PX, "each unmerged row keeps its 36 px minimum with one line (68 item 16)");
    }

    #[test]
    fn wrapped_first_row_does_not_consume_empty_rows_stored_heights() {
        use designllm_contracts::CanvasTableRow;
        let mut doc = populated_span_document(Size::slide(), 0, 0, 15_000, 7_200, "hug");
        let table = &mut doc.pages[0].nodes[0];
        table.layout.as_mut().unwrap().grid_columns = Some(3);
        let model = table.ext.table.as_mut().unwrap();
        model.cell_spans.clear();
        model.rows.clear();
        let mut cell = table.children[0].clone();
        cell.text.clear();
        cell.bounds.height = 2_400;
        table.children.clear();
        for r in 0..3 {
            let mut row = CanvasTableRow { id: format!("r{r}"), cell_ids: Vec::new() };
            for c in 0..3 {
                cell.id = format!("c{r}{c}");
                row.cell_ids.push(cell.id.clone());
                table.children.push(cell.clone());
            }
            model.rows.push(row);
        }
        table.children[0].text = "12345".repeat(10);
        let output = layout_pages(&doc, &UnitRenderer, None).unwrap();
        assert_eq!(output.frames["c00"].height, 12_000);
        assert_eq!(output.frames["c10"].height, 2_400);
        assert_eq!(output.frames["c20"].height, 2_400);
    }

    #[test]
    fn an_empty_row_keeps_its_stored_height_after_text_wraps() {
        let mut document = populated_span_document(Size::slide(), 0, 0, 10_000, 4800, "hug");
        let table = &mut document.pages[0].nodes[0];
        table.ext.table.as_mut().unwrap().cell_spans.clear();
        for cell in &mut table.children {
            cell.text.clear();
            cell.bounds.height = 2400;
        }
        table.children[2].text = "12345678901".into();

        let output = layout_pages(&document, &UnitRenderer, None).unwrap();
        assert_eq!(output.frames["c21"].height, 3600, "eleven glyphs wrap onto three lines");
        assert_eq!(output.frames["c11"].height, 2400, "the empty first row keeps its own height");
    }

    #[test]
    fn populated_span_anchor_vertical_alignment_offsets_the_row_major_text_block() {
        for (align, expected_offset) in [
            (designllm_contracts::VerticalAlign::Middle, 340_500),
            (designllm_contracts::VerticalAlign::Bottom, 681_000),
        ] {
            let px = designllm_contracts::EMU_PER_CSS_PX;
            let mut document = populated_span_document(Size::slide(), 0, 0, 400_000, 72, "hug");
            let table = &mut document.pages[0].nodes[0];
            table.bounds.height = 72 * px;
            for cell in &mut table.children {
                cell.style = Style { vertical_align: Some(align), ..Style::default() };
                cell.bounds.height = 36 * px;
            }
            assert!(crate::canvas::model::validate_document(&document).iter().all(|diagnostic| !diagnostic.blocking));

            let output = layout_pages(&document, &UnitRenderer, None).expect("valid populated span layout");
            let frame = *output.frames.get("c11").expect("merged anchor frame");
            let text: Vec<_> = output.pages[0].primitives.iter().filter_map(|primitive| match primitive {
                Primitive::Text { node_id, bounds, .. } => Some((node_id.as_str(), *bounds)),
                _ => None,
            }).collect();
            assert_eq!(text.len(), 4);
            assert_eq!(text[0].1.y, frame.y + expected_offset, "all text is aligned as one span content block");
            assert_eq!(text.last().unwrap().1.y + text.last().unwrap().1.height, frame.y + expected_offset + 4 * 1200);
            assert_eq!(text.iter().map(|(id, _)| *id).collect::<Vec<_>>(), ["c11", "c12", "c21", "c22"]);
        }
    }

    #[test]
    fn populated_span_fixed_height_parent_reports_overflow_without_dropping_text() {
        let document = populated_span_document(Size { width: 400_000, height: 10_000 }, 0, 0, 400_000, 2_400, "fixed");
        let output = layout_pages(&document, &UnitRenderer, None).expect("fixed parent overflow is reported in layout diagnostics");
        assert!(output.diagnostics.iter().any(|diagnostic| diagnostic.code == "TABLE_SPAN_PARENT_OVERFLOW" && diagnostic.blocking));
        let text_ids: Vec<&str> = output.pages[0].primitives.iter().filter_map(|primitive| match primitive {
            Primitive::Text { node_id, .. } => Some(node_id.as_str()),
            _ => None,
        }).collect();
        assert_eq!(text_ids, vec!["c11", "c12", "c21", "c22"], "fixed parent does not silently drop merged text");
        for covered in ["c12", "c21", "c22"] {
            assert!(output.pages[0].source_map.iter().any(|region| region.node_id == covered && region.logical_range.is_some()));
        }
        let scene = crate::canvas::scene_of(output).expect("scene retains the blocking layout loss");
        assert!(scene.diagnostics.iter().any(|diagnostic| diagnostic.code == "TABLE_SPAN_PARENT_OVERFLOW" && diagnostic.blocking));
    }

    #[test]
    fn populated_span_page_edge_is_reported_and_exact_edge_remains_in_bounds() {
        let size = Size { width: 400_000, height: 4_800 };
        let exact_edge = populated_span_document(size, 0, 0, 400_000, 72, "hug");
        let exact = layout_pages(&exact_edge, &UnitRenderer, None).expect("span ending at page edge lays out");
        let anchor = exact.frames.get("c11").expect("anchor frame");
        assert_eq!(anchor.x + anchor.width, size.width);
        assert_eq!(anchor.y + anchor.height, size.height);
        assert!(!exact.diagnostics.iter().any(|diagnostic| diagnostic.code == "TABLE_SPAN_PAGE_OVERFLOW"));

        let beyond_edge = populated_span_document(size, 0, 1, 400_000, 72, "hug");
        let beyond = layout_pages(&beyond_edge, &UnitRenderer, None).expect("page overflow is visible in diagnostics");
        assert!(beyond.diagnostics.iter().any(|diagnostic| diagnostic.code == "TABLE_SPAN_PAGE_OVERFLOW" && diagnostic.blocking));
        for covered in ["c12", "c21", "c22"] {
            assert!(beyond.pages[0].source_map.iter().any(|region| region.node_id == covered && region.logical_range.is_some()));
        }
    }

    #[test]
    fn populated_span_near_i64_edges_succeeds_at_limit_and_rejects_overflow() {
        let maximum = i64::MAX;
        let wide = populated_span_document(Size { width: maximum, height: 10_000 }, 0, 0, maximum, 72, "hug");
        let wide_output = layout_pages(&wide, &UnitRenderer, None).expect("i64-sized column tracks are split without multiplication overflow");
        assert_eq!(wide_output.frames.get("c11").unwrap().width, maximum);

        let table_width = 400_000;
        let exact_x = maximum - table_width;
        let at_limit = populated_span_document(Size { width: maximum, height: 10_000 }, exact_x, 0, table_width, 72, "hug");
        let exact = layout_pages(&at_limit, &UnitRenderer, None).expect("frame ending at i64::MAX is representable");
        assert_eq!(exact.frames.get("c11").unwrap().x + exact.frames.get("c11").unwrap().width, maximum);

        let beyond_limit = populated_span_document(Size { width: maximum, height: 10_000 }, exact_x + 1, 0, table_width, 72, "hug");
        assert!(layout_pages(&beyond_limit, &UnitRenderer, None).is_err(), "unrepresentable cell frame is rejected instead of wrapping");
    }

    #[test]
    fn grid_plan_checked_sums_reject_i64_overflow() {
        let plan = GridPlan {
            cells: Vec::new(),
            col_x: vec![i64::MAX],
            col_w: vec![1],
            row_h: vec![i64::MAX, 1],
            row_gap: 0,
        };
        assert!(plan.span(0, 1).is_err());
        assert!(plan.row_y(2).is_err());
        assert!(plan.row_span_height(0, 2).is_err());
        assert!(plan.total_height().is_err());
    }

    /// Renderer for unit tests: each grapheme is 1000 EMU wide and each line 1200 EMU tall. Nothing is encoded.
    struct UnitRenderer;

    impl RenderProvider for UnitRenderer {
        fn shape_text(&self, text: &str, _style: &Style) -> DltResult<ShapedText> {
            let count = text.graphemes(true).count() as i64;
            Ok(ShapedText {
                size: designllm_contracts::Size { width: count * 1000, height: 1200 },
                baseline: 1000,
                ascent: 1000,
                descent: 200,
                line_gap: 0,
                cap_height: 700,
                glyphs: vec![],
                caret_x: vec![],
                grapheme_rects: vec![],
                fonts_used: vec![],
                missing_glyphs: vec![],
            })
        }

        fn encode(
            &self,
            _scene: &designllm_contracts::RenderScene,
            _format: &str,
            _profile: &designllm_contracts::RenderProfile,
            _assets: &dyn designllm_contracts::AssetStore,
        ) -> DltResult<designllm_contracts::EncodedOutput> {
            Err(DltError::unsupported("the unit test renderer does not encode"))
        }

        fn font_catalog_fingerprint(&self) -> String {
            String::new()
        }

        fn available_fonts(&self) -> Vec<String> {
            Vec::new()
        }

        fn hit_test(&self, _page: &RenderPage, _point: Point) -> DltResult<Option<designllm_contracts::HitTestResult>> {
            Ok(None)
        }

        fn caret_rect(&self, _page: &RenderPage, _position: &designllm_contracts::TextPosition) -> DltResult<Rect> {
            Err(DltError::unsupported("the unit test renderer has no caret"))
        }

        fn selection_rects(&self, _page: &RenderPage, _range: &TextRange) -> DltResult<Vec<Rect>> {
            Ok(Vec::new())
        }
    }

    #[test]
    fn video_layout_draws_the_extracted_poster_as_an_image_primitive() {
        use designllm_contracts::{AssetRef, Page};
        let source = AssetRef { sha256: "a".repeat(64), mime: "video/mp4".into(), bytes: 16 };
        let poster = AssetRef { sha256: "b".repeat(64), mime: "image/png".into(), bytes: 68 };
        let video = Node {
            id: "video".into(), kind: NodeKind::Video, bounds: Rect { x: 0, y: 0, width: 320_000, height: 180_000 },
            text: String::new(), style: Style::default(), asset: Some(source), children: Vec::new(), layout: None,
            interactions: Vec::new(), variants: BTreeMap::new(),
            ext: designllm_contracts::NodeExt { alt: Some("Clip".into()), video_poster_asset: Some(poster.clone()), ..Default::default() },
        };
        let document = CanvasDocument {
            pages: vec![Page { nodes: vec![video], ..Page::new("p", Size::slide()) }],
            ..Default::default()
        };
        let output = layout_pages(&document, &UnitRenderer, None).expect("video poster layout");
        assert!(output.pages[0].primitives.iter().any(|primitive| matches!(primitive,
            Primitive::Image { node_id, asset, fit: Some(designllm_contracts::ImageFit::Contain), .. }
                if node_id == "video" && asset == &poster
        )), "static render uses the automatically extracted poster and preserves its source node ID");
    }

    #[test]
    fn open_url_links_follow_visible_preorder_and_clip_to_each_page() {
        const PX: i64 = designllm_contracts::EMU_PER_CSS_PX;
        let open = |id: &str, kind, bounds: Rect, url: &str| Node {
            id: id.into(), kind, bounds, text: String::new(), style: Style::default(), asset: None,
            children: vec![], layout: None, interactions: vec![Interaction::OpenUrl { url: url.into() }],
            variants: BTreeMap::new(), ext: Default::default(),
        };
        let child = open("child", NodeKind::Rectangle, Rect { x: 10 * PX, y: 10 * PX, width: 20 * PX, height: 20 * PX }, "https://example.com/child");
        let parent = Node {
            children: vec![child],
            ..open("parent", NodeKind::Group, Rect { x: 80 * PX, y: 0, width: 30 * PX, height: 30 * PX }, "https://example.com/parent")
        };
        let edge = open("edge", NodeKind::Rectangle, Rect { x: -10 * PX, y: 40 * PX, width: 30 * PX, height: 20 * PX }, "https://example.com/edge");
        let mut hidden = open("hidden", NodeKind::Rectangle, Rect { x: 10 * PX, y: 70 * PX, width: 10 * PX, height: 10 * PX }, "https://example.com/hidden");
        hidden.ext.hidden = Some(true);
        let second = open("second", NodeKind::Rectangle, Rect { x: 5 * PX, y: 5 * PX, width: 10 * PX, height: 10 * PX }, "https://example.com/second");
        let size = Size { width: 100 * PX, height: 100 * PX };
        let doc = CanvasDocument {
            pages: vec![
                designllm_contracts::Page {
                    nodes: vec![parent, edge, hidden],
                    ..designllm_contracts::Page::new("p", size)
                },
                designllm_contracts::Page {
                    nodes: vec![second],
                    ..designllm_contracts::Page::new("q", size)
                },
            ],
            styles: BTreeMap::new(),
            ..Default::default()
        };
        let output = layout_pages(&doc, &UnitRenderer, None).unwrap();
        let links = output.links.iter().map(|link| (link.page_id.as_str(), link.node_id.as_str(), link.bounds)).collect::<Vec<_>>();
        assert_eq!(links, vec![
            ("p", "parent", Rect { x: 80 * PX, y: 0, width: 20 * PX, height: 30 * PX }),
            ("p", "child", Rect { x: 90 * PX, y: 10 * PX, width: 10 * PX, height: 20 * PX }),
            ("p", "edge", Rect { x: 0, y: 40 * PX, width: 20 * PX, height: 20 * PX }),
            ("q", "second", Rect { x: 5 * PX, y: 5 * PX, width: 10 * PX, height: 10 * PX }),
        ]);
    }

    #[test]
    fn hug_group_takes_child_extent_plus_padding_and_clamps() {
        let child = |id: &str, w: i64| Node {
            id: id.into(),
            kind: NodeKind::Rectangle,
            bounds: Rect { x: 0, y: 0, width: w, height: 100 },
            text: String::new(),
            style: Style::default(),
            asset: None,
            children: vec![],
            layout: None,
            interactions: vec![],
            variants: BTreeMap::new(),
            ext: Default::default(),
        };
        let group = Node {
            id: "g".into(),
            kind: NodeKind::Group,
            bounds: Rect { x: 0, y: 0, width: 1, height: 1 },
            text: String::new(),
            style: Style::default(),
            asset: None,
            children: vec![child("a", 100), child("b", 50)],
            layout: Some(LayoutSpec {
                mode: "row".into(),
                width_mode: "hug".into(),
                height_mode: "fixed".into(),
                gap: 10,
                padding: 5,
                min_width: Some(300),
                ..Default::default()
            }),
            interactions: vec![],
            variants: BTreeMap::new(),
            ext: Default::default(),
        };
        // Content 100 + 10 + 50 = 160, plus padding 10 = 170, clamped up to min 300.
        let renderer = UnitRenderer;
        let painter = Painter { renderer: &renderer, viewport: 1280, page_id: "p".into(), page_size: Size::a4(), primitives: vec![], regions: vec![], diagnostics: vec![], fonts: &mut FontUse::default(), frames: BTreeMap::new(), links: vec![] };
        assert_eq!(painter.node_size(&group, None, None).expect("size").0, 300);
    }

    #[test]
    fn weighted_column_tracks_use_largest_remainder_and_stable_ties() {
        assert_eq!(weighted_column_tracks(10, &[1, 1, 1]).unwrap(), (vec![0, 4, 7], vec![4, 3, 3]));
        assert_eq!(weighted_column_tracks(11, &[1, 2, 3]).unwrap(), (vec![0, 2, 6], vec![2, 4, 5]));
        assert!(weighted_column_tracks(2, &[1, 1, 1]).is_err());
    }

    #[test]
    fn grapheme_break_rules_split_at_spaces_and_cjk_only() {
        assert!(is_ideograph_or_kana("中"));
        assert!(!is_ideograph_or_kana("한"));
        assert!(!is_ideograph_or_kana("a"));
    }
}
