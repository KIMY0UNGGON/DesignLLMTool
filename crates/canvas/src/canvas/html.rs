//! HTML export of canvas pages. One self-contained file: CSS in a `<style>` block, image bytes as data URIs,
//! and interactions as declarative JSON in a `<script type="application/json">` block read by a fixed interpreter.
//! OpenUrl keeps an escaped, keyboard-accessible `<a>`; when combined with other actions, the fixed interpreter
//! opens it at its position in the action list. No user code is ever executed.
//!
//! Responsive model: the page element is a size container (`container-type: inline-size`). Breakpoints are
//! `@container (min-width: N px)` queries on the page width, so the HTML matches the scene, where the page width is
//! the viewport. Each node gets one rule per interval between breakpoints of itself and its ancestors.
//! Absolute children use percentages of their parent's declared size. Fill and hug nodes inside an absolute parent
//! use `100%` and `fit-content`, which can differ from the scene when the parent is not fixed-size.
//! Flow containers map to flexbox, grids to CSS grid with equal `minmax(0, 1fr)` columns. Hidden nodes get the `hidden`
//! attribute, so they take no space, as in the scene.
//!
//! Paints follow the renderer: colours carry their opacity as alpha (`rgba`), never CSS `opacity`. An image has no colour,
//! so its opacity is its CSS `opacity`, which also fades its shadow as the renderer does. Paths and lines are SVG
//! with dashes `3w 3w` (dashed) and `1w 1w` (dotted), round or square caps and joins, miter limit 4, and gradients in user
//! space. Shape borders use CSS `border-style`, so the browser sets their dash lengths (close to `3w` and `1w`, fitted to each side).

use super::layout::{absolute_default, effective, list_layout, Align, Eff, Mode, Sizing};
use super::model;
use crate::common::{base64_encode, css_px, escape_html, escape_text_with_breaks, percent, script_json};
use designllm_contracts::{
    AlignSelf, AssetStore, CanvasDocument, CanvasTableSpan, Diagnostic, DltError, DltResult, ErrorCode, FillRule, Gradient, ImageFit, Interaction,
    Justify, LayoutSpec, Node, NodeKind, Page, PathCommand, PathSpec, Point, Rect, RenderProvider, StrokeCap, StrokeDash, StrokeJoin,
    Style, TextListKind, StylePatch,
};
use designllm_render::gradient_line;
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, BTreeSet};

pub(crate) struct HtmlOutput {
    pub bytes: Vec<u8>,
    pub diagnostics: Vec<Diagnostic>,
}

/// What the current node's parent contributes: its layout (None for the page) and its declared size.
#[derive(Clone, Copy)]
struct ParentRef<'a> {
    spec: Option<&'a LayoutSpec>,
    width: i64,
    height: i64,
}

struct Exporter<'a> {
    assets: &'a dyn AssetStore,
    renderer: Option<&'a dyn RenderProvider>,
    next_class: usize,
    base_css: String,
    state_css: String,
    variant_css: String,
    page_css: String,
    fonts: BTreeSet<String>,
    actions: Map<String, Value>,
    diagnostics: Vec<Diagnostic>,
    /// Page and node IDs present in the exported output. Interactions pointing elsewhere are dropped.
    exposed: BTreeSet<String>,
}

fn open_url_attributes(url: &str) -> String {
    format!("href=\"{}\" target=\"_blank\" rel=\"noopener noreferrer\"", escape_html(url))
}

fn has_ordered_open_url(node: &Node, exposed: &BTreeSet<String>) -> bool {
    let has_url = node.interactions.iter().any(|interaction| matches!(interaction, Interaction::OpenUrl { .. }));
    has_url && node.interactions.iter().any(|interaction| match interaction {
        Interaction::OpenUrl { .. } => false,
        Interaction::Back => true,
        Interaction::NavigatePage { page_id } => exposed.contains(page_id),
        Interaction::ToggleVisibility { node_id } | Interaction::SetVariant { node_id, .. } => exposed.contains(node_id),
    })
}

fn sizing_of(spec: Option<&LayoutSpec>, width: bool) -> Sizing {
    let Some(s) = spec else {
        return Sizing::Fixed;
    };
    let mode = if width { s.width_mode.as_str() } else { s.height_mode.as_str() };
    match mode {
        "fill" => Sizing::Fill,
        "hug" => Sizing::Hug,
        _ => Sizing::Fixed,
    }
}

/// `align_self` of a child. None means the parent's `align` applies.
fn item_align(spec: Option<&LayoutSpec>) -> Option<Align> {
    match spec?.align_self? {
        AlignSelf::Auto => None,
        AlignSelf::Start => Some(Align::Start),
        AlignSelf::Center => Some(Align::Center),
        AlignSelf::End => Some(Align::End),
        AlignSelf::Stretch => Some(Align::Stretch),
    }
}

fn align_css(align: Align) -> &'static str {
    match align {
        Align::Start => "flex-start",
        Align::Center => "center",
        Align::End => "flex-end",
        Align::Stretch => "stretch",
    }
}

/// Grid alignment keywords (`start`, not `flex-start`).
fn grid_align_css(align: Align) -> &'static str {
    match align {
        Align::Start => "start",
        Align::Center => "center",
        Align::End => "end",
        Align::Stretch => "stretch",
    }
}

fn justify_css(justify: Justify) -> &'static str {
    match justify {
        Justify::Start => "flex-start",
        Justify::Center => "center",
        Justify::End => "flex-end",
        Justify::SpaceBetween => "space-between",
        Justify::SpaceAround => "space-around",
        Justify::SpaceEvenly => "space-evenly",
    }
}

/// One value when all four sides match, else top, right, bottom, left.
fn padding_css(pads: [i64; 4]) -> String {
    if pads.iter().all(|p| *p == pads[0]) {
        format!("padding:{}px;", css_px(pads[0]))
    } else {
        format!("padding:{}px {}px {}px {}px;", css_px(pads[0]), css_px(pads[1]), css_px(pads[2]), css_px(pads[3]))
    }
}

fn css_string(text: &str) -> String {
    text.replace('\\', "\\\\").replace('"', "\\\"")
}

fn font_weight(style: &Style) -> u16 {
    style.font_weight.unwrap_or(if style.bold { 700 } else { 400 })
}

fn text_decoration(style: &Style) -> &'static str {
    match (style.underline == Some(true), style.strikethrough == Some(true)) {
        (true, true) => "underline line-through",
        (true, false) => "underline",
        (false, true) => "line-through",
        (false, false) => "none",
    }
}

fn font_style(italic: bool) -> &'static str {
    if italic { "italic" } else { "normal" }
}

/// A colour with the opacity (thousandths) multiplied into its alpha. Opacity 1000 or none leaves the colour as written.
fn paint(color: &str, opacity: Option<u32>) -> String {
    let Some(milli) = opacity.filter(|m| *m < 1000) else {
        return color.to_string();
    };
    let Some(hex) = color.strip_prefix('#').filter(|h| h.is_ascii() && (h.len() == 6 || h.len() == 8)) else {
        return color.to_string();
    };
    let channel = |at: usize| u8::from_str_radix(&hex[at..at + 2], 16).ok();
    let (Some(r), Some(g), Some(b)) = (channel(0), channel(2), channel(4)) else {
        return color.to_string();
    };
    let base_alpha = if hex.len() == 8 {
        match channel(6) {
            Some(a) => a,
            None => return color.to_string(),
        }
    } else {
        255
    };
    let alpha = (u32::from(base_alpha) * milli + 500) / 1000;
    format!("rgba({r},{g},{b},{:.3})", f64::from(alpha) / 255.0)
}

/// Fill of a shape: a two-stop linear gradient when one is set, else the fill colour.
fn fill_css(style: &Style) -> String {
    match &style.gradient {
        Some(gradient) => {
            let stops: Vec<String> = gradient
                .stops
                .iter()
                .map(|stop| format!("{} {}%", paint(&stop.color, style.opacity_milli), f64::from(stop.pos_milli) / 10.0))
                .collect();
            format!(
                "background:linear-gradient({}deg, {});",
                f64::from(gradient.angle_millideg) / 1000.0,
                stops.join(", ")
            )
        }
        None => format!("background:{};", paint(&style.fill, style.opacity_milli)),
    }
}

/// Outer shadow (at most one in v1.2): CSS `box-shadow` with blur as the blur radius (Gaussian sigma = blur / 2). The colour
/// takes `opacity` as alpha; pass None when the element already has that opacity as CSS `opacity`.
fn shadow_css(style: &Style, opacity: Option<u32>) -> String {
    if style.shadows.is_empty() {
        return String::new();
    }
    let parts: Vec<String> = style
        .shadows
        .iter()
        .map(|s| {
            format!(
                "{}px {}px {}px {}px {}",
                css_px(s.x),
                css_px(s.y),
                css_px(s.blur),
                css_px(s.spread),
                paint(&s.color, opacity)
            )
        })
        .collect();
    format!("box-shadow:{};", parts.join(", "))
}

/// CSS border style of a shape stroke: dashed and dotted follow the renderer's dash rule in the browser, else solid.
fn border_style(dash: Option<StrokeDash>) -> &'static str {
    match dash {
        Some(StrokeDash::Dashed) => "dashed",
        Some(StrokeDash::Dotted) => "dotted",
        Some(StrokeDash::Solid) | None => "solid",
    }
}

fn has_hangul(text: &str) -> bool {
    text.chars().any(|c| matches!(c as u32, 0x1100..=0x11FF | 0x3130..=0x318F | 0xAC00..=0xD7A3))
}

fn style_decls(style: &Style, kind: NodeKind, text: &str, image_fit: Option<ImageFit>) -> String {
    let op = style.opacity_milli;
    let mut out = format!(
        "box-sizing:border-box;font-family:\"{}\",sans-serif;font-size:{}px;color:{};font-weight:{};font-style:{};text-align:{};",
        css_string(&style.font_family),
        css_px(style.font_size),
        paint(&style.color, op),
        font_weight(style),
        font_style(style.italic),
        if style.align == "justify" { "justify" } else { style.align.as_str() },
    );
    if text_decoration(style) != "none" {
        out.push_str(&format!("text-decoration-line:{};", text_decoration(style)));
    }
    match kind {
        NodeKind::Rectangle => {
            out.push_str(&fill_css(style));
            out.push_str(&format!(
                "border:{}px {} {};",
                css_px(style.stroke_width),
                border_style(style.stroke_dash),
                paint(&style.stroke, op)
            ));
        }
        NodeKind::Ellipse => {
            out.push_str(&fill_css(style));
            out.push_str(&format!(
                "border:{}px {} {};border-radius:50%;",
                css_px(style.stroke_width),
                border_style(style.stroke_dash),
                paint(&style.stroke, op)
            ));
        }
        NodeKind::Image => {
            let fit = match image_fit {
                Some(ImageFit::Contain) => "contain",
                Some(ImageFit::Cover) => "cover",
                Some(ImageFit::Fill) | None => "fill",
            };
            out.push_str(&format!("display:block;object-fit:{fit};"));
            if image_fit == Some(ImageFit::Cover) {
                out.push_str("overflow:hidden;");
            }
            if let Some(milli) = style.opacity_milli.filter(|m| *m < 1000) {
                out.push_str(&format!("opacity:{};", f64::from(milli) / 1000.0));
            }
        }
        NodeKind::Video => {
            out.push_str("display:block;object-fit:contain;");
            if let Some(milli) = style.opacity_milli.filter(|m| *m < 1000) {
                out.push_str(&format!("opacity:{};", f64::from(milli) / 1000.0));
            }
        }
        _ => out.push_str("background:transparent;border:0;"),
    }
    if let Some(radius) = style.corner_radius {
        if kind != NodeKind::Ellipse {
            out.push_str(&format!("border-radius:{}px;", css_px(radius)));
        }
    }
    match kind {
        NodeKind::Rectangle | NodeKind::Ellipse => out.push_str(&shadow_css(style, style.opacity_milli)),
        // The image's opacity is its CSS opacity, which fades its shadow too, so the shadow takes no opacity of its own.
        NodeKind::Image | NodeKind::Video => out.push_str(&shadow_css(style, None)),
        _ => {}
    }
    out.push_str("white-space:pre-wrap;");
    if let Some(milli) = style.line_height_milli {
        out.push_str(&format!("line-height:{};", f64::from(milli) / 1000.0));
    }
    if let Some(spacing) = style.letter_spacing {
        out.push_str(&format!("letter-spacing:{}px;", css_px(spacing)));
    }
    if has_hangul(text) {
        // Hangul breaks only at spaces in the scene, so the browser must not break inside words either.
        out.push_str("word-break:keep-all;overflow-wrap:anywhere;");
    }
    if let Some(vertical) = style.vertical_align {
        let justify = match vertical {
            designllm_contracts::VerticalAlign::Top => "flex-start",
            designllm_contracts::VerticalAlign::Middle => "center",
            designllm_contracts::VerticalAlign::Bottom => "flex-end",
        };
        out.push_str(&format!("display:flex;flex-direction:column;justify-content:{justify};"));
    }
    out
}

fn patch_decls(node: &Node, patch: &StylePatch) -> String {
    let mut effective = node.style.clone();
    patch.apply(&mut effective);
    let mut out = String::new();
    if let Some(family) = &patch.font_family {
        out.push_str(&format!("font-family:\"{}\",sans-serif;", css_string(family)));
    }
    if let Some(size) = patch.font_size {
        out.push_str(&format!("font-size:{}px;", css_px(size)));
    }
    if let Some(color) = &patch.color {
        out.push_str(&format!("color:{color};"));
    }
    if let Some(fill) = &patch.fill {
        out.push_str(&format!("background:{fill};"));
    }
    if let Some(stroke) = &patch.stroke {
        out.push_str(&format!("border-color:{stroke};"));
    }
    if let Some(width) = patch.stroke_width {
        out.push_str(&format!("border-width:{}px;", css_px(width)));
    }
    if patch.bold.is_some() || patch.font_weight.is_some() {
        out.push_str(&format!("font-weight:{};", font_weight(&effective)));
    }
    if patch.underline.is_some() || patch.strikethrough.is_some() {
        out.push_str(&format!("text-decoration-line:{};", text_decoration(&effective)));
    }
    if let Some(italic) = patch.italic {
        out.push_str(&format!("font-style:{};", font_style(italic)));
    }
    if let Some(align) = &patch.align {
        out.push_str(&format!("text-align:{align};"));
    }
    out
}

/// The declarations of one variant rule: the node's style with the variant's patch applied, written in full, so a value the
/// patch clears falls back to its default. The resets first undo the declarations `style_decls` writes only while a value is
/// set, since the base rule may have set them; `style_decls` then writes the rest and wins over the resets. A node with a
/// layout keeps its display, which the layout rule owns, so its vertical alignment is not written here.
fn variant_decls(node: &Node, patch: &StylePatch) -> String {
    let mut effective = node.style.clone();
    patch.apply(&mut effective);
    let mut out = String::new();
    if text_decoration(&node.style) != "none" && text_decoration(&effective) == "none" {
        out.push_str("text-decoration-line:none;");
    }
    if node.kind != NodeKind::Ellipse {
        out.push_str("border-radius:0;");
    }
    out.push_str("box-shadow:none;line-height:normal;letter-spacing:normal;");
    if node.kind == NodeKind::Image {
        out.push_str("opacity:1;");
    }
    if node.layout.is_none() {
        out.push_str("display:block;");
    } else {
        effective.vertical_align = None;
    }
    out.push_str(&style_decls(&effective, node.kind, &node.text, node.ext.image_fit));
    out
}

/// Breakpoint interval condition on the page width. None means the whole range.
fn interval_condition(lo: u32, hi: Option<u32>) -> Option<String> {
    match (lo, hi) {
        (0, None) => None,
        (0, Some(h)) => Some(format!("(max-width: {}px)", f64::from(h) - 0.001)),
        (l, None) => Some(format!("(min-width: {l}px)")),
        (l, Some(h)) => Some(format!("(min-width: {l}px) and (max-width: {}px)", f64::from(h) - 0.001)),
    }
}

/// A grid child's own track: `width` (or `height`) and its alignment property. Stretch and fill take the whole track
/// with `auto` size, so an explicit size never blocks the stretch, as in the scene.
fn grid_track(sizing: Sizing, align: Align, bounds_px: i64, size_prop: &str, self_prop: &str) -> String {
    if align == Align::Stretch || sizing == Sizing::Fill {
        return format!("{size_prop}:auto;{self_prop}:stretch;");
    }
    let size = match sizing {
        Sizing::Hug => "fit-content".to_string(),
        _ => format!("{}px", css_px(bounds_px)),
    };
    format!("{size_prop}:{size};{self_prop}:{};", grid_align_css(align))
}

/// Path data for the scene's path (user units, three decimals at most).
fn path_d(path: &PathSpec) -> String {
    let user = |v: i64| format!("{}", v as f64 / 1000.0);
    let mut pieces = Vec::with_capacity(path.commands.len());
    for command in &path.commands {
        pieces.push(match command {
            PathCommand::MoveTo { x, y } => format!("M{} {}", user(*x), user(*y)),
            PathCommand::LineTo { x, y } => format!("L{} {}", user(*x), user(*y)),
            PathCommand::CubicTo { x1, y1, x2, y2, x, y } => {
                format!("C{} {} {} {} {} {}", user(*x1), user(*y1), user(*x2), user(*y2), user(*x), user(*y))
            }
            PathCommand::Close => "Z".to_string(),
        });
    }
    pieces.join(" ")
}

/// SVG stroke attributes of the renderer's dash rule: dashed `3w 3w`, dotted `1w 1w`, with `w` in CSS px (the stroke is
/// non-scaling, so dash lengths are in screen pixels too).
fn dash_attr(dash: Option<StrokeDash>, width_px: f64) -> String {
    match dash {
        Some(StrokeDash::Dashed) => format!(" stroke-dasharray=\"{0} {0}\"", 3.0 * width_px),
        Some(StrokeDash::Dotted) => format!(" stroke-dasharray=\"{0} {0}\"", width_px),
        Some(StrokeDash::Solid) | None => String::new(),
    }
}

/// Caps, joins, and the miter limit of the renderer (miter limit 4, as SVG's default).
fn cap_join_attrs(cap: Option<StrokeCap>, join: Option<StrokeJoin>) -> String {
    let cap = match cap {
        Some(StrokeCap::Round) => " stroke-linecap=\"round\"",
        Some(StrokeCap::Square) => " stroke-linecap=\"square\"",
        Some(StrokeCap::Butt) | None => "",
    };
    let join = match join {
        Some(StrokeJoin::Round) => " stroke-linejoin=\"round\"",
        Some(StrokeJoin::Bevel) => " stroke-linejoin=\"bevel\"",
        Some(StrokeJoin::Miter) | None => "",
    };
    format!("{cap}{join} stroke-miterlimit=\"4\"")
}

/// A linear gradient in the path's user space (the view box maps onto the element, which is fixed-size for paths). Its
/// line is the renderer's `gradient_line` for the element box. Stops keep the node's opacity as `stop-opacity`.
fn gradient_defs(id: &str, gradient: &Gradient, width: i64, height: i64, view: &Rect, opacity: Option<u32>) -> String {
    let (p1, p2) = gradient_line(Rect { x: 0, y: 0, width, height }, gradient.angle_millideg);
    let to_user = |p: Point| {
        let x = (p.x as f64 * view.width as f64 / width.max(1) as f64 + view.x as f64) / 1000.0;
        let y = (p.y as f64 * view.height as f64 / height.max(1) as f64 + view.y as f64) / 1000.0;
        (x, y)
    };
    let (x1, y1) = to_user(p1);
    let (x2, y2) = to_user(p2);
    let op = opacity.filter(|m| *m < 1000).map_or(1.0, |m| f64::from(m) / 1000.0);
    let stops: String = gradient
        .stops
        .iter()
        .map(|stop| format!("<stop offset=\"{}\" stop-color=\"{}\" stop-opacity=\"{op}\"/>", f64::from(stop.pos_milli) / 1000.0, stop.color))
        .collect();
    format!(
        "<defs><linearGradient id=\"{id}\" gradientUnits=\"userSpaceOnUse\" x1=\"{x1}\" y1=\"{y1}\" x2=\"{x2}\" y2=\"{y2}\">{stops}</linearGradient></defs>"
    )
}

impl<'a> Exporter<'a> {
    /// Position and own-container rules for one node in one breakpoint interval.
    fn state_decls(&self, node: &Node, parent: ParentRef<'_>, parent_eff: Eff, own: Option<Eff>) -> String {
        let mut out = String::new();
        let own_spec = node.layout.as_ref();
        if parent.spec.is_none() || parent_eff.mode == Mode::Absolute {
            let x = percent(node.bounds.x, parent.width);
            let y = percent(node.bounds.y, parent.height);
            let w = match sizing_of(own_spec, true) {
                Sizing::Fill => "width:100%".to_string(),
                Sizing::Hug => "width:fit-content".to_string(),
                Sizing::Fixed => format!("width:{}%", percent(node.bounds.width, parent.width)),
            };
            let h = match sizing_of(own_spec, false) {
                Sizing::Fill => "height:100%".to_string(),
                Sizing::Hug => "height:fit-content".to_string(),
                Sizing::Fixed => format!("height:{}%", percent(node.bounds.height, parent.height)),
            };
            out.push_str(&format!("position:absolute;left:{x}%;top:{y}%;{w};{h};"));
        } else if parent_eff.mode == Mode::Grid {
            out.push_str("position:relative;");
            let span = own_spec.and_then(|s| s.grid_span).unwrap_or(1).max(1);
            out.push_str(&format!("grid-column:span {span};"));
            let item = item_align(own_spec).unwrap_or(parent_eff.align);
            out.push_str(&grid_track(sizing_of(own_spec, true), item, node.bounds.width, "width", "justify-self"));
            out.push_str(&grid_track(sizing_of(own_spec, false), item, node.bounds.height, "height", "align-self"));
        } else {
            let row = parent_eff.mode == Mode::Row;
            let (main_sizing, cross_sizing, main_px, cross_px) = if row {
                (sizing_of(own_spec, true), sizing_of(own_spec, false), node.bounds.width, node.bounds.height)
            } else {
                (sizing_of(own_spec, false), sizing_of(own_spec, true), node.bounds.height, node.bounds.width)
            };
            out.push_str("position:relative;");
            let (main_prop, main_min) = if row { ("width", "min-width") } else { ("height", "min-height") };
            match main_sizing {
                Sizing::Fill => out.push_str(&format!("flex:1 1 0;{main_min}:0;")),
                Sizing::Hug => out.push_str(&format!("flex:none;{main_prop}:fit-content;")),
                Sizing::Fixed => out.push_str(&format!("flex:none;{main_prop}:{}px;", css_px(main_px))),
            }
            let cross_prop = if row { "height" } else { "width" };
            let item = item_align(own_spec);
            let stretch = match item {
                Some(align) => align == Align::Stretch,
                None => parent_eff.align == Align::Stretch,
            };
            if stretch || cross_sizing == Sizing::Fill {
                out.push_str("align-self:stretch;");
            } else {
                let explicit = item.map(|align| format!("align-self:{};", align_css(align))).unwrap_or_default();
                if cross_sizing == Sizing::Hug {
                    out.push_str(&format!("{explicit}{}{cross_prop}:fit-content;", if item.is_none() { "align-self:flex-start;" } else { "" }));
                } else {
                    out.push_str(&format!("{explicit}{cross_prop}:{}px;", css_px(cross_px)));
                }
            }
        }
        if let Some(eff) = own {
            match eff.mode {
                Mode::Absolute => out.push_str("display:block;"),
                Mode::Row | Mode::Column => {
                    let direction = if eff.mode == Mode::Row { "row" } else { "column" };
                    out.push_str(&format!(
                        "display:flex;flex-direction:{direction};gap:{}px;{}align-items:{};",
                        css_px(eff.gap),
                        padding_css(eff.pads),
                        align_css(eff.align)
                    ));
                    if eff.justify != Justify::Start {
                        out.push_str(&format!("justify-content:{};", justify_css(eff.justify)));
                    }
                }
                Mode::Grid => {
                    out.push_str(&format!(
                        "display:grid;grid-template-columns:repeat({},minmax(0,1fr));column-gap:{}px;row-gap:{}px;{}justify-items:{};align-items:{};",
                        eff.grid_columns,
                        css_px(eff.gap),
                        css_px(eff.row_gap),
                        padding_css(eff.pads),
                        grid_align_css(eff.align),
                        grid_align_css(eff.align)
                    ));
                }
            }
            if let Some(v) = eff.min_w {
                out.push_str(&format!("min-width:{}px;", css_px(v)));
            }
            if let Some(v) = eff.max_w {
                out.push_str(&format!("max-width:{}px;", css_px(v)));
            }
            if let Some(v) = eff.min_h {
                out.push_str(&format!("min-height:{}px;", css_px(v)));
            }
            if let Some(v) = eff.max_h {
                out.push_str(&format!("max-height:{}px;", css_px(v)));
            }
        }
        out
    }

    fn emit(
        &mut self,
        node: &Node,
        parent: ParentRef<'_>,
        ancestors: &[u32],
        ancestor_hidden: bool,
        table_header: Option<bool>,
        table_span: Option<CanvasTableSpan>,
        merged_text_cells: Option<&[&Node]>,
        out: &mut String,
    ) -> DltResult<()> {
        let k = self.next_class;
        self.next_class += 1;
        let class = format!("dlm-n{k}");
        let table_class = node.ext.table.as_ref().map(|_| format!("dlm-table-{k}"));
        let id = escape_html(&node.id);
        let hidden = if node.ext.hidden == Some(true) { " hidden" } else { "" };
        let name = node.ext.name.as_deref().map(|n| format!(" data-dlm-name=\"{}\"", escape_html(n))).unwrap_or_default();
        let alt = node.ext.alt.as_deref().map(escape_html);
        let alt_link_label = node.ext.alt.as_deref().filter(|label| !label.trim().is_empty()).map(escape_html);
        let visible_text_link_label = if matches!(node.kind, NodeKind::Text | NodeKind::Rectangle | NodeKind::Ellipse) && !node.text.trim().is_empty() {
            Some(escape_html(&node.text))
        } else {
            None
        };
        let ordered_open_url = has_ordered_open_url(node, &self.exposed);
        let open_url = node.interactions.iter().find_map(|interaction| match interaction {
            Interaction::OpenUrl { url } => Some(url.as_str()),
            _ => None,
        });
        let container_link = if matches!(node.kind, NodeKind::Group | NodeKind::Rectangle) { open_url } else { None };
        let root_link = if matches!(node.kind, NodeKind::Text | NodeKind::Ellipse | NodeKind::Image) { open_url } else { None };
        let svg_link = if matches!(node.kind, NodeKind::Path | NodeKind::Line) { open_url } else { None };
        let interpret_open_url = open_url.is_some() && ordered_open_url;
        let link_label_value = alt_link_label.clone()
            .or_else(|| visible_text_link_label.clone())
            .or_else(|| node.ext.name.as_deref().filter(|label| !label.trim().is_empty()).map(escape_html));
        let link_label = link_label_value.as_ref().map(|label| format!(" aria-label=\"{label}\"")).unwrap_or_default();
        // Container labels use only their own alt or visible text. Child text may be hidden, duplicated, or
        // unrelated to the link action, so it is not inferred here.
        let container_link_label_value = if container_link.is_some() {
            alt_link_label.clone().or_else(|| (node.kind == NodeKind::Rectangle).then(|| visible_text_link_label.clone()).flatten())
        } else {
            None
        };
        let container_link_label = container_link_label_value
            .as_ref()
            .map(|label| format!(" aria-label=\"{label}\""))
            .unwrap_or_default();
        let has_link_name = if container_link.is_some() {
            container_link_label_value.as_ref().is_some_and(|label| !label.trim().is_empty())
        } else {
            link_label_value.as_ref().is_some_and(|label| !label.trim().is_empty())
        };
        if open_url.is_some() && !ancestor_hidden && node.ext.hidden != Some(true) && !has_link_name {
            self.diagnostics.push(Diagnostic {
                code: "HTML_LINK_ACCESSIBLE_NAME_MISSING".into(),
                message: "URL link has no accessible name; set alternative text or visible text on the linked node".into(),
                node_id: Some(node.id.clone()),
                blocking: false,
                params: Default::default(),
            });
        }

        // Breakpoint intervals of this node and its ancestors, from the page width.
        let own_spec = node.layout.as_ref();
        let mut thresholds: Vec<u32> = ancestors.to_vec();
        if let Some(spec) = own_spec {
            thresholds.extend(spec.breakpoints.iter().map(|b| b.min_width_css_px));
        }
        thresholds.sort_unstable();
        thresholds.dedup();
        let mut starts = vec![0u32];
        starts.extend(thresholds.iter().copied().filter(|t| *t > 0));
        let single = starts.len() == 1;
        for (i, lo) in starts.iter().enumerate() {
            let hi = starts.get(i + 1).copied();
            let parent_eff = parent.spec.map(|s| effective(s, *lo)).unwrap_or_else(absolute_default);
            let own = own_spec.map(|s| effective(s, *lo));
            let decls = if table_header.is_some() { "position:relative;".to_string() } else { self.state_decls(node, parent, parent_eff, own) };
            let rule = match (single, interval_condition(*lo, hi)) {
                (true, _) | (false, None) => format!(".{class} {{ {decls} }}\n"),
                (false, Some(cond)) => format!("@container {cond} {{ .{class} {{ {decls} }} }}\n"),
            };
            self.state_css.push_str(&rule);
        }
        if table_class.is_some() {
            self.state_css.push_str(&format!(".{class} {{ display:block; }}\n"));
        }
        let mut base_decls = style_decls(&node.style, node.kind, &node.text, node.ext.image_fit);
        if table_header.is_some() {
            base_decls.push_str("display:table-cell;position:relative;");
            if let Some(vertical) = node.style.vertical_align {
                base_decls.push_str(&format!("vertical-align:{};", match vertical {
                    designllm_contracts::VerticalAlign::Top => "top",
                    designllm_contracts::VerticalAlign::Middle => "middle",
                    designllm_contracts::VerticalAlign::Bottom => "bottom",
                }));
            }
        }
        if root_link.is_some() && (node.kind != NodeKind::Text || text_decoration(&node.style) == "none") {
            base_decls.push_str("text-decoration-line:none;");
        }
        self.base_css.push_str(&format!(".{class} {{ {base_decls} }}\n"));
        if let Some(table_class) = &table_class {
            self.base_css.push_str(&format!(
                ".{table_class} {{ box-sizing:border-box;width:100%;table-layout:fixed;border-collapse:collapse;border-spacing:0; }}\n.{table_class}>thead>tr>th,.{table_class}>tbody>tr>td {{ box-sizing:border-box;padding:0; }}\n"
            ));
        }
        // A variant without clears keeps its declarations as they were. A variant that clears a field writes its whole effective
        // style (`variant_decls`), so each cleared field falls back to its default.
        for (name, patch) in &node.variants {
            self.variant_css.push_str(&format!(
                ".{class}[data-dlm-variant=\"{name}\"] {{ {} }}\n",
                if patch.clear.is_empty() { patch_decls(node, patch) } else { variant_decls(node, patch) }
            ));
        }
        if !node.text.is_empty() {
            self.fonts.insert(node.style.font_family.clone());
        }

        if !node.interactions.is_empty() {
            let mut list = Vec::new();
            for interaction in &node.interactions {
                let (target, entry) = match interaction {
                    Interaction::NavigatePage { page_id } => (page_id, json!({"do": "navigate", "page": page_id})),
                    Interaction::ToggleVisibility { node_id } => (node_id, json!({"do": "toggle", "node": node_id})),
                    Interaction::SetVariant { node_id, variant } => {
                        (node_id, json!({"do": "variant", "node": node_id, "variant": variant}))
                    }
                    Interaction::OpenUrl { url } => {
                        if interpret_open_url { list.push(json!({"do": "open_url", "url": url})); }
                        continue;
                    }
                    Interaction::Back => {
                        list.push(json!({"do": "back"}));
                        continue;
                    }
                };
                if self.exposed.contains(target) {
                    list.push(entry);
                } else {
                    self.diagnostics.push(Diagnostic {
                        code: "INTERACTION_DROPPED".into(),
                        message: format!("interaction target {target} is not part of the exported output"),
                        node_id: Some(node.id.clone()),
                        blocking: false,
                        params: Default::default(),
                    });
                }
            }
            if !list.is_empty() {
                self.actions.insert(node.id.clone(), Value::Array(list));
            }
        }

        let attrs = format!("class=\"{class}\" data-dlm-id=\"{id}\"{hidden}{name}");
        match node.kind {
            NodeKind::Path => {
                let Some(path) = &node.ext.path else {
                    return Err(DltError::new(ErrorCode::InvalidArgument, format!("path node {} has no path", node.id)));
                };
                let fill_rule = match path.fill_rule {
                    FillRule::Nonzero => "nonzero",
                    FillRule::Evenodd => "evenodd",
                };
                let user = |v: i64| format!("{}", v as f64 / 1000.0);
                let view = &path.view_box;
                let op = node.style.opacity_milli;
                let width_px = node.style.stroke_width as f64 / 9525.0;
                let stroke = if node.style.stroke == "transparent" { "none".to_string() } else { paint(&node.style.stroke, op) };
                let (defs, fill) = match &node.style.gradient {
                    Some(gradient) => {
                        let gid = format!("dlm-g{k}");
                        let defs = gradient_defs(&gid, gradient, node.bounds.width, node.bounds.height, view, op);
                        (defs, format!("url(#{gid})"))
                    }
                    None => (String::new(), paint(&node.style.fill, op)),
                };
                let stroke_attrs = format!("{}{}", dash_attr(node.style.stroke_dash, width_px), cap_join_attrs(node.style.stroke_cap, node.style.stroke_join));
                let aria = alt.as_ref().map(|a| format!(" aria-label=\"{a}\"")).unwrap_or_default();
                let svg_aria = if svg_link.is_some() { "" } else { aria.as_str() };
                out.push_str(&format!(
                    "<svg {attrs} viewBox=\"{} {} {} {}\" preserveAspectRatio=\"none\"{svg_aria}>{defs}",
                    user(view.x), user(view.y), user(view.width), user(view.height),
                ));
                if let Some(url) = svg_link {
                    out.push_str(&format!("<a {} tabindex=\"0\"{link_label}>", open_url_attributes(url)));
                }
                out.push_str(&format!(
                    "<path d=\"{}\" fill-rule=\"{fill_rule}\" fill=\"{fill}\" stroke=\"{stroke}\" stroke-width=\"{}\" vector-effect=\"non-scaling-stroke\"{stroke_attrs}/>",
                    path_d(path), css_px(node.style.stroke_width),
                ));
                if svg_link.is_some() { out.push_str("</a>"); }
                out.push_str("</svg>");
            }
            NodeKind::Group => {
                out.push_str(&format!("<div {attrs}>"));
                if let Some(url) = container_link {
                    out.push_str(&format!("<a {}{container_link_label} style=\"position:absolute;inset:0;z-index:0;color:inherit;text-decoration:none\"></a>", open_url_attributes(url)));
                }
                let mut inner: Vec<u32> = ancestors.to_vec();
                if let Some(spec) = own_spec {
                    inner.extend(spec.breakpoints.iter().map(|b| b.min_width_css_px));
                }
                if let Some(table) = &node.ext.table {
                    let class = table_class.as_deref().expect("table node receives a semantic HTML class");
                    let columns = table.rows.first().map_or(0, |row| row.cell_ids.len());
                    let table_label = alt.as_deref().filter(|label| !label.trim().is_empty())
                        .map(|label| format!(" aria-label=\"{label}\"")).unwrap_or_default();
                    out.push_str(&format!("<table class=\"{class}\" aria-rowcount=\"{}\" aria-colcount=\"{columns}\"{table_label}>", table.rows.len()));
                    if let Some(widths) = table.column_widths.as_ref() {
                        if widths.len() != columns || widths.iter().any(|width| *width == 0) {
                            return Err(DltError::invalid(format!("table {} has invalid column width weights", node.id)));
                        }
                        let total = widths.iter().try_fold(0u64, |sum, width| sum.checked_add(u64::from(*width)))
                            .ok_or_else(|| DltError::invalid(format!("table {} column width weights overflow", node.id)))?;
                        out.push_str("<colgroup>");
                        for width in widths {
                            let percent = f64::from(*width) / total as f64 * 100.0;
                            out.push_str(&format!("<col style=\"width:{percent:.12}%\">"));
                        }
                        out.push_str("</colgroup>");
                    }
                    let cells: BTreeMap<&str, &Node> = node.children.iter().map(|cell| (cell.id.as_str(), cell)).collect();
                    let cell_parent = ParentRef { spec: own_spec, width: node.bounds.width, height: node.bounds.height };
                    let spans = model::table_span_grid(table).map_err(DltError::invalid)?;
                    let emit_row = |row_index: usize, row: &designllm_contracts::CanvasTableRow, header: bool, this: &mut Self, out: &mut String| -> DltResult<()> {
                        out.push_str("<tr>");
                        for (column, cell_id) in row.cell_ids.iter().enumerate() {
                            let position = spans.cells[row_index * columns + column];
                            let super::model::TableCellPosition::Anchor { row_span, column_span } = position else {
                                continue;
                            };
                            let cell = cells.get(cell_id.as_str()).copied()
                                .ok_or_else(|| DltError::invalid(format!("table {} references missing cell {cell_id}", node.id)))?;
                            let table_span = (row_span > 1 || column_span > 1).then_some(CanvasTableSpan {
                                row_span: u32::try_from(row_span).map_err(|_| DltError::invalid("table row span is out of range"))?,
                                column_span: u32::try_from(column_span).map_err(|_| DltError::invalid("table column span is out of range"))?,
                            });
                            let merged_text_cells = if row_span > 1 || column_span > 1 {
                                let mut text_cells = Vec::new();
                                for merged_row in row_index..row_index + row_span {
                                    for merged_column in column..column + column_span {
                                        let merged_id = &table.rows[merged_row].cell_ids[merged_column];
                                        let merged_cell = cells.get(merged_id.as_str()).copied()
                                            .ok_or_else(|| DltError::invalid(format!("table {} references missing cell {merged_id}", node.id)))?;
                                        if !merged_cell.text.is_empty() {
                                            text_cells.push(merged_cell);
                                        }
                                    }
                                }
                                text_cells.iter().any(|text_cell| text_cell.id != cell.id).then_some(text_cells)
                            } else {
                                None
                            };
                            this.emit(
                                cell,
                                cell_parent,
                                &inner,
                                ancestor_hidden || node.ext.hidden == Some(true),
                                Some(header),
                                table_span,
                                merged_text_cells.as_deref(),
                                out,
                            )?;
                        }
                        out.push_str("</tr>");
                        Ok(())
                    };
                    let first_body_row = if table.header_row_id.is_some() {
                        out.push_str("<thead>");
                        emit_row(0, &table.rows[0], true, self, out)?;
                        out.push_str("</thead>");
                        1
                    } else {
                        0
                    };
                    if first_body_row < table.rows.len() {
                        out.push_str("<tbody>");
                        for (row_index, row) in table.rows.iter().enumerate().skip(first_body_row) {
                            emit_row(row_index, row, false, self, out)?;
                        }
                        out.push_str("</tbody>");
                    }
                    out.push_str("</table>");
                } else {
                    let child_parent = ParentRef { spec: own_spec, width: node.bounds.width, height: node.bounds.height };
                    for child in &node.children {
                        self.emit(child, child_parent, &inner, ancestor_hidden || node.ext.hidden == Some(true), None, None, None, out)?;
                    }
                }
                out.push_str("</div>");
            }
            NodeKind::Rectangle => {
                let aria = alt.as_ref().map(|a| format!(" aria-label=\"{a}\"")).unwrap_or_default();
                let (tag, scope) = match table_header {
                    Some(true) => ("th", " scope=\"col\""),
                    Some(false) => ("td", ""),
                    None => ("div", ""),
                };
                let table_span = table_span.map(|span| format!(" rowspan=\"{}\" colspan=\"{}\"", span.row_span, span.column_span)).unwrap_or_default();
                out.push_str(&format!("<{tag} {attrs}{scope}{aria}{table_span}>"));
                if let Some(text_cells) = merged_text_cells {
                    for text_cell in text_cells {
                        if text_cell.id == node.id {
                            out.push_str(&escape_text_with_breaks(&text_cell.text));
                        } else {
                            let declarations = style_decls(&text_cell.style, NodeKind::Text, &text_cell.text, None);
                            self.fonts.insert(text_cell.style.font_family.clone());
                            out.push_str(&format!(
                                "<div data-dlm-id=\"{}\" style=\"{}\">{}</div>",
                                escape_html(&text_cell.id),
                                escape_html(&format!("display:block;{declarations}")),
                                escape_text_with_breaks(&text_cell.text),
                            ));
                        }
                    }
                } else {
                    out.push_str(&escape_text_with_breaks(&node.text));
                }
                if let Some(url) = container_link {
                    out.push_str(&format!("<a {}{container_link_label} style=\"position:absolute;inset:0;z-index:0;color:inherit;text-decoration:none\"></a>", open_url_attributes(url)));
                }
                let mut inner: Vec<u32> = ancestors.to_vec();
                if let Some(spec) = own_spec {
                    inner.extend(spec.breakpoints.iter().map(|b| b.min_width_css_px));
                }
                let child_parent = ParentRef { spec: own_spec, width: node.bounds.width, height: node.bounds.height };
                for child in &node.children {
                    self.emit(child, child_parent, &inner, ancestor_hidden || node.ext.hidden == Some(true), None, None, None, out)?;
                }
                out.push_str(&format!("</{tag}>"));
            }
            NodeKind::Text => {
                let aria = if root_link.is_some() {
                    if let Some(label) = &alt_link_label {
                        format!(" aria-label=\"{label}\"")
                    } else if node.text.trim().is_empty() {
                        link_label.clone()
                    } else {
                        String::new()
                    }
                } else {
                    alt.as_ref().map(|a| format!(" aria-label=\"{a}\"")).unwrap_or_default()
                };
                let root_tag = if root_link.is_some() { "a" } else { "div" };
                let link_attrs = root_link.map(|url| format!(" {}", open_url_attributes(url))).unwrap_or_default();
                match node.ext.list {
                    Some(kind) => {
                        let list_tag = match kind { TextListKind::Bullet => "ul", TextListKind::Numbered => "ol" };
                        let renderer = self.renderer.ok_or_else(|| DltError::unsupported("HTML list output requires the shared text renderer"))?;
                        let paragraphs: Vec<&str> = node.text.split('\n').map(|paragraph| paragraph.strip_suffix('\r').unwrap_or(paragraph)).collect();
                        let layout = list_layout(renderer, &node.style, Some(kind), paragraphs.len())?
                            .ok_or_else(|| DltError::invalid("list layout was not produced for a list text node"))?;
                        let marker_width = css_px(layout.marker_column_width);
                        let marker_gap = css_px(layout.gap);
                        out.push_str(&format!("<{root_tag} {attrs}{aria}{link_attrs}><{list_tag} style=\"margin:0;padding:0;list-style:none;\">"));
                        for (paragraph, marker) in paragraphs.into_iter().zip(&layout.markers) {
                            out.push_str(&format!(
                                "<li style=\"display:grid;grid-template-columns:{marker_width}px minmax(0,1fr);column-gap:{marker_gap}px;list-style:none;align-items:start;\"><span aria-hidden=\"true\" style=\"text-align:right;white-space:pre;\">{}</span><span style=\"min-width:0;\">{}</span></li>",
                                escape_html(&marker.text), escape_text_with_breaks(paragraph)
                            ));
                        }
                        out.push_str(&format!("</{list_tag}></{root_tag}>"));
                    }
                    None => out.push_str(&format!("<{root_tag} {attrs}{aria}{link_attrs}>{}</{root_tag}>", escape_text_with_breaks(&node.text))),
                }
            }
            NodeKind::Ellipse => {
                let aria = if root_link.is_some() {
                    if let Some(label) = &alt_link_label {
                        format!(" aria-label=\"{label}\"")
                    } else if node.text.trim().is_empty() {
                        link_label.clone()
                    } else {
                        String::new()
                    }
                } else {
                    alt.as_ref().map(|a| format!(" aria-label=\"{a}\"")).unwrap_or_default()
                };
                if let Some(url) = root_link {
                    out.push_str(&format!("<a {attrs}{aria} {}>{}</a>", open_url_attributes(url), escape_text_with_breaks(&node.text)));
                } else {
                    out.push_str(&format!("<div {attrs}{aria}>{}</div>", escape_text_with_breaks(&node.text)));
                }
            }
            NodeKind::Line => {
                let svg_aria = if svg_link.is_some() { "" } else { " aria-hidden=\"true\"" };
                let svg_link_open = svg_link
                    .map(|url| format!("<a {} tabindex=\"0\"{link_label}>", open_url_attributes(url)))
                    .unwrap_or_default();
                let svg_link_close = if svg_link.is_some() { "</a>" } else { "" };
                let from = Point { x: 0, y: 0 };
                let to = Point { x: node.bounds.width, y: node.bounds.height };
                let mut line_states = vec![(None, node.style.clone(), designllm_render::line_markers(from, to, &node.style, false))];
                for (index, (_, patch)) in node.variants.iter().enumerate() {
                    let mut effective = node.style.clone();
                    patch.apply(&mut effective);
                    line_states.push((Some(index), effective.clone(), designllm_render::line_markers(from, to, &effective, false)));
                }
                let has_markers = line_states.iter().any(|(_, _, markers)| markers.iter().any(|marker| marker.is_some()));
                if !has_markers {
                    let op = node.style.opacity_milli;
                    let stroke = if node.style.stroke == "transparent" { "none".to_string() } else { paint(&node.style.stroke, op) };
                    let stroke_width = node.style.stroke_width.max(1);
                    let width_px = stroke_width as f64 / 9525.0;
                    let stroke_attrs = format!("{}{}", dash_attr(node.style.stroke_dash, width_px), cap_join_attrs(node.style.stroke_cap, node.style.stroke_join));
                    if node.bounds.width == 0 && node.bounds.height == 0 {
                        return Err(DltError::invalid(format!("line node {} has zero length", node.id)));
                    } else if node.bounds.height == 0 {
                        out.push_str(&format!(
                            "<div {attrs} style=\"overflow:visible\"><svg style=\"position:absolute;left:0;top:0;width:100%;height:1px;overflow:visible\" viewBox=\"0 0 {} 1\" preserveAspectRatio=\"none\"{svg_aria}>{svg_link_open}<line data-node-id=\"{id}\" x1=\"0\" y1=\"0\" x2=\"{}\" y2=\"0\" vector-effect=\"non-scaling-stroke\" stroke=\"{stroke}\" stroke-width=\"{}\"{stroke_attrs}/>{svg_link_close}</svg></div>",
                            node.bounds.width, node.bounds.width, css_px(stroke_width)
                        ));
                    } else if node.bounds.width == 0 {
                        out.push_str(&format!(
                            "<div {attrs} style=\"overflow:visible\"><svg style=\"position:absolute;left:0;top:0;width:1px;height:100%;overflow:visible\" viewBox=\"0 0 1 {}\" preserveAspectRatio=\"none\"{svg_aria}>{svg_link_open}<line data-node-id=\"{id}\" x1=\"0\" y1=\"0\" x2=\"0\" y2=\"{}\" vector-effect=\"non-scaling-stroke\" stroke=\"{stroke}\" stroke-width=\"{}\"{stroke_attrs}/>{svg_link_close}</svg></div>",
                            node.bounds.height, node.bounds.height, css_px(stroke_width)
                        ));
                    } else {
                        out.push_str(&format!(
                            "<svg {attrs} viewBox=\"0 0 100 100\" preserveAspectRatio=\"none\"{svg_aria}>{svg_link_open}<line x1=\"0\" y1=\"0\" x2=\"100\" y2=\"100\" vector-effect=\"non-scaling-stroke\" stroke=\"{stroke}\" stroke-width=\"{}\"{stroke_attrs}/>{svg_link_close}</svg>",
                            css_px(stroke_width)
                        ));
                    }
                } else {
                    let mut min_x = from.x.min(to.x);
                    let mut min_y = from.y.min(to.y);
                    let mut max_x = from.x.max(to.x);
                    let mut max_y = from.y.max(to.y);
                    for (_, _, markers) in &line_states {
                        for marker in markers.iter().flatten() {
                            for point in marker.points {
                                min_x = min_x.min(point.x);
                                min_y = min_y.min(point.y);
                                max_x = max_x.max(point.x);
                                max_y = max_y.max(point.y);
                            }
                        }
                    }
                    let view_width = max_x - min_x;
                    let view_height = max_y - min_y;
                    if view_width <= 0 || view_height <= 0 {
                        let mut error = DltError::unsupported(format!("HTML line-end geometry is degenerate on node {}", node.id));
                        error.affected_ids.push(node.id.clone());
                        return Err(error);
                    }
                    let base_group = format!("dlm-line-{k}-base");
                    for (index, (variant, _)) in node.variants.iter().enumerate() {
                        let variant_group = format!("dlm-line-{k}-v{index}");
                        self.base_css.push_str(&format!(".{variant_group}{{display:none;}}\n"));
                        self.variant_css.push_str(&format!(
                            ".{class}[data-dlm-variant=\"{variant}\"] .{base_group}{{display:none;}}\n.{class}[data-dlm-variant=\"{variant}\"] .{variant_group}{{display:inline;}}\n"
                        ));
                    }
                    let groups_svg = line_states.into_iter().map(|(variant_index, state_style, markers)| {
                        let group_class = variant_index.map_or_else(|| base_group.clone(), |index| format!("dlm-line-{k}-v{index}"));
                        let op = state_style.opacity_milli;
                        let stroke = if state_style.stroke == "transparent" { "none".to_string() } else { paint(&state_style.stroke, op) };
                        let stroke_width = state_style.stroke_width.max(1);
                        let width_px = stroke_width as f64 / 9525.0;
                        let stroke_attrs = format!("{}{}", dash_attr(state_style.stroke_dash, width_px), cap_join_attrs(state_style.stroke_cap, state_style.stroke_join));
                        let marker_svg = markers.into_iter().flatten().map(|marker| {
                            let [tip, left, right] = marker.points;
                            match marker.kind {
                                designllm_contracts::LineEnd::Filled => format!(
                                    "<path data-node-id=\"{id}\" d=\"M{} {}L{} {}L{} {}Z\" fill=\"{stroke}\"/>",
                                    tip.x, tip.y, left.x, left.y, right.x, right.y,
                                ),
                                designllm_contracts::LineEnd::Open => format!(
                                    "<path data-node-id=\"{id}\" d=\"M{} {}L{} {}M{} {}L{} {}\" fill=\"none\" stroke=\"{stroke}\" stroke-width=\"{}\"{stroke_attrs} vector-effect=\"non-scaling-stroke\"/>",
                                    tip.x, tip.y, left.x, left.y, tip.x, tip.y, right.x, right.y, css_px(stroke_width),
                                ),
                            }
                        }).collect::<String>();
                        let line_svg = format!(
                            "<line data-node-id=\"{id}\" x1=\"{}\" y1=\"{}\" x2=\"{}\" y2=\"{}\" vector-effect=\"non-scaling-stroke\" stroke=\"{stroke}\" stroke-width=\"{}\"{stroke_attrs}/>",
                            from.x, from.y, to.x, to.y, css_px(stroke_width),
                        );
                        format!("<g class=\"{group_class}\">{line_svg}{marker_svg}</g>")
                    }).collect::<String>();
                    let width_css = if node.bounds.width > 0 {
                        format!("calc(100% + {}px)", css_px(view_width - node.bounds.width))
                    } else {
                        format!("{}px", css_px(view_width))
                    };
                    let height_css = if node.bounds.height > 0 {
                        format!("calc(100% + {}px)", css_px(view_height - node.bounds.height))
                    } else {
                        format!("{}px", css_px(view_height))
                    };
                    out.push_str(&format!(
                        "<div {attrs} style=\"overflow:visible\"><svg style=\"position:absolute;left:{}px;top:{}px;width:{width_css};height:{height_css};overflow:visible\" viewBox=\"{min_x} {min_y} {view_width} {view_height}\" preserveAspectRatio=\"none\"{svg_aria}>{svg_link_open}{groups_svg}{svg_link_close}</svg></div>",
                        css_px(min_x), css_px(min_y),
                    ));
                }
            }
            NodeKind::Image => {
                let Some(asset) = &node.asset else {
                    return Err(DltError::new(ErrorCode::InvalidArgument, "image node has no asset"));
                };
                let bytes = self.assets.read(asset)?;
                let crop = node.ext.image_crop_pixel_rect;
                let cropped_bytes = crop.map(|rect| cropped_image_png(asset, &bytes, rect, &node.id, self.assets)).transpose()?;
                let output_bytes = cropped_bytes.as_deref().unwrap_or(&bytes);
                let output_mime = if crop.is_some() { "image/png" } else { asset.mime.as_str() };
                let alt_text = alt.unwrap_or_default();
                if let Some(url) = root_link {
                    let fit = match node.ext.image_fit {
                        Some(ImageFit::Contain) => "contain",
                        Some(ImageFit::Cover) => "cover",
                        Some(ImageFit::Fill) | None => "fill",
                    };
                    let width = if sizing_of(own_spec, true) == Sizing::Hug { "auto" } else { "100%" };
                    let height = if sizing_of(own_spec, false) == Sizing::Hug { "auto" } else { "100%" };
                    let orientation = if crop.is_some() { "image-orientation:none;" } else { "" };
                    let image_style = format!("display:block;width:{width};height:{height};object-fit:{fit};border-radius:inherit;{orientation}");
                    out.push_str(&format!(
                        "<a {attrs} {}{link_label}><img alt=\"{alt_text}\" draggable=\"false\" style=\"{image_style}\" src=\"data:{};base64,{}\"></a>",
                        open_url_attributes(url),
                        escape_html(output_mime),
                        base64_encode(output_bytes)
                    ));
                } else {
                    let crop_style = if crop.is_some() { " style=\"image-orientation:none;\"" } else { "" };
                    out.push_str(&format!(
                        "<img {attrs} alt=\"{alt_text}\" draggable=\"false\"{crop_style} src=\"data:{};base64,{}\">",
                        escape_html(output_mime),
                        base64_encode(output_bytes)
                    ));
                }
            }
            NodeKind::Video => {
                let Some(video_asset) = &node.asset else {
                    return Err(DltError::new(ErrorCode::InvalidArgument, "video node has no source asset"));
                };
                let Some(poster_asset) = &node.ext.video_poster_asset else {
                    return Err(DltError::new(ErrorCode::InvalidArgument, "video node has no extracted poster asset"));
                };
                let video_bytes = self.assets.read(video_asset)?;
                let poster_bytes = self.assets.read(poster_asset)?;
                let aria = alt.as_ref().map(|label| format!(" aria-label=\"{label}\"")).unwrap_or_default();
                out.push_str(&format!(
                    "<video {attrs} controls playsinline preload=\"metadata\" poster=\"data:image/png;base64,{}\"{aria}><source src=\"data:{};base64,{}\" type=\"{}\"><p>Video playback is not supported by this browser.</p></video>",
                    base64_encode(&poster_bytes),
                    escape_html(&video_asset.mime),
                    base64_encode(&video_bytes),
                    escape_html(&video_asset.mime),
                ));
            }
        }
        Ok(())
    }
}

fn cropped_image_png(
    asset: &designllm_contracts::AssetRef,
    bytes: &[u8],
    crop: designllm_contracts::PixelRect,
    node_id: &str,
    assets: &dyn AssetStore,
) -> DltResult<Vec<u8>> {
    let (source_width, source_height) = super::ops::image_crop_source_dimensions(asset, assets, node_id)?;
    let (decoded_width, decoded_height, pixels) = designllm_render::decode_image(bytes)?;
    if (decoded_width, decoded_height) != (source_width, source_height) {
        return Err(DltError::new(ErrorCode::IntegrityError, "decoded image dimensions do not match the inspected source dimensions"));
    }
    if !super::ops::pixel_rect_within(crop, decoded_width, decoded_height) {
        return Err(DltError::invalid(format!("image crop for {node_id} is outside the decoded source pixels")));
    }
    let row_bytes = usize::try_from(crop.width.checked_mul(4).ok_or_else(|| DltError::invalid("image crop row size overflows"))?)
        .map_err(|_| DltError::invalid("image crop row size is out of range"))?;
    let output_len = row_bytes.checked_mul(usize::try_from(crop.height).map_err(|_| DltError::invalid("image crop height is out of range"))?)
        .ok_or_else(|| DltError::invalid("image crop buffer size overflows"))?;
    let mut cropped = Vec::with_capacity(output_len);
    for row in 0..crop.height {
        let source_row = u64::from(crop.y.checked_add(row).ok_or_else(|| DltError::invalid("image crop row overflows"))?);
        let start_pixel = source_row.checked_mul(u64::from(decoded_width))
            .and_then(|pixels| pixels.checked_add(u64::from(crop.x)))
            .ok_or_else(|| DltError::invalid("image crop source offset overflows"))?;
        let start = usize::try_from(start_pixel.checked_mul(4).ok_or_else(|| DltError::invalid("image crop byte offset overflows"))?)
            .map_err(|_| DltError::invalid("image crop byte offset is out of range"))?;
        let end = start.checked_add(row_bytes).ok_or_else(|| DltError::invalid("image crop row edge overflows"))?;
        let source = pixels.get(start..end).ok_or_else(|| DltError::new(ErrorCode::IntegrityError, "decoded image buffer does not contain the requested crop row"))?;
        cropped.extend_from_slice(source);
    }
    designllm_render::png_from_rgba(crop.width, crop.height, &cropped)
}

/// Exports the selected pages (all pages when `selected` is None) as one HTML file.
/// Interactions whose target is not exported are dropped and reported as `INTERACTION_DROPPED`.
pub(crate) fn export_html<'a>(
    doc: &CanvasDocument,
    selected: Option<&[u32]>,
    title: &str,
    assets: &'a dyn AssetStore,
    renderer: Option<&'a dyn RenderProvider>,
) -> DltResult<HtmlOutput> {
    let pages: Vec<(usize, &Page)> = doc
        .pages
        .iter()
        .enumerate()
        .filter(|(i, _)| selected.is_none_or(|list| list.contains(&(*i as u32))))
        .collect();
    let mut exposed: BTreeSet<String> = BTreeSet::new();
    for (_, page) in &pages {
        exposed.insert(page.id.clone());
        for node in model::flatten(&page.nodes) {
            exposed.insert(node.id.clone());
        }
    }
    let mut exporter = Exporter {
        assets,
        renderer,
        next_class: 0,
        base_css: String::new(),
        state_css: String::new(),
        variant_css: String::new(),
        page_css: String::new(),
        fonts: BTreeSet::new(),
        actions: Map::new(),
        diagnostics: Vec::new(),
        exposed,
    };
    let multi = pages.len() > 1;
    let mut body = String::new();
    for (position, (_, page)) in pages.iter().enumerate() {
        let page_class = format!("dlm-p{position}");
        exporter.page_css.push_str(&format!(
            ".{page_class} {{ position:relative;width:100%;max-width:{}px;aspect-ratio:{} / {};margin:0 auto 24px auto;background:#FFFFFF;container-type:inline-size;overflow:hidden; }}\n",
            css_px(page.size.width),
            page.size.width,
            page.size.height
        ));
        let hidden = if multi && position > 0 { " hidden" } else { "" };
        let name = page.name.as_deref().map(|n| format!(" data-dlm-name=\"{}\"", escape_html(n))).unwrap_or_default();
        body.push_str(&format!(
            "<section class=\"dlm-page {page_class}\" data-dlm-page=\"{}\"{hidden}{name}>",
            escape_html(&page.id)
        ));
        let parent = ParentRef { spec: None, width: page.size.width, height: page.size.height };
        for node in &page.nodes {
            exporter.emit(node, parent, &[], false, None, None, None, &mut body)?;
        }
        body.push_str("</section>\n");
        if !page.notes.is_empty() {
            exporter.diagnostics.push(Diagnostic {
                code: "PAGE_NOTES_NOT_EXPORTED".into(),
                message: "page notes are kept in the project file but not written to HTML".into(),
                node_id: Some(page.id.clone()),
                blocking: false,
                params: Default::default(),
            });
        }
    }
    for family in &exporter.fonts {
        exporter.diagnostics.push(Diagnostic {
            code: "FONT_NOT_EMBEDDED".into(),
            message: format!("font family {family} is referenced by name and is not embedded in the HTML"),
            node_id: None,
            blocking: false,
            params: Default::default(),
        });
    }

    let page_ids: Vec<&str> = pages.iter().map(|(_, p)| p.id.as_str()).collect();
    let config = json!({ "pages": page_ids, "actions": Value::Object(exporter.actions.clone()) });
    let config_json = script_json(&config)?;
    let has_interpreted_open_url = exporter.actions.values().any(|actions| {
        actions.as_array().is_some_and(|actions| actions.iter().any(|action| action["do"] == "open_url"))
    });
    let has_back = exporter.actions.values().any(|actions| {
        actions.as_array().is_some_and(|actions| actions.iter().any(|action| action["do"] == "back"))
    });
    let script = if has_interpreted_open_url {
        SCRIPT_WITH_ORDERED_OPEN_URL
    } else if has_back {
        SCRIPT
    } else {
        SCRIPT_LEGACY
    };
    let css = format!(
        "*{{margin:0;}}body{{background:#EEF1F5;font-family:sans-serif;}}\n[hidden]{{display:none !important;}}\n{}{}{}{}",
        exporter.page_css, exporter.base_css, exporter.state_css, exporter.variant_css
    );
    let title = escape_html(title);
    let html = format!(
        "<!doctype html>\n<html>\n<head>\n<meta charset=\"utf-8\">\n<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n<title>{title}</title>\n<style>\n{css}</style>\n</head>\n<body>\n<main class=\"dlm-doc\">\n{body}</main>\n<script type=\"application/json\" id=\"dlm-config\">{config_json}</script>\n<script>\n{script}</script>\n</body>\n</html>\n"
    );
    Ok(HtmlOutput { bytes: html.into_bytes(), diagnostics: exporter.diagnostics })
}

/// Original fixed interpreter for navigate, toggle, and variant. Exports without Back or mixed OpenUrl use it byte-for-byte.
const SCRIPT_LEGACY: &str = r#"(function () {
  var configText = document.getElementById('dlm-config').textContent;
  var config = JSON.parse(configText);
  var pages = Array.prototype.slice.call(document.querySelectorAll('[data-dlm-page]'));
  var nodes = Array.prototype.slice.call(document.querySelectorAll('[data-dlm-id]'));
  function showPage(id) {
    pages.forEach(function (section) {
      section.hidden = section.getAttribute('data-dlm-page') !== id;
    });
  }
  function nodeById(id) {
    for (var i = 0; i < nodes.length; i++) {
      if (nodes[i].getAttribute('data-dlm-id') === id) { return nodes[i]; }
    }
    return null;
  }
  document.addEventListener('click', function (event) {
    var target = event.target && event.target.closest ? event.target.closest('[data-dlm-id]') : null;
    if (!target) { return; }
    var id = target.getAttribute('data-dlm-id');
    if (!Object.prototype.hasOwnProperty.call(config.actions, id)) { return; }
    config.actions[id].forEach(function (action) {
      if (action.do === 'navigate') {
        showPage(action.page);
      } else if (action.do === 'toggle') {
        var toggled = nodeById(action.node);
        if (toggled) { toggled.hidden = !toggled.hidden; }
      } else if (action.do === 'variant') {
        var varied = nodeById(action.node);
        if (varied) { varied.setAttribute('data-dlm-variant', action.variant); }
      }
    });
  });
  if (pages.length > 1 && config.pages.length > 0) { showPage(config.pages[0]); }
})();
"#;

/// Fixed interpreter that adds the Back page-history action.
const SCRIPT: &str = r#"(function () {
  var configText = document.getElementById('dlm-config').textContent;
  var config = JSON.parse(configText);
  var pages = Array.prototype.slice.call(document.querySelectorAll('[data-dlm-page]'));
  var nodes = Array.prototype.slice.call(document.querySelectorAll('[data-dlm-id]'));
  var pageHistory = [];
  var currentPage = config.pages.length > 0 ? config.pages[0] : null;
  function showPage(id, remember) {
    var exists = pages.some(function (section) { return section.getAttribute('data-dlm-page') === id; });
    if (!exists) { return false; }
    if (remember && currentPage !== null && currentPage !== id) { pageHistory.push(currentPage); }
    currentPage = id;
    pages.forEach(function (section) {
      section.hidden = section.getAttribute('data-dlm-page') !== id;
    });
    return true;
  }
  function goBack() {
    if (pageHistory.length === 0) { return; }
    showPage(pageHistory.pop(), false);
  }
  function nodeById(id) {
    for (var i = 0; i < nodes.length; i++) {
      if (nodes[i].getAttribute('data-dlm-id') === id) { return nodes[i]; }
    }
    return null;
  }
  document.addEventListener('click', function (event) {
    var target = event.target && event.target.closest ? event.target.closest('[data-dlm-id]') : null;
    if (!target) { return; }
    var id = target.getAttribute('data-dlm-id');
    if (!Object.prototype.hasOwnProperty.call(config.actions, id)) { return; }
    config.actions[id].forEach(function (action) {
      if (action.do === 'navigate') {
        showPage(action.page, true);
      } else if (action.do === 'toggle') {
        var toggled = nodeById(action.node);
        if (toggled) { toggled.hidden = !toggled.hidden; }
      } else if (action.do === 'variant') {
        var varied = nodeById(action.node);
        if (varied) { varied.setAttribute('data-dlm-variant', action.variant); }
      } else if (action.do === 'back') {
        goBack();
      }
    });
  });
  if (config.pages.length > 0) { showPage(config.pages[0], false); }
})();
"#;

/// Wrapper OpenUrl links use this interpreter for keyboard activation and stored action order. Other exports keep `SCRIPT` above.
const SCRIPT_WITH_ORDERED_OPEN_URL: &str = r#"(function () {
  var configText = document.getElementById('dlm-config').textContent;
  var config = JSON.parse(configText);
  var pages = Array.prototype.slice.call(document.querySelectorAll('[data-dlm-page]'));
  var nodes = Array.prototype.slice.call(document.querySelectorAll('[data-dlm-id]'));
  var pageHistory = [];
  var currentPage = config.pages.length > 0 ? config.pages[0] : null;
  function showPage(id, remember) {
    var exists = pages.some(function (section) { return section.getAttribute('data-dlm-page') === id; });
    if (!exists) { return false; }
    if (remember && currentPage !== null && currentPage !== id) { pageHistory.push(currentPage); }
    currentPage = id;
    pages.forEach(function (section) {
      section.hidden = section.getAttribute('data-dlm-page') !== id;
    });
    return true;
  }
  function goBack() {
    if (pageHistory.length === 0) { return; }
    showPage(pageHistory.pop(), false);
  }
  function nodeById(id) {
    for (var i = 0; i < nodes.length; i++) {
      if (nodes[i].getAttribute('data-dlm-id') === id) { return nodes[i]; }
    }
    return null;
  }
  function runActions(id, event) {
    var actions = config.actions[id];
    if (!Array.isArray(actions)) { return; }
    var hasOrderedUrl = actions.some(function (action) { return action.do === 'open_url'; });
    if (hasOrderedUrl && event) { event.preventDefault(); }
    actions.forEach(function (action) {
      if (action.do === 'open_url') {
        window.open(action.url, '_blank', 'noopener,noreferrer');
      } else if (action.do === 'navigate') {
        showPage(action.page, true);
      } else if (action.do === 'toggle') {
        var toggled = nodeById(action.node);
        if (toggled) { toggled.hidden = !toggled.hidden; }
      } else if (action.do === 'variant') {
        var varied = nodeById(action.node);
        if (varied) { varied.setAttribute('data-dlm-variant', action.variant); }
      } else if (action.do === 'back') {
        goBack();
      }
    });
  }
  document.addEventListener('click', function (event) {
    var target = event.target && event.target.closest ? event.target.closest('[data-dlm-id]') : null;
    if (!target) { return; }
    var id = target.getAttribute('data-dlm-id');
    if (!Object.prototype.hasOwnProperty.call(config.actions, id)) { return; }
    runActions(id, event);
  });
  if (config.pages.length > 0) { showPage(config.pages[0], false); }
})();
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use designllm_contracts::{AssetRef, Size};

    struct EmptyAssets;
    impl AssetStore for EmptyAssets {
        fn put(&self, _: &[u8], _: &str) -> DltResult<AssetRef> { Err(DltError::unsupported("no assets")) }
        fn read(&self, _: &AssetRef) -> DltResult<Vec<u8>> { Err(DltError::unsupported("no assets")) }
        fn lookup(&self, hash: &str) -> DltResult<AssetRef> { Err(DltError::new(ErrorCode::NotFound, hash)) }
    }

    struct OneImage;
    impl AssetStore for OneImage {
        fn put(&self, _: &[u8], _: &str) -> DltResult<AssetRef> { Err(DltError::unsupported("no writes")) }
        fn read(&self, _: &AssetRef) -> DltResult<Vec<u8>> { Ok(vec![0x89]) }
        fn lookup(&self, hash: &str) -> DltResult<AssetRef> { Err(DltError::new(ErrorCode::NotFound, hash)) }
    }

    struct CropImageAsset { reference: AssetRef, bytes: Vec<u8> }
    impl AssetStore for CropImageAsset {
        fn put(&self, _: &[u8], _: &str) -> DltResult<AssetRef> { Err(DltError::unsupported("no writes")) }
        fn read(&self, asset: &AssetRef) -> DltResult<Vec<u8>> {
            if asset.sha256 == self.reference.sha256 { Ok(self.bytes.clone()) } else { Err(DltError::new(ErrorCode::NotFound, asset.sha256.clone())) }
        }
        fn lookup(&self, hash: &str) -> DltResult<AssetRef> {
            if hash == self.reference.sha256 { Ok(self.reference.clone()) } else { Err(DltError::new(ErrorCode::NotFound, hash.to_owned())) }
        }
    }

    struct HtmlVideoAssets(std::collections::BTreeMap<String, (AssetRef, Vec<u8>)>);
    impl AssetStore for HtmlVideoAssets {
        fn put(&self, _: &[u8], _: &str) -> DltResult<AssetRef> { Err(DltError::unsupported("no writes")) }
        fn read(&self, asset: &AssetRef) -> DltResult<Vec<u8>> {
            self.0.get(&asset.sha256).filter(|(reference, _)| reference == asset).map(|(_, bytes)| bytes.clone())
                .ok_or_else(|| DltError::new(ErrorCode::NotFound, asset.sha256.clone()))
        }
        fn lookup(&self, hash: &str) -> DltResult<AssetRef> {
            self.0.get(hash).map(|(reference, _)| reference.clone()).ok_or_else(|| DltError::new(ErrorCode::NotFound, hash.to_owned()))
        }
    }

    #[test]
    fn merged_cells_export_one_semantic_html_cell_with_both_spans() {
        use designllm_contracts::{CanvasTable, CanvasTableRow, NodeExt, NodeKind, Page, Rect, Style};

        let cell = |id: &str, text: &str| Node {
            id: id.into(), kind: NodeKind::Rectangle, bounds: Rect { x: 0, y: 0, width: 200, height: 36 },
            text: text.into(), style: Style::default(), asset: None, children: vec![],
            layout: Some(LayoutSpec { width_mode: "fill".into(), height_mode: "hug".into(), ..Default::default() }),
            interactions: vec![], variants: Default::default(), ext: NodeExt::default(),
        };
        let table = Node {
            id: "table".into(), kind: NodeKind::Group, bounds: Rect { x: 0, y: 0, width: 400, height: 72 },
            text: String::new(), style: Style::default(), asset: None,
            children: vec![cell("c11", "anchor"), cell("c12", ""), cell("c21", ""), cell("c22", "")],
            layout: Some(LayoutSpec { mode: "grid".into(), height_mode: "hug".into(), grid_columns: Some(2), gap: 0, row_gap: Some(0), ..Default::default() }),
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
        let document = CanvasDocument {
            pages: vec![Page { nodes: vec![table], ..Page::new("p", Size::slide()) }],
            ..Default::default()
        };

        let html = String::from_utf8(export_html(&document, None, "merged", &EmptyAssets, None).expect("HTML export").bytes).unwrap();
        assert!(html.contains("rowspan=\"2\" colspan=\"2\""));
        assert!(html.contains("data-dlm-id=\"c11\""));
        assert_eq!(html.matches("<td ").count(), 1, "covered empty cells are not emitted as duplicate table cells");
        for covered in ["c12", "c21", "c22"] {
            assert!(!html.contains(&format!("data-dlm-id=\"{covered}\"")), "covered cell {covered} is not emitted");
        }
    }

    #[test]
    fn private_html_keeps_populated_covered_text_in_row_major_blocks() {
        use designllm_contracts::{CanvasTable, CanvasTableRow, CanvasTableSpan, NodeExt, NodeKind, Page, Rect, Style};

        let mut covered_style = Style::default();
        covered_style.color = "#CC0000".into();
        let cell = |id: &str, text: &str, style: Style| Node {
            id: id.into(), kind: NodeKind::Rectangle, bounds: Rect { x: 0, y: 0, width: 200, height: 36 },
            text: text.into(), style, asset: None, children: vec![],
            layout: Some(LayoutSpec { width_mode: "fill".into(), height_mode: "hug".into(), ..Default::default() }),
            interactions: vec![], variants: Default::default(), ext: NodeExt::default(),
        };
        let table = Node {
            id: "table".into(), kind: NodeKind::Group, bounds: Rect { x: 0, y: 0, width: 400, height: 72 },
            text: String::new(), style: Style::default(), asset: None,
            children: vec![
                cell("c11", "anchor-alpha", Style::default()),
                cell("c12", "covered-east", covered_style),
                cell("c21", "covered-south", Style::default()),
                cell("c22", "covered-corner", Style::default()),
            ],
            layout: Some(LayoutSpec { mode: "grid".into(), height_mode: "hug".into(), grid_columns: Some(2), gap: 0, row_gap: Some(0), ..Default::default() }),
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
        let document = CanvasDocument {
            pages: vec![Page { nodes: vec![table], ..Page::new("p", Size::slide()) }],
            ..Default::default()
        };

        assert!(super::model::validate_document(&document).iter().any(|diagnostic| diagnostic.code == "TABLE_SPAN_HIDDEN_DATA"),
            "this private fixture intentionally gives a covered cell a different text color from its anchor");
        let html = String::from_utf8(export_html(&document, None, "merged populated", &EmptyAssets, None).expect("private HTML path").bytes).unwrap();
        assert_eq!(html.matches("<td ").count(), 1, "the span remains one semantic table cell");
        assert_eq!(html.matches("data-dlm-id=\"c11\"").count(), 1, "the anchor keeps its original source ID");
        for cell_id in ["c12", "c21", "c22"] {
            assert_eq!(html.matches(&format!("data-dlm-id=\"{cell_id}\"")).count(), 1, "covered text retains {cell_id}");
        }
        let anchor = html.find("anchor-alpha").expect("anchor text");
        let east = html.find("covered-east").expect("first covered text");
        let south = html.find("covered-south").expect("second covered text");
        let corner = html.find("covered-corner").expect("third covered text");
        assert!(anchor < east && east < south && south < corner, "text blocks follow dense row-major order");
        let east_block = &html[html.find("data-dlm-id=\"c12\"").unwrap()..];
        let east_block = east_block.split("</div>").next().unwrap();
        assert!(east_block.contains("color:#CC0000;"), "covered text uses its source cell text style");
    }

    #[test]
    fn open_url_html_anchors_escape_hrefs_and_do_not_nest() {
        let child = Node {
            id: "child".into(), kind: NodeKind::Text, bounds: Rect { x: 10, y: 10, width: 30, height: 10 },
            text: "child link".into(), style: Style::default(), asset: None, children: vec![], layout: None,
            interactions: vec![Interaction::OpenUrl { url: "mailto:child@example.com".into() }],
            variants: Default::default(), ext: Default::default(),
        };
        let parent = Node {
            id: "parent".into(), kind: NodeKind::Group, bounds: Rect { x: 0, y: 0, width: 100, height: 50 },
            text: String::new(), style: Style::default(), asset: None, children: vec![child], layout: None,
            interactions: vec![Interaction::OpenUrl { url: "https://example.com/?q=\"parent\"&id=1".into() }],
            variants: Default::default(), ext: Default::default(),
        };
        let document = CanvasDocument {
            pages: vec![Page { nodes: vec![parent], ..Page::new("p", Size::slide()) }],
            styles: Default::default(),
            ..Default::default()
        };
        let html = String::from_utf8(export_html(&document, None, "links", &EmptyAssets, None).unwrap().bytes).unwrap();
        let parent_href = "href=\"https://example.com/?q=&quot;parent&quot;&amp;id=1\" target=\"_blank\" rel=\"noopener noreferrer\"";
        let child_href = "href=\"mailto:child@example.com\" target=\"_blank\" rel=\"noopener noreferrer\"";
        let parent_at = html.find(parent_href).expect("escaped parent href and safe target");
        let parent_end = parent_at + html[parent_at..].find("</a>").expect("parent anchor closes");
        let child_at = html.find(child_href).expect("child href and safe target");
        assert!(parent_end < child_at, "parent and child anchors are siblings, not nested");
        assert!(!html[parent_at..parent_end].contains("aria-label="), "a Group does not infer a link name from child text");
        assert_eq!(html.matches("data-dlm-id=\"child\"").count(), 1, "the Text link itself is the one node target");
        assert!(!html.contains("data-dlm-action-id"), "links do not add a second action identity");
        let legacy_script_suffix = format!("<script>\n{SCRIPT_LEGACY}</script>\n</body>\n</html>\n");
        assert!(html.ends_with(&legacy_script_suffix), "URL-only nodes keep the existing interpreter bytes");
        assert!(!html.contains("\"do\":\"open_url\""), "URL-only actions stay native links");
        assert!(!html.contains("\"do\":\"open_url\""), "URL-only nodes stay native links");
    }

    #[test]
    fn url_only_container_keeps_the_legacy_anchor_interpreter() {
        let node = Node {
            id: "card".into(), kind: NodeKind::Rectangle, bounds: Rect { x: 0, y: 0, width: 100, height: 40 },
            text: "card link".into(), style: Style::default(), asset: None, children: vec![], layout: None,
            interactions: vec![Interaction::OpenUrl { url: "https://example.com/card".into() }],
            variants: Default::default(), ext: designllm_contracts::NodeExt { name: Some("Layer name".into()), ..Default::default() },
        };
        let document = CanvasDocument {
            pages: vec![Page { nodes: vec![node], ..Page::new("p", Size::slide()) }],
            styles: Default::default(),
            ..Default::default()
        };
        let html = String::from_utf8(export_html(&document, None, "container-link", &EmptyAssets, None).unwrap().bytes).unwrap();
        let legacy_script_suffix = format!("<script>\n{SCRIPT_LEGACY}</script>\n</body>\n</html>\n");
        assert!(html.ends_with(&legacy_script_suffix), "a URL-only Rectangle keeps the legacy interpreter bytes");
        assert!(html.contains("href=\"https://example.com/card\" target=\"_blank\" rel=\"noopener noreferrer\""));
        assert!(html.contains("aria-label=\"card link\""), "visible text takes priority over layer name");
        assert!(!html.contains("aria-label=\"Layer name\""));
        assert!(!html.contains("\"do\":\"open_url\""), "a URL-only container remains a native anchor");
    }

    #[test]
    fn navigate_and_toggle_without_back_keep_the_original_html_interpreter() {
        let second = Page::new("second", Size::slide());
        let navigate = Node {
            id: "navigate".into(), kind: NodeKind::Rectangle, bounds: Rect { x: 0, y: 0, width: 100, height: 40 },
            text: "Next".into(), style: Style::default(), asset: None, children: vec![], layout: None,
            interactions: vec![Interaction::NavigatePage { page_id: second.id.clone() }],
            variants: Default::default(), ext: Default::default(),
        };
        let toggle = Node {
            id: "toggle".into(), kind: NodeKind::Rectangle, bounds: Rect { x: 120, y: 0, width: 100, height: 40 },
            text: "Toggle".into(), style: Style::default(), asset: None, children: vec![], layout: None,
            interactions: vec![Interaction::ToggleVisibility { node_id: "navigate".into() }],
            variants: Default::default(), ext: Default::default(),
        };
        let document = CanvasDocument {
            pages: vec![
                Page { nodes: vec![navigate, toggle], ..Page::new("first", Size::slide()) },
                second,
            ],
            styles: Default::default(),
            ..Default::default()
        };
        let html = String::from_utf8(export_html(&document, None, "legacy actions", &EmptyAssets, None).unwrap().bytes).unwrap();
        let suffix = format!("<script>\n{SCRIPT_LEGACY}</script>\n</body>\n</html>\n");
        assert!(html.ends_with(&suffix), "non-Back, non-mixed exports keep the exact original interpreter");
        assert!(!html.contains("pageHistory"));
        assert!(html.contains("\"do\":\"navigate\""));
        assert!(html.contains("\"do\":\"toggle\""));
    }

    #[test]
    fn unnamed_group_link_reports_a_diagnostic_instead_of_using_child_text() {
        let mut hidden = Node {
            id: "hidden-label".into(), kind: NodeKind::Text, bounds: Rect { x: 0, y: 0, width: 80, height: 20 },
            text: "Hidden label".into(), style: Style::default(), asset: None, children: vec![], layout: None,
            interactions: vec![], variants: Default::default(), ext: Default::default(),
        };
        hidden.ext.hidden = Some(true);
        let visible = Node {
            id: "visible-label".into(), kind: NodeKind::Text, bounds: Rect { x: 0, y: 24, width: 120, height: 20 },
            text: "Visible child heading".into(), style: Style::default(), asset: None, children: vec![], layout: None,
            interactions: vec![], variants: Default::default(), ext: Default::default(),
        };
        let group = Node {
            id: "unnamed-link".into(), kind: NodeKind::Group, bounds: Rect { x: 0, y: 0, width: 160, height: 60 },
            text: String::new(), style: Style::default(), asset: None, children: vec![hidden, visible], layout: None,
            interactions: vec![Interaction::OpenUrl { url: "https://example.com/group".into() }],
            variants: Default::default(), ext: designllm_contracts::NodeExt { name: Some("Internal layer name".into()), ..Default::default() },
        };
        let document = CanvasDocument {
            pages: vec![Page { nodes: vec![group], ..Page::new("p", Size::slide()) }],
            styles: Default::default(),
            ..Default::default()
        };
        let output = export_html(&document, None, "unnamed group link", &EmptyAssets, None).unwrap();
        let diagnostic = output.diagnostics.iter().find(|d| d.code == "HTML_LINK_ACCESSIBLE_NAME_MISSING").expect("non-blocking link-name diagnostic");
        assert!(!diagnostic.blocking);
        assert_eq!(diagnostic.node_id.as_deref(), Some("unnamed-link"));
        assert!(diagnostic.message.contains("alternative text or visible text"));
        let html = String::from_utf8(output.bytes).unwrap();
        assert!(!html.contains("aria-label=\"Internal layer name\""), "a non-visible layer name is not exposed as the Group link name");
        assert!(!html.contains("aria-label=\"Hidden label\""));
        assert!(!html.contains("aria-label=\"Visible child heading\""), "child text is not guessed as the Group link's name");
    }

    #[test]
    fn mixed_text_open_url_uses_one_keyboard_accessible_node_anchor() {
        let node = Node {
            id: "text-link".into(), kind: NodeKind::Text, bounds: Rect { x: 10, y: 10, width: 80, height: 20 },
            text: "open link".into(), style: Style::default(), asset: None, children: vec![], layout: None,
            interactions: vec![Interaction::OpenUrl { url: "https://example.com/action".into() }, Interaction::Back],
            variants: Default::default(), ext: Default::default(),
        };
        let document = CanvasDocument {
            pages: vec![Page { nodes: vec![node], ..Page::new("p", Size::slide()) }],
            styles: Default::default(),
            ..Default::default()
        };
        let html = String::from_utf8(export_html(&document, None, "mixed", &EmptyAssets, None).unwrap().bytes).unwrap();
        let link = html.find("<a class=\"dlm-n0\"").expect("the Text node itself is the link anchor");
        let link_end = link + html[link..].find('>').expect("anchor opens");
        assert!(html[link..link_end].contains("data-dlm-id=\"text-link\""));
        assert!(html[link..link_end].contains("href=\"https://example.com/action\""));
        assert_eq!(html.matches("data-dlm-id=\"text-link\"").count(), 1, "the node ID appears once on the anchor itself");
        assert!(!html.contains("data-dlm-action-id"), "the anchor and node do not create nested interaction identities");
        assert!(html.contains("window.open(action.url, '_blank', 'noopener,noreferrer')"), "mixed links use the ordered action interpreter");

        let marker = "<script type=\"application/json\" id=\"dlm-config\">";
        let start = html.find(marker).unwrap() + marker.len();
        let end = start + html[start..].find("</script>").unwrap();
        let config: Value = serde_json::from_str(&html[start..end]).unwrap();
        assert_eq!(
            config["actions"]["text-link"],
            json!([
                { "do": "open_url", "url": "https://example.com/action" },
                { "do": "back" }
            ]),
        );
    }

    #[test]
    fn image_open_url_is_the_single_focusable_root_anchor() {
        let asset = AssetRef { sha256: "a".repeat(64), mime: "image/png".into(), bytes: 1 };
        let node = Node {
            id: "image-link".into(), kind: NodeKind::Image, bounds: Rect { x: 0, y: 0, width: 100, height: 50 },
            text: String::new(), style: Style::default(), asset: Some(asset), children: vec![], layout: None,
            interactions: vec![Interaction::OpenUrl { url: "https://example.com/image".into() }, Interaction::Back],
            variants: Default::default(),
            ext: designllm_contracts::NodeExt { name: Some("Image & Link".into()), ..Default::default() },
        };
        let document = CanvasDocument {
            pages: vec![Page { nodes: vec![node], ..Page::new("p", Size::slide()) }],
            styles: Default::default(),
            ..Default::default()
        };
        let html = String::from_utf8(export_html(&document, None, "image", &OneImage, None).unwrap().bytes).unwrap();
        assert!(html.contains("<a class=\"dlm-n0\" data-dlm-id=\"image-link\""));
        assert!(html.contains("href=\"https://example.com/image\""));
        assert!(html.contains("aria-label=\"Image &amp; Link\""), "layer name supplies the image link name when alt is absent");
        assert!(html.contains("<img alt=\"\" draggable=\"false\" style=\"display:block;"));
        assert_eq!(html.matches("data-dlm-id=\"image-link\"").count(), 1, "nodeById sees the anchor, not a duplicate image node");
        assert!(!html.contains("data-dlm-action-id"));
    }

    #[test]
    fn path_open_url_uses_one_svg_link_without_a_nested_html_anchor() {
        let node = Node {
            id: "path-link".into(), kind: NodeKind::Path, bounds: Rect { x: 0, y: 0, width: 100, height: 100 },
            text: String::new(), style: Style::default(), asset: None, children: vec![], layout: None,
            interactions: vec![Interaction::OpenUrl { url: "https://example.com/path".into() }, Interaction::Back],
            variants: Default::default(),
            ext: designllm_contracts::NodeExt {
                alt: Some("Vector \"label\" & <path>".into()),
                name: Some("Path layer fallback".into()),
                path: Some(PathSpec {
                    view_box: Rect { x: 0, y: 0, width: 100_000, height: 100_000 },
                    fill_rule: FillRule::Nonzero,
                    commands: vec![PathCommand::MoveTo { x: 0, y: 0 }, PathCommand::LineTo { x: 100_000, y: 100_000 }],
                }),
                ..Default::default()
            },
        };
        let document = CanvasDocument {
            pages: vec![Page { nodes: vec![node], ..Page::new("p", Size::slide()) }],
            styles: Default::default(),
            ..Default::default()
        };
        let html = String::from_utf8(export_html(&document, None, "path", &EmptyAssets, None).unwrap().bytes).unwrap();
        assert!(html.contains("<svg class=\"dlm-n0\" data-dlm-id=\"path-link\""));
        assert!(html.contains("<a href=\"https://example.com/path\" target=\"_blank\" rel=\"noopener noreferrer\" tabindex=\"0\" aria-label=\"Vector &quot;label&quot; &amp; &lt;path&gt;\""));
        assert_eq!(html.matches("data-dlm-id=\"path-link\"").count(), 1);
        assert!(!html.contains("data-dlm-action-id"));
    }

    #[test]
    fn css_string_escapes_quotes_and_backslashes() {
        assert_eq!(css_string("A\"B\\C"), "A\\\"B\\\\C");
    }

    #[test]
    fn interval_conditions_cover_open_and_closed_ranges() {
        assert_eq!(interval_condition(0, None), None);
        assert_eq!(interval_condition(600, None).as_deref(), Some("(min-width: 600px)"));
        assert_eq!(interval_condition(0, Some(600)).as_deref(), Some("(max-width: 599.999px)"));
        assert_eq!(
            interval_condition(600, Some(900)).as_deref(),
            Some("(min-width: 600px) and (max-width: 899.999px)")
        );
    }

    #[test]
    fn opacity_multiplies_the_alpha_of_a_colour() {
        assert_eq!(paint("#112233", None), "#112233");
        assert_eq!(paint("#112233", Some(1000)), "#112233");
        assert_eq!(paint("#112233", Some(500)), "rgba(17,34,51,0.502)");
        assert_eq!(paint("#11223380", Some(500)), "rgba(17,34,51,0.251)");
        assert_eq!(paint("transparent", Some(500)), "transparent");
    }

    #[test]
    fn hangul_text_gets_keep_all_and_other_text_does_not() {
        assert!(has_hangul("가나다"));
        assert!(!has_hangul("Hello 中文"));
        let plain = style_decls(&Style::default(), NodeKind::Text, "Hello", None);
        assert!(!plain.contains("keep-all"));
        let korean = style_decls(&Style::default(), NodeKind::Text, "안녕", None);
        assert!(korean.contains("word-break:keep-all;overflow-wrap:anywhere;"));
    }

    #[test]
    fn dashes_follow_the_renderer_rule_in_svg_and_css() {
        assert_eq!(dash_attr(Some(StrokeDash::Dashed), 2.0), " stroke-dasharray=\"6 6\"");
        assert_eq!(dash_attr(Some(StrokeDash::Dotted), 2.0), " stroke-dasharray=\"2 2\"");
        assert_eq!(dash_attr(None, 2.0), "");
        let dashed = style_decls(&Style { stroke_dash: Some(StrokeDash::Dashed), stroke_width: 9525, ..Style::default() }, NodeKind::Rectangle, "", None);
        assert!(dashed.contains("border:1px dashed"), "{dashed}");
    }

    #[test]
    fn image_fit_contain_maps_to_object_fit() {
        assert!(style_decls(&Style::default(), NodeKind::Image, "", Some(ImageFit::Contain)).contains("object-fit:contain"));
        assert!(style_decls(&Style::default(), NodeKind::Image, "", None).contains("object-fit:fill"));
    }

    #[test]
    fn video_html_keeps_the_source_and_first_frame_poster_for_playback() {
        let mut mp4 = Vec::new();
        mp4.extend_from_slice(&16u32.to_be_bytes());
        mp4.extend_from_slice(b"ftypisom");
        mp4.extend_from_slice(&0u32.to_be_bytes());
        let poster_bytes = designllm_render::png_from_rgba(1, 1, &[5, 6, 7, 255]).expect("poster PNG");
        let source = AssetRef { sha256: designllm_contracts::sha256(&mp4), mime: "video/mp4".into(), bytes: mp4.len() as u64 };
        let poster = AssetRef { sha256: designllm_contracts::sha256(&poster_bytes), mime: "image/png".into(), bytes: poster_bytes.len() as u64 };
        let assets = HtmlVideoAssets([
            (source.sha256.clone(), (source.clone(), mp4.clone())),
            (poster.sha256.clone(), (poster.clone(), poster_bytes.clone())),
        ].into_iter().collect());
        let node = Node {
            id: "video".into(), kind: NodeKind::Video, bounds: Rect { x: 0, y: 0, width: 640, height: 360 },
            text: String::new(), style: Style::default(), asset: Some(source), children: Vec::new(), layout: None,
            interactions: Vec::new(), variants: Default::default(),
            ext: designllm_contracts::NodeExt { alt: Some("Training clip".into()), video_poster_asset: Some(poster), ..Default::default() },
        };
        let document = CanvasDocument {
            pages: vec![Page { nodes: vec![node], ..Page::new("p", Size::slide()) }],
            ..Default::default()
        };
        let html = String::from_utf8(export_html(&document, None, "video", &assets, None).expect("HTML video export").bytes).unwrap();
        assert!(html.contains("<video class=\"dlm-n0\" data-dlm-id=\"video\" controls playsinline preload=\"metadata\""), "the exported element has native playback controls");
        assert!(html.contains(&format!("poster=\"data:image/png;base64,{}\"", base64_encode(&poster_bytes))));
        assert!(html.contains(&format!("src=\"data:video/mp4;base64,{}\"", base64_encode(&mp4))), "the original video bytes are embedded for playback");
        assert!(html.contains("aria-label=\"Training clip\""), "the node alternative text labels the video control");
        assert!(!html.contains("autoplay"), "video playback does not start automatically");
    }

    #[test]
    fn html_exports_the_selected_source_pixels_before_applying_fit() {
        let source_pixels = [255, 0, 0, 255, 0, 0, 255, 255];
        let bytes = designllm_render::png_from_rgba(2, 1, &source_pixels).expect("2x1 source image");
        let asset = AssetRef { sha256: designllm_contracts::sha256(&bytes), mime: "image/png".into(), bytes: bytes.len() as u64 };
        let assets = CropImageAsset { reference: asset.clone(), bytes };
        let crop = designllm_contracts::PixelRect { x: 1, y: 0, width: 1, height: 1 };
        let image = Node {
            id: "cropped-image".into(), kind: NodeKind::Image, bounds: Rect { x: 0, y: 0, width: 100, height: 50 },
            text: String::new(), style: Style::default(), asset: Some(asset), children: Vec::new(), layout: None,
            interactions: Vec::new(), variants: Default::default(),
            ext: designllm_contracts::NodeExt { alt: Some("blue pixel".into()), image_fit: Some(ImageFit::Contain), image_crop_pixel_rect: Some(crop), ..Default::default() },
        };
        let document = CanvasDocument {
            pages: vec![Page { nodes: vec![image], ..Page::new("p", Size::slide()) }],
            ..Default::default()
        };
        let html = String::from_utf8(export_html(&document, None, "crop", &assets, None).expect("HTML crop export").bytes).unwrap();
        let cropped_png = designllm_render::png_from_rgba(1, 1, &[0, 0, 255, 255]).expect("crop bytes contain the selected blue source pixel");
        assert!(html.contains(&format!("data:image/png;base64,{}", base64_encode(&cropped_png))));
        assert!(html.contains("image-orientation:none;"), "cropped HTML uses the same source-pixel orientation as native renderers");
        assert!(html.contains("object-fit:contain"), "the existing fit is applied after crop extraction");
        assert!(html.contains("alt=\"blue pixel\""));
    }

    #[test]
    fn path_data_uses_user_units() {
        let path = PathSpec {
            view_box: Rect { x: 0, y: 0, width: 24_000, height: 24_000 },
            fill_rule: FillRule::Nonzero,
            commands: vec![PathCommand::MoveTo { x: 0, y: 0 }, PathCommand::LineTo { x: 12_500, y: 2_000 }, PathCommand::Close],
        };
        assert_eq!(path_d(&path), "M0 0 L12.5 2 Z");
    }

    #[test]
    fn a_variant_that_clears_a_field_writes_its_default() {
        use designllm_contracts::{GradientStop, NodeExt, Shadow, StyleField};
        let node = Node {
            id: "r".into(),
            kind: NodeKind::Rectangle,
            bounds: Rect { x: 0, y: 0, width: 10, height: 10 },
            text: String::new(),
            style: Style {
                fill: "#FFFFFF".into(),
                gradient: Some(Gradient {
                    angle_millideg: 0,
                    stops: vec![
                        GradientStop { pos_milli: 0, color: "#000000".into() },
                        GradientStop { pos_milli: 1000, color: "#FFFFFF".into() },
                    ],
                }),
                corner_radius: Some(8 * 9_525),
                shadows: vec![Shadow { x: 0, y: 0, blur: 4 * 9_525, spread: 0, color: "#000000".into() }],
                ..Style::default()
            },
            asset: None,
            children: vec![],
            layout: None,
            interactions: vec![],
            variants: std::collections::BTreeMap::new(),
            ext: NodeExt::default(),
        };
        let clears = StylePatch {
            clear: [StyleField::Gradient, StyleField::CornerRadius, StyleField::Shadows].into_iter().collect(),
            ..Default::default()
        };
        let decls = variant_decls(&node, &clears);
        assert!(decls.contains("background:#FFFFFF;"), "{decls}");
        assert!(!decls.contains("linear-gradient"), "{decls}");
        assert!(decls.contains("border-radius:0;") && decls.contains("box-shadow:none;"), "{decls}");
    }

    #[test]
    fn typography_css_honours_weight_precedence_and_partial_decoration_changes() {
        let node = Node {
            id: "t".into(),
            kind: NodeKind::Text,
            bounds: Rect { x: 0, y: 0, width: 100, height: 40 },
            text: "text".into(),
            style: Style { bold: true, font_weight: Some(400), underline: Some(true), strikethrough: Some(true), ..Default::default() },
            asset: None,
            children: vec![],
            layout: None,
            interactions: vec![],
            variants: Default::default(),
            ext: Default::default(),
        };
        let base = style_decls(&node.style, node.kind, &node.text, None);
        assert!(base.contains("font-weight:400;") && base.contains("text-decoration-line:underline line-through;"), "{base}");

        let patch = StylePatch { bold: Some(true), underline: Some(false), ..Default::default() };
        let variant = patch_decls(&node, &patch);
        assert!(variant.contains("font-weight:400;"), "font_weight overrides bold: {variant}");
        assert!(variant.contains("text-decoration-line:line-through;"), "the unset decoration remains: {variant}");
    }
}
