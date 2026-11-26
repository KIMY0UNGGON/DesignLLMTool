//! Figure layout. Every panel is drawn into one common RenderScene page (page_index 0) in document order.
//! Text is shaped only through the injected RenderProvider. Labels and connectors are checked against
//! each other: candidate label positions are tried first, and a collision that no candidate resolves
//! is a blocking diagnostic. Nothing is silently shortened, shrunk, or dropped. A math node is typeset by math.rs into
//! text lines, rules, and radical paths inside its node box.

use std::collections::BTreeMap;

use designllm_contracts::{
    CommonLegend, CommonLegendPlacement, DltResult, Diagram, Diagnostic, Edge, EMU_PER_MM, EMU_PER_PT, FigureDocument, FigureImage,
    FigurePanel, FontFaceRef, Node, NodeKind, PanelBody, PathCommand, PlotDisplayMode, PlotErrorKind, Point, PlotSpec, Primitive, Rect,
    RenderPage, RenderProvider, ShapedText, SourceRegion, Style, figure_math, json_hash,
};
use serde_json::{Value, json};

use crate::axis::{ResolvedAxis, distance};
use crate::decimal::{Exact, round_div};
use crate::diag::{self, code};
use crate::math::{self, Piece};
use crate::plot::{self, Kind, XVal};

/// Page index of the figure page. The figure is one page; panels are laid out on it.
pub const PAGE_INDEX: u32 = 0;
/// 1 mm.
const PAD: i64 = 36_000;
/// 0.5 mm.
const GAP: i64 = 18_000;
/// 1 mm.
const TICK: i64 = 36_000;
/// 1.5 mm.
const CAP_HALF: i64 = 54_000;
/// 4.5 pt marker diameter.
const MARKER: i64 = 57_150;
/// 10 pt panel label.
const LABEL_PT: i64 = 127_000;
/// Passes allowed when fitting numeric x tick labels into the content box. Each pass only widens a margin.
const X_FIT_PASSES: usize = 8;
/// Series colours: one definition for the renderer and the window that predicts them (output profiles, legend swatches).
pub use designllm_contracts::figure_rules::{legacy_series_colors, series_colors};
/// Model id of the common legend: the source-map owner of its primitives, and the id it reports. No element may use it.
pub const COMMON_LEGEND_ID: &str = "common_legend";
/// Id of the visual review in `changed_ids`. The review has no element of its own, so no element may use it either.
pub const VISUAL_REVIEW_ID: &str = designllm_contracts::figure_rules::VISUAL_REVIEW_ID;

/// The smallest rectangle that holds every panel (the origin and size are 0 when there is no panel).
fn panels_union(doc: &FigureDocument) -> Rect {
    let x0 = doc.panels.iter().map(|p| p.bounds.x).min().unwrap_or(0);
    let y0 = doc.panels.iter().map(|p| p.bounds.y).min().unwrap_or(0);
    let x1 = doc.panels.iter().map(|p| p.bounds.x.saturating_add(p.bounds.width)).max().unwrap_or(0);
    let y1 = doc.panels.iter().map(|p| p.bounds.y.saturating_add(p.bounds.height)).max().unwrap_or(0);
    Rect { x: x0, y: y0, width: x1.saturating_sub(x0), height: y1.saturating_sub(y0) }
}

/// The laid-out figure.
pub struct Built {
    pub page: RenderPage,
    pub metadata: BTreeMap<String, Value>,
    pub diagnostics: Vec<Diagnostic>,
    pub font_fingerprint: String,
}

#[derive(Clone)]
struct Obstacle {
    owner: String,
    rect: Rect,
    /// Fixed text is placed by layout and checked afterwards. Candidate text is checked when placed.
    fixed: bool,
}

#[derive(Clone)]
struct Seg {
    owner: String,
    a: Point,
    b: Point,
}

struct Pending {
    owner: String,
    element: String,
    text: String,
    style: Style,
    candidates: Vec<(i64, i64)>,
    /// Bounds of the panel the label belongs to. A label may not leave it.
    panel: Rect,
    /// Model element (panel or connector) the label belongs to, for the source map.
    model: String,
}

struct Canvas<'a> {
    renderer: &'a dyn RenderProvider,
    shaped: BTreeMap<String, ShapedText>,
    prims: Vec<Primitive>,
    /// Model element (panel, node, or connector id) of each primitive, parallel to `prims`.
    prim_models: Vec<String>,
    /// Model element being drawn. Primitives emitted now belong to it.
    model: String,
    /// One region per valid panel, so a point anywhere in a panel resolves to that panel.
    panel_regions: Vec<SourceRegion>,
    texts: Vec<Obstacle>,
    rects: Vec<Obstacle>,
    segs: Vec<Seg>,
    pending: Vec<Pending>,
    diags: Vec<Diagnostic>,
    fonts: BTreeMap<String, FontFaceRef>,
    panels_meta: Vec<Value>,
    axes_meta: serde_json::Map<String, Value>,
    /// Bounds of the panel being drawn.
    current: Rect,
}

fn overlaps(a: Rect, b: Rect) -> bool {
    a.x < b.x + b.width && b.x < a.x + a.width && a.y < b.y + b.height && b.y < a.y + a.height
}

fn inside(outer: Rect, r: Rect) -> bool {
    r.x >= outer.x && r.y >= outer.y && r.x + r.width <= outer.x + outer.width && r.y + r.height <= outer.y + outer.height
}

/// Segment against the rectangle interior. Rectangle edges are inset by 1 EMU, so touching does not count.
fn seg_hits_rect(a: Point, b: Point, r: Rect) -> bool {
    let (x0, y0, x1, y1) = (r.x + 1, r.y + 1, r.x + r.width - 1, r.y + r.height - 1);
    if x1 < x0 || y1 < y0 {
        return false;
    }
    if a.x.max(b.x) < x0 || a.x.min(b.x) > x1 || a.y.max(b.y) < y0 || a.y.min(b.y) > y1 {
        return false;
    }
    let (dx, dy) = (i128::from(b.x - a.x), i128::from(b.y - a.y));
    let (mut pos, mut neg) = (false, false);
    for (cx, cy) in [(x0, y0), (x1, y0), (x1, y1), (x0, y1)] {
        let side = dx * i128::from(cy - a.y) - dy * i128::from(cx - a.x);
        match side.cmp(&0) {
            std::cmp::Ordering::Greater => pos = true,
            std::cmp::Ordering::Less => neg = true,
            std::cmp::Ordering::Equal => return true,
        }
    }
    pos && neg
}

/// Top offset of text `content` high inside a box `box_high` high. The same rule as the renderer's `text_block_offset`
/// (checked in tests/p1_style.rs): middle centres, bottom sits on the bottom edge, top starts at the top.
fn text_offset(box_high: i64, content: i64, align: designllm_contracts::VerticalAlign) -> i64 {
    use designllm_contracts::VerticalAlign;
    let free = box_high - content;
    match align {
        VerticalAlign::Middle => free.div_euclid(2),
        VerticalAlign::Bottom => free,
        VerticalAlign::Top => 0,
    }
}

fn label_style(style: &Style) -> Style {
    let mut s = style.clone();
    s.bold = true;
    s.font_size = LABEL_PT;
    s
}

fn line_style(style: &Style, color: &str) -> Style {
    let mut s = style.clone();
    s.color = color.to_owned();
    s.stroke = color.to_owned();
    s
}


/// Bounds of a primitive as a source region. A line gets a tolerance of half its stroke width, between 1 pt and
/// 2 mm, so that a connector or an axis can be hit next to the drawn line.
fn region_bounds(p: &Primitive) -> Rect {
    let b = prim_bounds(p);
    match p {
        Primitive::Line { style, .. } => {
            let pad = (style.stroke_width / 2).clamp(EMU_PER_PT, 2 * EMU_PER_MM);
            Rect { x: b.x - pad, y: b.y - pad, width: b.width + 2 * pad, height: b.height + 2 * pad }
        }
        _ => b,
    }
}

fn prim_bounds(p: &Primitive) -> Rect {
    match p {
        // The figure engine emits a Path for a radical sign only (math nodes); its bounds are its box.
        Primitive::Text { bounds, .. } | Primitive::Shape { bounds, .. } | Primitive::Image { bounds, .. } | Primitive::Path { bounds, .. } => *bounds,
        Primitive::Line { from, to, .. } => Rect {
            x: from.x.min(to.x),
            y: from.y.min(to.y),
            width: (from.x - to.x).abs(),
            height: (from.y - to.y).abs(),
        },
    }
}

/// The statistic that the error bars of a plot show, in the words of the spec. The confidence level is the text the
/// user entered. Nothing is derived here: plot.rs blocks a confidence interval that has no level.
fn error_key(kind: PlotErrorKind, level: Option<&str>) -> Option<String> {
    match kind {
        PlotErrorKind::Sd => Some("Error bars: SD".to_owned()),
        PlotErrorKind::Sem => Some("Error bars: SEM".to_owned()),
        PlotErrorKind::Ci => level.map(|l| format!("Error bars: CI, level {l}")),
        PlotErrorKind::Range => Some("Error bars: explicit range".to_owned()),
    }
}

fn axis_json(a: &ResolvedAxis, caption: &str) -> Value {
    json!({
        "scale": a.scale.name(),
        "mode": if a.explicit { "explicit" } else { "auto" },
        "min": a.min.canonical(),
        "max": a.max.canonical(),
        "tick_step": a.tick_step.map(|s| s.canonical()),
        "ticks": a.tick_labels,
        "caption": caption,
    })
}

/// Try the middle first, then the rest of the segment. A short edge may need the label outside its endpoint boxes.
fn edge_label_candidates(p0: Point, p1: Point, endpoints: [Rect; 2], w: i64, h: i64) -> Vec<(i64, i64)> {
    let (dx, dy) = ((p1.x - p0.x) as f64, (p1.y - p0.y) as f64);
    let len = (dx * dx + dy * dy).sqrt();
    if len == 0.0 {
        return Vec::new();
    }
    let (nx, ny) = (-dy / len, dx / len);
    let half = nx.abs() * w as f64 / 2.0 + ny.abs() * h as f64 / 2.0 + GAP as f64;
    let mut out = Vec::new();
    let anchors = [0.5, 0.25, 0.75, 0.0, 1.0].map(|t| (p0.x as f64 + t * dx, p0.y as f64 + t * dy));
    for distance in [half, 2.0 * half, 3.0 * half] {
        for (x, y) in anchors {
            for sign in [1.0, -1.0] {
                let cx = x + sign * nx * distance;
                let cy = y + sign * ny * distance;
                out.push(((cx - w as f64 / 2.0).round() as i64, (cy - h as f64 / 2.0).round() as i64));
            }
        }
    }
    for (x, y) in anchors {
        let (left, top) = (x.round() as i64 - w / 2, y.round() as i64 - h / 2);
        for b in endpoints {
            out.extend([(left, b.y - GAP - h), (left, b.y + b.height + GAP),
                (b.x - GAP - w, top), (b.x + b.width + GAP, top)]);
        }
    }
    out
}

/// Distance from a node center to its boundary along the unit direction (ux, uy).
fn exit_distance(r: Rect, ellipse: bool, ux: f64, uy: f64) -> f64 {
    let (a, b) = (r.width as f64 / 2.0, r.height as f64 / 2.0);
    if ellipse {
        let k = (ux / a).powi(2) + (uy / b).powi(2);
        return if k > 0.0 { 1.0 / k.sqrt() } else { 0.0 };
    }
    let tx = if ux.abs() > 1e-12 { a / ux.abs() } else { f64::INFINITY };
    let ty = if uy.abs() > 1e-12 { b / uy.abs() } else { f64::INFINITY };
    tx.min(ty)
}

fn center(r: Rect) -> (f64, f64) {
    (r.x as f64 + r.width as f64 / 2.0, r.y as f64 + r.height as f64 / 2.0)
}

/// Room that numeric x tick labels need beyond the panel edges `[edge_left, edge_right]` when the plot spans
/// `[pl, pr]`. Returns (left, right) in EMU, zero where the labels fit. Each label is centred on its tick, as drawn.
fn x_tick_overhang(xa: &ResolvedAxis, widths: &[i64], pl: i64, pr: i64, edge_left: i64, edge_right: i64) -> (i64, i64) {
    let pw = pr - pl;
    if pw <= 0 {
        return (0, 0);
    }
    let (mut left, mut right) = (0, 0);
    for (tick, &w) in xa.ticks.iter().zip(widths) {
        let Some(d) = distance(xa, *tick, pw) else { continue };
        let start = pl + d - w / 2;
        left = left.max(edge_left - start);
        right = right.max(start + w - edge_right);
    }
    (left.max(0), right.max(0))
}

impl<'a> Canvas<'a> {
    fn new(renderer: &'a dyn RenderProvider) -> Self {
        Canvas {
            renderer,
            shaped: BTreeMap::new(),
            prims: Vec::new(),
            prim_models: Vec::new(),
            model: String::new(),
            panel_regions: Vec::new(),
            texts: Vec::new(),
            rects: Vec::new(),
            segs: Vec::new(),
            pending: Vec::new(),
            diags: Vec::new(),
            fonts: BTreeMap::new(),
            panels_meta: Vec::new(),
            axes_meta: serde_json::Map::new(),
            current: Rect::default(),
        }
    }

    /// Adds a primitive that belongs to the element being drawn.
    fn push_prim(&mut self, prim: Primitive) {
        self.prim_models.push(self.model.clone());
        self.prims.push(prim);
    }

    fn shape(&mut self, text: &str, style: &Style) -> DltResult<ShapedText> {
        let key = format!("{}\u{1}{}", serde_json::to_string(style).unwrap_or_default(), text);
        if let Some(s) = self.shaped.get(&key) {
            return Ok(s.clone());
        }
        let s = self.renderer.shape_text(text, style)?;
        for f in &s.fonts_used {
            self.fonts.entry(f.key.clone()).or_insert_with(|| f.clone());
        }
        self.shaped.insert(key, s.clone());
        Ok(s)
    }

    fn size(&mut self, text: &str, style: &Style) -> DltResult<(i64, i64)> {
        let s = self.shape(text, style)?;
        Ok((s.size.width, s.size.height))
    }

    fn emit_text(&mut self, owner: &str, text: &str, style: &Style, left: i64, top: i64, fixed: bool) -> DltResult<Rect> {
        let s = self.shape(text, style)?;
        Ok(self.push_text(owner, text.to_owned(), style.clone(), s, Point { x: left, y: top }, fixed))
    }

    /// Adds a shaped line whose box has its top-left corner at `top_left`, and records it as text to check collisions with.
    fn push_text(&mut self, owner: &str, text: String, style: Style, shaped: ShapedText, top_left: Point, fixed: bool) -> Rect {
        let rect = Rect { x: top_left.x, y: top_left.y, width: shaped.size.width, height: shaped.size.height };
        let origin = Point { x: top_left.x, y: top_left.y + shaped.baseline };
        self.push_prim(Primitive::Text { node_id: owner.to_owned(), bounds: rect, origin, text, style, shaped });
        self.texts.push(Obstacle { owner: owner.to_owned(), rect, fixed });
        rect
    }

    fn add_shape(&mut self, owner: &str, rect: Rect, shape: &str, style: &Style) {
        self.push_prim(Primitive::Shape { node_id: owner.to_owned(), bounds: rect, shape: shape.to_owned(), style: style.clone() });
        self.rects.push(Obstacle { owner: owner.to_owned(), rect, fixed: true });
    }

    fn add_line(&mut self, owner: &str, a: Point, b: Point, style: &Style, arrow: bool) {
        self.push_prim(Primitive::Line { node_id: owner.to_owned(), from: a, to: b, style: style.clone(), arrow });
        self.segs.push(Seg { owner: owner.to_owned(), a, b });
    }

    /// Returns the owner of the first obstacle that `rect` touches, ignoring `owner`'s own items.
    fn conflict(&self, rect: Rect, owner: &str) -> Option<String> {
        for o in &self.texts {
            if o.owner != owner && overlaps(o.rect, rect) {
                return Some(o.owner.clone());
            }
        }
        for o in &self.rects {
            if o.owner != owner && overlaps(o.rect, rect) {
                return Some(o.owner.clone());
            }
        }
        for s in &self.segs {
            if s.owner != owner && seg_hits_rect(s.a, s.b, rect) {
                return Some(s.owner.clone());
            }
        }
        None
    }

    fn place_candidates(&mut self, p: Pending) -> DltResult<()> {
        self.model = p.model.clone();
        let (w, h) = self.size(&p.text, &p.style)?;
        for (left, top) in &p.candidates {
            let rect = Rect { x: *left, y: *top, width: w, height: h };
            if inside(p.panel, rect) && self.conflict(rect, &p.owner).is_none() {
                self.emit_text(&p.owner, &p.text, &p.style, *left, *top, false)?;
                return Ok(());
            }
        }
        self.diags.push(diag::blocking(
            code::LABEL_COLLISION,
            Some(p.element.as_str()),
            format!("no room for label \"{}\" inside the panel without an overlap. Move nearby elements or enlarge the panel", p.text),
        ));
        Ok(())
    }

    /// Fixed text is checked against every other obstacle. Candidate text was checked when placed.
    fn check_fixed(&mut self) {
        let texts = self.texts.clone();
        for (i, t) in texts.iter().enumerate() {
            if !t.fixed {
                continue;
            }
            for u in texts.iter().skip(i + 1) {
                if u.fixed && u.owner != t.owner && overlaps(t.rect, u.rect) {
                    self.diags.push(diag::blocking(
                        code::LABEL_COLLISION,
                        Some(t.owner.as_str()),
                        format!("label overlaps label {}", u.owner),
                    ));
                }
            }
            for o in &self.rects {
                if o.owner != t.owner && overlaps(t.rect, o.rect) {
                    self.diags.push(diag::blocking(code::LABEL_COLLISION, Some(t.owner.as_str()), format!("label overlaps shape {}", o.owner)));
                }
            }
            for s in &self.segs {
                if s.owner != t.owner && seg_hits_rect(s.a, s.b, t.rect) {
                    let (c, element) = match s.owner.strip_prefix("edge:") {
                        Some(id) => (code::CONNECTOR_CROSSES_LABEL, id.to_owned()),
                        None => (code::LABEL_COLLISION, s.owner.clone()),
                    };
                    self.diags.push(diag::blocking(c, Some(t.owner.as_str()), format!("label is crossed by {element}")));
                }
            }
        }
    }

    fn draw_panel(&mut self, panel: &FigurePanel, doc: &FigureDocument) -> DltResult<()> {
        let b = panel.bounds;
        self.current = b;
        self.model = panel.id.clone();
        let (kind, editability) = match &panel.body {
            PanelBody::Diagram(_) => ("diagram", "objects"),
            PanelBody::Plot(_) => ("plot", "objects"),
            PanelBody::Image(_) => ("image", "raster"),
        };
        self.panels_meta.push(json!({
            "id": panel.id,
            "label": panel.label,
            "kind": kind,
            "bounds": [b.x, b.y, b.width, b.height],
            "editability": editability,
        }));
        if b.width <= 0 || b.height <= 0 {
            self.diags.push(diag::blocking(code::PANEL_SIZE_INVALID, Some(panel.id.as_str()), "panel size must be positive"));
            return Ok(());
        }
        self.panel_regions.push(SourceRegion { node_id: panel.id.clone(), bounds: b, logical_range: None, grapheme_rects: Vec::new() });
        let ls = label_style(&panel.style);
        let label_probe = if panel.label.is_empty() { "Ag" } else { panel.label.as_str() };
        let label_h = self.size(label_probe, &ls)?.1;
        if !panel.label.is_empty() {
            self.emit_text(&format!("{}/label", panel.id), &panel.label, &ls, b.x + PAD, b.y + PAD, true)?;
        }
        let band = PAD + label_h + GAP;
        let content = Rect { x: b.x + PAD, y: b.y + band, width: b.width - 2 * PAD, height: b.height - band - PAD };
        match &panel.body {
            PanelBody::Image(img) => self.draw_image(panel, img, content),
            PanelBody::Diagram(dg) => self.draw_diagram(panel, dg),
            PanelBody::Plot(spec) => self.draw_plot(panel, spec, doc, content),
        }
    }

    /// Draws the common legend (R07.6) in the margin outside the panels: centred under or over their union, or to its right.
    /// Panels keep their bounds. A legend that does not fit inside the canvas is blocking; nothing is shortened.
    fn draw_common_legend(&mut self, legend: &CommonLegend, doc: &FigureDocument) -> DltResult<()> {
        let owner = COMMON_LEGEND_ID;
        let style = Style { font_size: LABEL_PT, ..Style::default() };
        let previous = std::mem::replace(&mut self.model, owner.to_owned());
        let mut labels = Vec::with_capacity(legend.items.len());
        for item in &legend.items {
            labels.push(self.size(&item.label, &style)?);
        }
        let title = match legend.title.as_deref() {
            Some(text) => Some((text, self.size(text, &style)?)),
            None => None,
        };
        let row_h = labels.iter().map(|s| s.1).chain(title.map(|t| t.1.1)).max().unwrap_or(0);
        let sw = row_h * 6 / 10;
        let vertical = legend.placement == CommonLegendPlacement::Right;
        let (width, height) = if vertical {
            let rows = labels.len() as i64 + i64::from(title.is_some());
            let widest = labels.iter().map(|s| sw + GAP + s.0).chain(title.map(|t| t.1.0)).max().unwrap_or(0);
            (widest, rows * row_h + (rows - 1).max(0) * GAP)
        } else {
            let items: i64 = labels.iter().map(|s| sw + GAP + s.0).sum::<i64>() + (labels.len() as i64 - 1).max(0) * 2 * GAP;
            (title.map_or(0, |t| t.1.0 + 2 * GAP) + items, row_h)
        };
        let (x, y) = if doc.panels.is_empty() {
            (0, 0)
        } else {
            let bbox = panels_union(doc);
            match legend.placement {
                CommonLegendPlacement::Bottom => (bbox.x + (bbox.width - width) / 2, bbox.y + bbox.height + GAP),
                CommonLegendPlacement::Top => (bbox.x + (bbox.width - width) / 2, bbox.y - GAP - height),
                CommonLegendPlacement::Right => (bbox.x + bbox.width + GAP, bbox.y),
            }
        };
        let page = Rect { x: 0, y: 0, width: doc.size.width, height: doc.size.height };
        if !inside(page, Rect { x, y, width, height }) {
            self.diags.push(diag::blocking(
                code::COMMON_LEGEND_OVERFLOW,
                Some(owner),
                "the common legend does not fit in the margin outside the panels, inside the canvas",
            ));
            self.model = previous;
            return Ok(());
        }
        let (mut left, mut top) = (x, y);
        if let Some((text, (tw, th))) = title {
            self.emit_text(&format!("{owner}/title"), text, &style, left, top + (row_h - th) / 2, true)?;
            if vertical {
                top += row_h + GAP;
            } else {
                left += tw + 2 * GAP;
            }
        }
        for (i, item) in legend.items.iter().enumerate() {
            let (lw, lh) = labels[i];
            let swatch = Rect { x: left, y: top + (row_h - sw) / 2, width: sw, height: sw };
            let colour = Style { fill: item.color.clone(), stroke: item.color.clone(), ..Style::default() };
            self.add_shape(&format!("{owner}/swatch{i}"), swatch, "rect", &colour);
            self.emit_text(&format!("{owner}/label{i}"), &item.label, &style, left + sw + GAP, top + (row_h - lh) / 2, true)?;
            if vertical {
                top += row_h + GAP;
            } else {
                left += sw + GAP + lw + 2 * GAP;
            }
        }
        self.model = previous;
        Ok(())
    }

    fn draw_image(&mut self, panel: &FigurePanel, img: &FigureImage, content: Rect) -> DltResult<()> {
        let pid = panel.id.as_str();
        if content.width <= 0 || content.height <= 0 {
            self.diags.push(diag::blocking(code::PANEL_TOO_SMALL, Some(pid), "image panel is too small for its label"));
            return Ok(());
        }
        // The window drawn is the crop. The renderers stretch it to the box (fit None), so the box takes the window's
        // proportions. An empty or outside crop is refused by validation and is not drawn here; a crop of the whole
        // picture draws the same as no crop.
        let crop = img.crop_pixel_rect.filter(|c| {
            c.width > 0
                && c.height > 0
                && c.x.checked_add(c.width).is_some_and(|right| right <= img.pixel_width)
                && c.y.checked_add(c.height).is_some_and(|bottom| bottom <= img.pixel_height)
                && !(c.x == 0 && c.y == 0 && c.width == img.pixel_width && c.height == img.pixel_height)
        });
        let (pw, ph) = match crop {
            Some(c) => (i128::from(c.width), i128::from(c.height)),
            None => (i128::from(img.pixel_width), i128::from(img.pixel_height)),
        };
        let (w, h) = if i128::from(content.width) * ph >= i128::from(content.height) * pw {
            let h = content.height;
            (round_div(i128::from(h) * pw, ph).unwrap_or(1) as i64, h)
        } else {
            let w = content.width;
            (w, round_div(i128::from(w) * ph, pw).unwrap_or(1) as i64)
        };
        let rect = Rect { x: content.x + (content.width - w) / 2, y: content.y + (content.height - h) / 2, width: w, height: h };
        self.push_prim(Primitive::Image { node_id: panel.id.clone(), bounds: rect, asset: img.asset.clone(), fit: None, image_crop_pixel_rect: crop, style: None });
        // Primitive::Image has no rotation, so a rotated picture is drawn unrotated on every path: a loss.
        if img.rotation_millidegrees != 0 {
            self.diags.push(diag::info(code::IMAGE_ROTATION_NOT_APPLIED, Some(pid), "the rotation is stored but not rendered"));
        }
        self.diags.push(diag::info(code::RASTER_IMAGE_PANEL, Some(pid), "raster image: its contents are not editable as objects"));
        Ok(())
    }

    fn draw_diagram(&mut self, panel: &FigurePanel, dg: &Diagram) -> DltResult<()> {
        // Node bounds are relative to the panel origin.
        let mut boxes: BTreeMap<String, (Rect, bool)> = BTreeMap::new();
        for node in &dg.nodes {
            self.model = node.id.clone();
            self.draw_node(panel, node, &mut boxes)?;
        }
        for edge in &dg.edges {
            self.model = edge.id.clone();
            self.draw_edge(edge, &dg.nodes, &boxes)?;
        }
        self.model = panel.id.clone();
        Ok(())
    }

    fn draw_node(&mut self, panel: &FigurePanel, node: &Node, boxes: &mut BTreeMap<String, (Rect, bool)>) -> DltResult<()> {
        let b = panel.bounds;
        // A text node with math is a formula (guard.rs accepts `ext.math` on diagram text nodes only).
        let math = node.kind == NodeKind::Text && node.ext.math.is_some();
        let ellipse = match node.kind {
            NodeKind::Rectangle => false,
            NodeKind::Ellipse => true,
            _ if math => false,
            _ => {
                self.diags.push(diag::blocking(
                    code::NODE_KIND_UNSUPPORTED,
                    Some(node.id.as_str()),
                    "only rectangle, ellipse and math nodes are supported in figures",
                ));
                return Ok(());
            }
        };
        if node.bounds.width <= 0 || node.bounds.height <= 0 {
            self.diags.push(diag::blocking(code::NODE_SIZE_INVALID, Some(node.id.as_str()), "node size must be positive"));
            return Ok(());
        }
        let abs = Rect { x: b.x + node.bounds.x, y: b.y + node.bounds.y, width: node.bounds.width, height: node.bounds.height };
        if abs.x < b.x || abs.y < b.y || abs.x + abs.width > b.x + b.width || abs.y + abs.height > b.y + b.height {
            self.diags.push(diag::blocking(code::NODE_OUTSIDE_PANEL, Some(node.id.as_str()), "node extends outside its panel"));
            return Ok(());
        }
        if math {
            // A connector meets the formula's node box, as it meets a rectangle.
            boxes.insert(node.id.clone(), (abs, false));
            return self.draw_math(node, abs);
        }
        self.add_shape(&node.id, abs, if ellipse { "ellipse" } else { "rect" }, &node.style);
        boxes.insert(node.id.clone(), (abs, ellipse));
        if !node.text.is_empty() {
            let (tw, th) = self.size(&node.text, &node.style)?;
            if tw > abs.width - 2 * PAD || th > abs.height - 2 * PAD {
                self.diags.push(diag::blocking(
                    code::LABEL_OVERFLOW,
                    Some(node.id.as_str()),
                    format!("label \"{}\" does not fit its node; enlarge the node", node.text),
                ));
            }
            let left = abs.x + (abs.width - tw) / 2;
            // An unset alignment keeps the centred line of earlier versions (the renderer's helper treats unset as top).
            let top = match node.style.vertical_align {
                None => abs.y + (abs.height - th) / 2,
                Some(align) => abs.y + text_offset(abs.height, th, align),
            };
            self.emit_text(&node.id, &node.text, &node.style, left, top, true)?;
        }
        Ok(())
    }

    /// Draws a math node: its source (`Node.text`) parsed by the contracts grammar and typeset by math.rs, centred in the
    /// node box. The node has no frame. A source outside the grammar or over a limit is the parser's blocking diagnostic
    /// on the node, and nothing of it is drawn; a formula larger than its box is blocking as a label is.
    fn draw_math(&mut self, node: &Node, abs: Rect) -> DltResult<()> {
        let items = match figure_math::parse(&node.text) {
            Ok(items) => items,
            Err(refused) => {
                for mut d in refused.diagnostics {
                    d.node_id = Some(node.id.clone());
                    self.diags.push(d);
                }
                return Ok(());
            }
        };
        let formula = {
            let mut measure = |text: &str, style: &Style| self.shape(text, style);
            math::typeset(&items, &node.style, &mut measure)?
        };
        let high = formula.ascent.saturating_add(formula.descent);
        if formula.width > abs.width || high > abs.height {
            // Blocking, so nothing of the figure is output. The formula is not placed, so no coordinate of it can leave the
            // node box (or overflow the collision checks).
            self.diags.push(diag::blocking(
                code::LABEL_OVERFLOW,
                Some(node.id.as_str()),
                format!("formula \"{}\" does not fit its node; enlarge the node", node.text),
            ));
            return Ok(());
        }
        // From here every piece lies inside the node box, which lies inside the panel and the canvas.
        let left = abs.x + (abs.width - formula.width) / 2;
        let baseline = abs.y + (abs.height - high) / 2 + formula.ascent;
        let at = |p: Point| Point { x: left + p.x, y: baseline + p.y };
        for piece in formula.pieces {
            match piece {
                Piece::Text { x, baseline: dy, text, style, shaped } => {
                    let top_left = Point { x: left + x, y: baseline + dy - shaped.baseline };
                    self.push_text(&node.id, text, style, shaped, top_left, true);
                }
                Piece::Rule { from, to, style } => self.add_line(&node.id, at(from), at(to), &style, false),
                Piece::Radical { bounds, path, style } => {
                    let bounds = Rect { x: left + bounds.x, y: baseline + bounds.y, ..bounds };
                    // The sign's strokes are checked against labels as connector segments are.
                    let points: Vec<Point> = path
                        .commands
                        .iter()
                        .filter_map(|c| match *c {
                            PathCommand::MoveTo { x, y } | PathCommand::LineTo { x, y } => Some(Point { x: bounds.x + x, y: bounds.y + y }),
                            PathCommand::CubicTo { .. } | PathCommand::Close => None,
                        })
                        .collect();
                    for pair in points.windows(2) {
                        self.segs.push(Seg { owner: node.id.clone(), a: pair[0], b: pair[1] });
                    }
                    self.push_prim(Primitive::Path { node_id: node.id.clone(), bounds, path, style });
                }
            }
        }
        Ok(())
    }

    fn draw_edge(&mut self, edge: &Edge, nodes: &[Node], boxes: &BTreeMap<String, (Rect, bool)>) -> DltResult<()> {
        if !nodes.iter().any(|n| n.id == edge.from) || !nodes.iter().any(|n| n.id == edge.to) {
            self.diags.push(diag::blocking(code::EDGE_ENDPOINT_NOT_FOUND, Some(edge.id.as_str()), "connector endpoint is not a node of this diagram"));
            return Ok(());
        }
        if edge.from == edge.to {
            self.diags.push(diag::blocking(code::SELF_LOOP_UNSUPPORTED, Some(edge.id.as_str()), "self loops are not supported"));
            return Ok(());
        }
        let (Some(&(ra, ea)), Some(&(rb, eb))) = (boxes.get(&edge.from), boxes.get(&edge.to)) else {
            return Ok(());
        };
        let (ca, cb) = (center(ra), center(rb));
        let (dx, dy) = (cb.0 - ca.0, cb.1 - ca.1);
        let len = (dx * dx + dy * dy).sqrt();
        if len == 0.0 {
            self.diags.push(diag::blocking(code::EDGE_ZERO_LENGTH, Some(edge.id.as_str()), "connector endpoints coincide"));
            return Ok(());
        }
        let (ux, uy) = (dx / len, dy / len);
        let ta = exit_distance(ra, ea, ux, uy);
        let tb = exit_distance(rb, eb, -ux, -uy);
        let p0 = Point { x: (ca.0 + ux * ta).round() as i64, y: (ca.1 + uy * ta).round() as i64 };
        let p1 = Point { x: (cb.0 - ux * tb).round() as i64, y: (cb.1 - uy * tb).round() as i64 };
        for (id, (r, _)) in boxes {
            if *id != edge.from && *id != edge.to && seg_hits_rect(p0, p1, *r) {
                self.diags.push(diag::blocking(
                    code::CONNECTOR_CROSSES_NODE,
                    Some(edge.id.as_str()),
                    format!("connector passes through node {id}; move a node"),
                ));
            }
        }
        self.add_line(&format!("edge:{}", edge.id), p0, p1, &edge.style, true);
        if !edge.label.is_empty() {
            let (w, h) = self.size(&edge.label, &edge.style)?;
            self.pending.push(Pending {
                owner: format!("label:{}", edge.id),
                element: edge.id.clone(),
                text: edge.label.clone(),
                style: edge.style.clone(),
                candidates: edge_label_candidates(p0, p1, [ra, rb], w, h),
                panel: self.current,
                model: edge.id.clone(),
            });
        }
        Ok(())
    }

    fn draw_plot(&mut self, panel: &FigurePanel, spec: &PlotSpec, doc: &FigureDocument, content: Rect) -> DltResult<()> {
        let built = plot::build(doc, panel, spec);
        self.diags.extend(built.diagnostics);
        let Some(data) = built.data else { return Ok(()) };
        let pid = panel.id.as_str();
        let st = panel.style.clone();
        // Every series colour of this plot, computed once for its marks and its legend.
        let colours = series_colors(&st.fill, data.series.len());
        let colour_of = |j: usize| colours.get(j).cloned().unwrap_or_else(|| series_colors(&st.fill, j + 1).pop().unwrap_or_default());
        let axis_style = line_style(&st, &st.stroke.clone());
        let line_h = self.size("Ag", &st)?.1;
        let cap_h = self.size(&data.y_caption, &st)?.1;
        let cap_w = self.size(&data.y_caption, &st)?.0;

        let mut max_tick_w = 0;
        for label in &data.y.tick_labels {
            max_tick_w = max_tick_w.max(self.size(label, &st)?.0);
        }
        let n = data.rows.len() as i64;
        let has_n = !data.x_numeric && data.rows.iter().any(|r| r.n_text.is_some());
        let mut x_tick_widths: Vec<i64> = Vec::new();
        let x_tick_h = match &data.x_axis {
            Some(xa) => {
                let mut h = 0;
                for label in &xa.tick_labels {
                    let (w, lh) = self.size(label, &st)?;
                    x_tick_widths.push(w);
                    h = h.max(lh);
                }
                h
            }
            None => 0,
        };
        let tick_band = if data.x_numeric {
            TICK + GAP + x_tick_h
        } else {
            line_h + if has_n { line_h } else { 0 }
        };
        let x_cap_h = if data.x_caption.is_empty() { 0 } else { self.size(&data.x_caption, &st)?.1 };
        let bottom_reserved = tick_band + if x_cap_h > 0 { GAP + x_cap_h } else { 0 };

        let (x0, y0) = (content.x, content.y);
        let x1 = content.x + content.width;
        let y1 = content.y + content.height;
        let pl0 = x0 + max_tick_w + GAP + TICK;
        // The error key has a row of its own under the axis caption. Only a plot whose display mode is ErrorKey, and that
        // has error bars, reserves that row. Without a mode the plot is drawn as it was before the key existed.
        // A confidence interval without its level never gets here: plot.rs blocks it first.
        let key = match spec.display {
            Some(PlotDisplayMode::ErrorKey) => data.error.and_then(|kind| error_key(kind, data.confidence_level.as_deref())),
            None => None,
        };
        let key_row = match &key {
            Some(text) => self.size(text, &st)?.1 + GAP,
            None => 0,
        };
        // The top tick label is centred on the plot top, so the band reserves half a line above it.
        let pt = y0 + cap_h + GAP + key_row + line_h / 2;
        let pb = y1 - bottom_reserved;
        // Numeric x tick labels are centred on their ticks, so the first and last labels can overhang the plot
        // edges. The room they need is reserved on each side, re-measured on the result, and must end inside the
        // panel, which is the bound every label is checked against.
        let numeric_x = match (&data.x_axis, data.x_numeric) {
            (Some(xa), true) => Some(xa),
            _ => None,
        };
        let (edge_left, edge_right) = (panel.bounds.x, panel.bounds.x + panel.bounds.width);
        let (mut left_reserve, mut right_reserve) = (0, 0);
        if let Some(xa) = numeric_x {
            for _ in 0..X_FIT_PASSES {
                let (lo, ro) = x_tick_overhang(xa, &x_tick_widths, pl0 + left_reserve, x1 - right_reserve, edge_left, edge_right);
                if lo == 0 && ro == 0 {
                    break;
                }
                left_reserve += lo;
                right_reserve += ro;
            }
        }
        let pl = pl0 + left_reserve;
        let pr = x1 - right_reserve;
        let (pw, ph) = (pr - pl, pb - pt);
        if pw <= 0 || ph <= 0 || n == 0 {
            self.diags.push(diag::blocking(code::PANEL_TOO_SMALL, Some(pid), "plot area is too small for its labels"));
            return Ok(());
        }
        if let Some(xa) = numeric_x {
            let (lo, ro) = x_tick_overhang(xa, &x_tick_widths, pl, pr, edge_left, edge_right);
            if lo > 0 || ro > 0 {
                self.diags.push(diag::blocking(
                    code::PANEL_TOO_SMALL,
                    Some(pid),
                    "x tick labels do not fit beside the plot; enlarge the panel",
                ));
            }
        }

        // Positions of rows along x.
        let x_of_row: Vec<Option<i64>> = data
            .rows
            .iter()
            .enumerate()
            .map(|(i, r)| match &r.x {
                XVal::Category(_) => Some(pl + (2 * i as i64 + 1) * pw / (2 * n)),
                XVal::Number(v) => data.x_axis.as_ref().and_then(|xa| distance(xa, *v, pw)).map(|d| pl + d),
                XVal::Missing => None,
            })
            .collect();
        let y_of = |v: Exact| -> Option<i64> { distance(&data.y, v, ph).map(|d| pb - d) };
        let point_of = |i: usize, j: usize| -> Option<Point> {
            let x = x_of_row[i]?;
            let yv = data.rows[i].ys.get(j)?.as_ref()?;
            Some(Point { x, y: y_of(yv.mean)? })
        };

        // Axes, ticks, and fixed labels.
        self.add_line(&format!("{pid}/axis-y"), Point { x: pl, y: pb }, Point { x: pl, y: pt }, &axis_style, false);
        self.add_line(&format!("{pid}/axis-x"), Point { x: pl, y: pb }, Point { x: pr, y: pb }, &axis_style, false);
        self.emit_text(&format!("{pid}/ycap"), &data.y_caption, &st, x0, y0, true)?;
        // Horizontal span of the first numeric x label, which may reach the y label column at the corner.
        let first_x_span: Option<(i64, i64)> = match (numeric_x, x_tick_widths.first()) {
            (Some(xa), Some(&w0)) => xa.ticks.first().and_then(|t| distance(xa, *t, pw)).map(|d| (pl + d - w0 / 2, pl + d - w0 / 2 + w0)),
            _ => None,
        };
        for (k, (t, label)) in data.y.ticks.iter().zip(&data.y.tick_labels).enumerate() {
            let Some(yy) = y_of(*t) else {
                self.diags.push(diag::blocking(code::NUMERIC_RANGE, Some(pid), "tick cannot be placed exactly"));
                continue;
            };
            self.add_line(&format!("{pid}/ytick{k}"), Point { x: pl - TICK, y: yy }, Point { x: pl, y: yy }, &axis_style, false);
            let (w, h) = self.size(label, &st)?;
            // When the first numeric x label overlaps the lowest y label, that y label sits on its tick instead of
            // being centred on it. Otherwise the two labels collide at the corner.
            let y_column = (pl - TICK - GAP - w, pl - TICK - GAP);
            let lowest_on_tick = k == 0
                && first_x_span.is_some_and(|(left, right)| left < y_column.1 && right > y_column.0)
                && yy + h / 2 > pb + TICK + GAP;
            let top = if lowest_on_tick { yy - h } else { yy - h / 2 };
            self.emit_text(&format!("{pid}/ylabel{k}"), label, &st, pl - TICK - GAP - w, top, true)?;
        }
        if let Some(xa) = &data.x_axis {
            for (k, (t, label)) in xa.ticks.iter().zip(&xa.tick_labels).enumerate() {
                let Some(d) = distance(xa, *t, pw) else { continue };
                let xx = pl + d;
                self.add_line(&format!("{pid}/xtick{k}"), Point { x: xx, y: pb }, Point { x: xx, y: pb + TICK }, &axis_style, false);
                let (w, _) = self.size(label, &st)?;
                self.emit_text(&format!("{pid}/xlabel{k}"), label, &st, xx - w / 2, pb + TICK + GAP, true)?;
            }
        } else {
            for (i, r) in data.rows.iter().enumerate() {
                let Some(cx) = x_of_row[i] else { continue };
                let (w, _) = self.size(&r.x_text, &st)?;
                self.emit_text(&format!("{pid}/cat{i}"), &r.x_text, &st, cx - w / 2, pb + GAP, true)?;
                if has_n {
                    if let Some(nt) = &r.n_text {
                        let label = format!("n = {nt}");
                        let (wn, _) = self.size(&label, &st)?;
                        self.emit_text(&format!("{pid}/n{i}"), &label, &st, cx - wn / 2, pb + GAP + line_h, true)?;
                    }
                }
            }
        }
        if x_cap_h > 0 {
            let (w, _) = self.size(&data.x_caption, &st)?;
            let cx = pl + pw / 2;
            self.emit_text(&format!("{pid}/xcap"), &data.x_caption, &st, cx - w / 2, pb + tick_band + GAP, true)?;
        }

        // Bars.
        if data.kind == Kind::Bar {
            let s_count = data.series.len() as i64;
            let base = if data.y.min.is_positive() {
                data.y.min
            } else if data.y.max.is_negative() {
                data.y.max
            } else {
                Exact::ZERO
            };
            if let Some(base_y) = y_of(base) {
                let slot = pw / n;
                let group = slot * 6 / 10;
                let bw = group / s_count.max(1);
                for (i, r) in data.rows.iter().enumerate() {
                    let Some(cx) = x_of_row[i] else { continue };
                    for (j, yv) in r.ys.iter().enumerate() {
                        let Some(v) = yv else { continue };
                        let Some(ym) = y_of(v.mean) else { continue };
                        let h = (base_y - ym).abs();
                        if h == 0 {
                            continue;
                        }
                        let left = cx - group / 2 + j as i64 * bw;
                        let bar_style = Style { fill: colour_of(j), ..st.clone() };
                        self.add_shape(&format!("{pid}/bar{i}/{j}"), Rect { x: left, y: ym.min(base_y), width: bw, height: h }, "rect", &bar_style);
                    }
                }
            }
        }
        // Lines between consecutive present points.
        if data.kind == Kind::Line {
            for j in 0..data.series.len() {
                for i in 1..data.rows.len() {
                    if let (Some(a), Some(b)) = (point_of(i - 1, j), point_of(i, j)) {
                        let style = line_style(&st, &colour_of(j));
                        self.add_line(&format!("{pid}/line{j}/{i}"), a, b, &style, false);
                    }
                }
            }
        }
        // Error bars belong to the single series.
        if data.series.len() == 1 && data.error.is_some() {
            for (i, r) in data.rows.iter().enumerate() {
                let (Some(cx), Some(Some(yv))) = (x_of_row[i], r.ys.first()) else { continue };
                let Some(e) = &yv.err else { continue };
                let (Some(yl), Some(yu)) = (y_of(e.lower), y_of(e.upper)) else { continue };
                self.add_line(&format!("{pid}/err{i}"), Point { x: cx, y: yl }, Point { x: cx, y: yu }, &axis_style, false);
                self.add_line(&format!("{pid}/err{i}/lo"), Point { x: cx - CAP_HALF, y: yl }, Point { x: cx + CAP_HALF, y: yl }, &axis_style, false);
                self.add_line(&format!("{pid}/err{i}/hi"), Point { x: cx - CAP_HALF, y: yu }, Point { x: cx + CAP_HALF, y: yu }, &axis_style, false);
            }
        }
        // Markers for line and scatter points.
        if data.kind != Kind::Bar {
            for j in 0..data.series.len() {
                for i in 0..data.rows.len() {
                    if let Some(p) = point_of(i, j) {
                        let color = colour_of(j);
                        let mk_style = Style { fill: color.clone(), stroke: color.clone(), color: color.clone(), ..st.clone() };
                        let rect = Rect { x: p.x - MARKER / 2, y: p.y - MARKER / 2, width: MARKER, height: MARKER };
                        self.add_shape(&format!("{pid}/mk{j}/{i}"), rect, "ellipse", &mk_style);
                    }
                }
            }
        }
        // Value labels for a single series: the source text of the value and its error.
        if data.series.len() == 1 {
            for (i, r) in data.rows.iter().enumerate() {
                let (Some(cx), Some(Some(yv))) = (x_of_row[i], r.ys.first()) else { continue };
                let Some(ym) = y_of(yv.mean) else { continue };
                let mut label = match &yv.err {
                    Some(e) => e.label.clone(),
                    None => yv.mean_text.clone(),
                };
                if data.x_numeric {
                    if let Some(nt) = &r.n_text {
                        label = format!("{label}, n = {nt}");
                    }
                }
                let (w, h) = self.size(&label, &st)?;
                let (top_a, bot_a) = match &yv.err {
                    Some(e) => match (y_of(e.upper), y_of(e.lower)) {
                        (Some(yu), Some(yl)) => (yu.min(ym), yl.max(ym)),
                        _ => (ym, ym),
                    },
                    None => (ym, ym),
                };
                // The diagonal ones are tried only when the others fail: beside the point, clear of a connector
                // that runs to it from the side (for example the first or last point of a line on a plot edge).
                let candidates = vec![
                    (cx - w / 2, top_a - GAP - h),
                    (cx - w / 2, bot_a + GAP),
                    (cx + CAP_HALF + GAP, ym - h / 2),
                    (cx - CAP_HALF - GAP - w, ym - h / 2),
                    (cx - w / 2, top_a - 2 * (GAP + h)),
                    (cx + CAP_HALF + GAP, ym + GAP),
                    (cx + CAP_HALF + GAP, ym - h - GAP),
                    (cx - CAP_HALF - GAP - w, ym + GAP),
                    (cx - CAP_HALF - GAP - w, ym - h - GAP),
                ];
                self.pending.push(Pending {
                    owner: format!("value:{pid}/{i}"),
                    element: format!("{pid}/{}", r.row_id),
                    text: label,
                    style: st.clone(),
                    candidates,
                    panel: self.current,
                    model: pid.to_owned(),
                });
            }
        }
        // Legend for several series, right-aligned in the top band.
        if data.series.len() > 1 {
            let sw = line_h * 6 / 10;
            let mut widths = Vec::new();
            for name in &data.series {
                widths.push(self.size(name, &st)?.0);
            }
            let total: i64 = widths.iter().map(|w| sw + GAP + w).sum::<i64>() + (data.series.len() as i64 - 1) * 2 * GAP;
            let start = pr - total;
            if start < x0 + cap_w + 2 * GAP {
                self.diags.push(diag::blocking(code::LEGEND_OVERFLOW, Some(pid), "legend does not fit beside the axis caption"));
            } else {
                let mut x = start;
                for (j, name) in data.series.iter().enumerate() {
                    let color = colour_of(j);
                    self.add_shape(&format!("{pid}/swatch{j}"), Rect { x, y: y0 + (line_h - sw) / 2, width: sw, height: sw }, "rect", &Style { fill: color.clone(), stroke: color.clone(), ..st.clone() });
                    self.emit_text(&format!("{pid}/legend{j}"), name, &st, x + sw + GAP, y0, true)?;
                    x += sw + GAP + widths[j] + 2 * GAP;
                }
            }
        }

        // The error key says which statistic the bars show. It is right-aligned to the plot, on the row reserved above.
        // A plot too narrow for it is blocked, not shortened.
        if let Some(text) = &key {
            let (w, _) = self.size(text, &st)?;
            if pr - w < x0 {
                self.diags.push(diag::blocking(code::LEGEND_OVERFLOW, Some(pid), "error key does not fit within the plot width"));
            } else {
                self.emit_text(&format!("{pid}/error-key"), text, &st, pr - w, y0 + cap_h + GAP, true)?;
            }
        }

        self.axes_meta.insert(
            pid.to_owned(),
            json!({
                "y": axis_json(&data.y, &data.y_caption),
                "x": data.x_axis.as_ref().map(|a| axis_json(a, &data.x_caption)),
                "kind": match data.kind { Kind::Bar => "bar", Kind::Line => "line", Kind::Scatter => "scatter" },
                "series": data.series,
            }),
        );
        Ok(())
    }

    fn finish(self, doc: &FigureDocument, figure_id: &str) -> DltResult<Built> {
        let fonts: Vec<&FontFaceRef> = self.fonts.values().collect();
        let font_fingerprint = json_hash(&fonts)?;
        // Each primitive is listed under the element it belongs to (panel, node, or connector), so a hit resolves
        // to an id that the engine can inspect and select. The render crate picks the smallest region that holds
        // the point and, on equal sizes, the first one; the panel regions come last for that reason.
        let mut source_map: Vec<SourceRegion> = self
            .prims
            .iter()
            .zip(&self.prim_models)
            .map(|(p, model)| SourceRegion { node_id: model.clone(), bounds: region_bounds(p), logical_range: None, grapheme_rects: Vec::new() })
            .collect();
        source_map.extend(self.panel_regions.iter().cloned());
        let mut metadata = BTreeMap::new();
        metadata.insert("canvas".to_owned(), json!({"width": doc.size.width, "height": doc.size.height}));
        metadata.insert("panels".to_owned(), Value::Array(self.panels_meta.clone()));
        metadata.insert("resolved_axes".to_owned(), Value::Object(self.axes_meta.clone()));
        metadata.insert("caption".to_owned(), json!(doc.caption));
        metadata.insert("total_pages".to_owned(), json!(1));
        metadata.insert("layout".to_owned(), json!("figure-v1"));
        let page = RenderPage {
            id: format!("{figure_id}#page-{PAGE_INDEX}"),
            page_index: PAGE_INDEX,
            size: doc.size,
            primitives: self.prims,
            source_map,
        };
        Ok(Built { page, metadata, diagnostics: self.diags, font_fingerprint })
    }
}

/// Lays out the figure. Blocking diagnostics are returned inside `Built`; the caller decides what they refuse.
pub fn build(doc: &FigureDocument, figure_id: &str, renderer: &dyn RenderProvider) -> DltResult<Built> {
    let mut cv = Canvas::new(renderer);
    for panel in &doc.panels {
        let first_text = cv.texts.len();
        cv.draw_panel(panel, doc)?;
        // Fixed text of this panel must stay inside it. Candidate text is checked when placed.
        let outside: Vec<(String, Rect)> = cv.texts[first_text..]
            .iter()
            .filter(|t| t.fixed && !inside(panel.bounds, t.rect))
            .map(|t| (t.owner.clone(), t.rect))
            .collect();
        for (owner, _) in outside {
            cv.diags.push(diag::blocking(
                code::LABEL_OUTSIDE_PANEL,
                Some(owner.as_str()),
                format!("label extends beyond panel {}; it would overlap another panel", panel.id),
            ));
        }
    }
    // A legend that validation refuses is not drawn; validation reports it as blocking.
    if let Some(legend) = &doc.common_legend {
        if crate::validate::legend_problems(legend).is_empty() {
            cv.draw_common_legend(legend, doc)?;
        }
    }
    let pending = std::mem::take(&mut cv.pending);
    for p in pending {
        cv.place_candidates(p)?;
    }
    cv.check_fixed();
    cv.finish(doc, figure_id)
}

