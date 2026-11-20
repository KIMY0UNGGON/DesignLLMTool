//! Contract v1.2 values in PresentationML (P1): the native XML of each value, and a loss entry for each part that
//! PresentationML cannot carry exactly. A value that is not set produces no XML, so a v1.1 scene keeps its bytes.
//!
//! Mapping (docs/P1_PLAN.md): corner radius -> `roundRect` with `adj`; per-paint opacity -> `a:alpha`; outer shadow ->
//! `a:outerShdw`; two-stop gradient -> `a:gradFill` with `a:lin`; path -> `a:custGeom`; line height -> `a:lnSpc` (exact
//! points); letter spacing -> `spc`; vertical alignment -> `bodyPr` anchor; stroke dash -> `a:prstDash` or `a:custDash`; stroke cap ->
//! `cap`; stroke join -> `a:round`, `a:bevel`, or `a:miter`. A value that PowerPoint does not carry
//! exactly is still written as closely as possible, and a loss entry (diagnostic `PPTX_LOSS`, non-blocking) says how.

use designllm_contracts::{
    quantize, Diagnostic, DltError, DltResult, FillRule, Gradient, LineEnd, PathCommand, PathSpec, Rect, Shadow, Style, StyleField,
    StrokeCap, StrokeDash, StrokeJoin, VerticalAlign,
};
use designllm_package::xml as pxml;

use crate::writer::{centipoints, generated_rpr, hex6};

/// Diagnostic code of a value that PresentationML does not carry exactly.
pub const LOSS: &str = "PPTX_LOSS";

/// A loss entry for `field` on `node_id`. The message names what PowerPoint shows differently.
pub fn loss(node_id: &str, field: &str, message: &str) -> Diagnostic {
    Diagnostic { code: LOSS.into(), message: format!("{field}: {message}"), node_id: Some(node_id.to_string()), blocking: false, params: Default::default() }
}

/// The kind of primitive a style is written for. Each kind maps a different set of fields.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Text,
    Shape,
    Path,
    Line,
}

/// A paint opacity in 1/1000 (0..=1000). Unset means opaque.
pub fn opacity_of(value: Option<u32>, node_id: &str) -> DltResult<u32> {
    match value {
        None => Ok(1000),
        Some(v) if v <= 1000 => Ok(v),
        Some(v) => Err(DltError::invalid(format!("{node_id}: opacity_milli {v} is above 1000"))),
    }
}

/// `#RRGGBB` or `#RRGGBBAA`: the colour with six digits, and its own alpha (255 when absent).
fn split_colour(color: &str, node_id: &str) -> DltResult<(String, u32)> {
    let body = color.strip_prefix('#').unwrap_or("");
    let hex = body.bytes().all(|b| b.is_ascii_hexdigit());
    match body.len() {
        6 if hex => Ok((format!("#{body}"), 255)),
        8 if hex => {
            let alpha = u32::from_str_radix(&body[6..], 16)
                .map_err(|_| DltError::invalid(format!("{node_id}: colour {color:?} has a bad alpha")))?;
            Ok((format!("#{}", &body[..6]), alpha))
        }
        _ => Err(DltError::invalid(format!("{node_id}: colour {color:?} must be #RRGGBB or #RRGGBBAA"))),
    }
}

/// `a:srgbClr` for a colour with the paint opacity multiplied in. The alpha is the colour's alpha times the opacity,
/// in 1/1000 % (100000 is opaque). A product that does not divide exactly is rounded half up, and reported.
pub fn srgb(color: &str, opacity: u32, node_id: &str, field: &str, losses: &mut Vec<Diagnostic>) -> DltResult<String> {
    let (rgb, colour_alpha) = split_colour(color, node_id)?;
    let product = u64::from(colour_alpha) * u64::from(opacity) * 100;
    if product % 255 != 0 {
        losses.push(loss(node_id, field, "the alpha is rounded to 1/1000 %"));
    }
    let alpha = ((product + 127) / 255) as u32;
    let rgb = hex6(&rgb)?;
    if alpha >= 100_000 {
        Ok(format!("<a:srgbClr val=\"{rgb}\"/>"))
    } else {
        Ok(format!("<a:srgbClr val=\"{rgb}\"><a:alpha val=\"{alpha}\"/></a:srgbClr>"))
    }
}

/// The `a:solidFill` or `a:gradFill` of a shape. A gradient is painted instead of the fill.
pub fn fill_xml(style: &Style, node_id: &str, losses: &mut Vec<Diagnostic>) -> DltResult<String> {
    let opacity = opacity_of(style.opacity_milli, node_id)?;
    if let Some(gradient) = &style.gradient {
        return grad_fill_xml(gradient, opacity, node_id, losses);
    }
    if style.fill == "none" {
        return Ok("<a:noFill/>".into());
    }
    Ok(format!("<a:solidFill>{}</a:solidFill>", srgb(&style.fill, opacity, node_id, "fill", losses)?))
}

/// `a:gradFill`: every stop, its position in 1/1000 % (100000 is the end), and the CSS angle turned into DrawingML's:
/// CSS 90 deg (left to right) is DrawingML 0, in 1/60000 deg. The gradient is not scaled to the shape.
fn grad_fill_xml(gradient: &Gradient, opacity: u32, node_id: &str, losses: &mut Vec<Diagnostic>) -> DltResult<String> {
    if gradient.stops.len() < 2 {
        return Err(DltError::invalid(format!("{node_id}: a gradient needs two stops")));
    }
    let mut stops = String::new();
    for stop in &gradient.stops {
        if stop.pos_milli > 1000 {
            return Err(DltError::invalid(format!("{node_id}: a gradient stop is above 1000")));
        }
        stops.push_str(&format!(
            "<a:gs pos=\"{}\">{}</a:gs>",
            stop.pos_milli * 100,
            srgb(&stop.color, opacity, node_id, "gradient", losses)?
        ));
    }
    let angle = (i64::from(gradient.angle_millideg) - 90_000).rem_euclid(360_000) * 60;
    Ok(format!("<a:gradFill rotWithShape=\"1\"><a:gsLst>{stops}</a:gsLst><a:lin ang=\"{angle}\" scaled=\"0\"/></a:gradFill>"))
}

/// The largest `a:ln/@w`: ST_LineWidth, 20116800 EMU (1584 pt). A width outside `0..=MAX_LINE_WIDTH_EMU` is not a valid
/// line width.
pub const MAX_LINE_WIDTH_EMU: i64 = 20_116_800;

/// The `a:ln/@w` of an outline on the create path, which reports what it changes. The render draws a line at no less than
/// 1 EMU and `a:ln/@w` cannot be negative, so a negative width is written as 0, the thinnest line. A width above the maximum
/// is written as the maximum. Each change is a loss entry.
fn outline_width(width: i64, node_id: &str, losses: &mut Vec<Diagnostic>) -> i64 {
    if width < 0 {
        losses.push(loss(node_id, "stroke_width", "a negative width is written as 0"));
        return 0;
    }
    if width > MAX_LINE_WIDTH_EMU {
        losses.push(loss(
            node_id,
            "stroke_width",
            &format!("a width above {MAX_LINE_WIDTH_EMU} EMU is written as {MAX_LINE_WIDTH_EMU} EMU"),
        ));
        return MAX_LINE_WIDTH_EMU;
    }
    width
}

/// The `a:ln/@w` of an outline on the edit path. That path returns no diagnostics, and a clamped width would differ from the
/// model when the saved file is verified, so a width outside `0..=MAX_LINE_WIDTH_EMU` is refused, as `opacity_of` refuses an
/// opacity above 1000.
pub fn checked_line_width(width: i64, node_id: &str) -> DltResult<i64> {
    if (0..=MAX_LINE_WIDTH_EMU).contains(&width) {
        return Ok(width);
    }
    Err(DltError::invalid(format!("{node_id}: stroke width {width} EMU is outside 0..={MAX_LINE_WIDTH_EMU}, the range of a:ln/@w")))
}

/// `a:ln`: no line, or a solid line of the stroke colour and width, with its opacity, cap, dash, and join. The children
/// keep the schema order: fill, dash, join, then the arrow end (`tail`). An unset dash, cap, or join writes nothing.
pub fn ln_xml(style: &Style, node_id: &str, losses: &mut Vec<Diagnostic>, tail: &str) -> DltResult<String> {
    let width = outline_width(style.stroke_width, node_id, losses);
    if style.stroke == "none" {
        return Ok(format!("<a:ln w=\"{width}\"><a:noFill/>{tail}</a:ln>"));
    }
    let opacity = opacity_of(style.opacity_milli, node_id)?;
    let colour = srgb(&style.stroke, opacity, node_id, "stroke", losses)?;
    let dash = dash_xml(style);
    Ok(format!(
        "<a:ln w=\"{width}\"{}><a:solidFill>{colour}</a:solidFill>{dash}{}{tail}</a:ln>",
        cap_attr(style),
        join_xml(style)
    ))
}

/// Endpoint markers represented by the contract. OOXML width and length attributes are left at their standard defaults.
pub(crate) fn line_end_xml(style: &Style) -> String {
    fn end(name: &str, marker: Option<LineEnd>) -> String {
        let kind = match marker {
            Some(LineEnd::Open) => "open",
            Some(LineEnd::Filled) => "triangle",
            None => return String::new(),
        };
        format!("<a:{name} type=\"{kind}\"/>")
    }
    format!("{}{}", end("headEnd", style.stroke_start), end("tailEnd", style.stroke_end))
}

/// The dash of a stroke, or nothing when unset. Solid is `prstDash solid`. Dotted is `prstDash sysDot` (dash 1 and gap 1
/// line widths, the render's 1w and 1w; PowerPoint's preset table is not checked in PowerPoint itself). Dashed is a custom
/// dash of 3 and 3 line widths: `a:ds` values are in 1/1000 % of the line width, so 300000 is 3w. That is the render's
/// 3w and 3w exactly, so nothing is reported.
fn dash_xml(style: &Style) -> String {
    match style.stroke_dash {
        None => String::new(),
        Some(StrokeDash::Solid) => "<a:prstDash val=\"solid\"/>".into(),
        Some(StrokeDash::Dotted) => "<a:prstDash val=\"sysDot\"/>".into(),
        Some(StrokeDash::Dashed) => "<a:custDash><a:ds d=\"300000\" sp=\"300000\"/></a:custDash>".into(),
    }
}

/// The `cap` attribute of the outline: butt is `flat`, round is `rnd`, square is `sq`. Unset writes nothing.
fn cap_attr(style: &Style) -> &'static str {
    match style.stroke_cap {
        None => "",
        Some(StrokeCap::Butt) => " cap=\"flat\"",
        Some(StrokeCap::Round) => " cap=\"rnd\"",
        Some(StrokeCap::Square) => " cap=\"sq\"",
    }
}

/// The join of the outline: round, bevel, or miter. The miter limit is 4, the render's limit (tiny-skia's default, which
/// the raster and SVG output both use), not PowerPoint's default of 8. With 8, joins sharper than 4 to 8 would be mitred
/// in PowerPoint and bevelled in the render. Unset writes nothing.
fn join_xml(style: &Style) -> &'static str {
    match style.stroke_join {
        None => "",
        Some(StrokeJoin::Round) => "<a:round/>",
        Some(StrokeJoin::Bevel) => "<a:bevel/>",
        Some(StrokeJoin::Miter) => "<a:miter lim=\"400000\"/>",
    }
}

/// `a:effectLst` with the outer shadow, or nothing. Only the first shadow is written; a second one is reported. A
/// blur is written as `blurRad`, which PowerPoint blurs differently from CSS, so it is reported. A spread has no
/// PowerPoint counterpart and is not written. An offset off the axes is written as a distance and a direction.
pub fn effect_xml(style: &Style, node_id: &str, losses: &mut Vec<Diagnostic>) -> DltResult<String> {
    let Some(shadow) = style.shadows.first() else { return Ok(String::new()) };
    if style.shadows.len() > 1 {
        losses.push(loss(node_id, "shadows", "only the first shadow is written"));
    }
    if shadow.blur < 0 {
        return Err(DltError::invalid(format!("{node_id}: a shadow blur cannot be negative")));
    }
    let opacity = opacity_of(style.opacity_milli, node_id)?;
    let (dist, dir) = offset_polar(shadow, node_id, losses);
    if shadow.blur > 0 {
        losses.push(loss(node_id, "shadows", "the blur is written as blurRad, which PowerPoint blurs differently from CSS"));
    }
    if shadow.spread != 0 {
        losses.push(loss(node_id, "shadows", "the spread has no PowerPoint counterpart and is not written"));
    }
    let colour = srgb(&shadow.color, opacity, node_id, "shadows", losses)?;
    Ok(format!(
        "<a:effectLst><a:outerShdw blurRad=\"{}\" dist=\"{dist}\" dir=\"{dir}\" algn=\"ctr\" rotWithShape=\"0\">{colour}</a:outerShdw></a:effectLst>",
        shadow.blur.max(0)
    ))
}

/// Distance (EMU) and direction (1/60000 deg, 0 = right, clockwise) of a shadow offset. Axis offsets are exact; any
/// other offset is rounded, and reported.
fn offset_polar(shadow: &Shadow, node_id: &str, losses: &mut Vec<Diagnostic>) -> (i64, i64) {
    let (x, y) = (shadow.x, shadow.y);
    if x == 0 && y == 0 {
        return (0, 0);
    }
    if y == 0 {
        return (x.abs(), if x > 0 { 0 } else { 10_800_000 });
    }
    if x == 0 {
        return (y.abs(), if y > 0 { 5_400_000 } else { 16_200_000 });
    }
    losses.push(loss(node_id, "shadows", "an offset off the axes is written as a distance and a direction, rounded"));
    let dist = (x as f64).hypot(y as f64).round() as i64;
    let degrees = (y as f64).atan2(x as f64).to_degrees();
    let dir = ((degrees * 60_000.0).round() as i64).rem_euclid(21_600_000);
    (dist, dir)
}

/// The preset geometry of a shape. A corner radius on `rect` or `roundRect` is `roundRect` with `adj` (the radius as
/// 1/100000 of the shorter side). A radius above half the shorter side is clamped, and one that does not divide is
/// rounded; both are reported. Other presets have no corner radius in PowerPoint, so it is reported and not written.
pub fn geometry_xml(shape: &str, style: &Style, bounds: &Rect, node_id: &str, losses: &mut Vec<Diagnostic>) -> DltResult<String> {
    let plain = format!("<a:prstGeom prst=\"{shape}\"><a:avLst/></a:prstGeom>");
    let radius = style.corner_radius.unwrap_or(0);
    if radius < 0 {
        return Err(DltError::invalid(format!("{node_id}: the corner radius cannot be negative")));
    }
    if radius == 0 {
        return Ok(plain);
    }
    if shape != "rect" && shape != "roundRect" {
        losses.push(loss(node_id, "corner_radius", &format!("{shape} has no corner radius in PowerPoint; not written")));
        return Ok(plain);
    }
    let shorter = bounds.width.min(bounds.height);
    if shorter <= 0 {
        losses.push(loss(node_id, "corner_radius", "the shape has no size; not written"));
        return Ok(plain);
    }
    let adj = if radius * 2 > shorter {
        losses.push(loss(node_id, "corner_radius", "clamped to half the shorter side"));
        50_000
    } else {
        let numerator = radius * 100_000;
        if numerator % shorter != 0 {
            losses.push(loss(node_id, "corner_radius", "rounded to 1/100000 of the shorter side"));
        }
        (numerator + shorter / 2) / shorter
    };
    Ok(format!("<a:prstGeom prst=\"roundRect\"><a:avLst><a:gd name=\"adj\" fmla=\"val {adj}\"/></a:avLst></a:prstGeom>"))
}

/// `a:custGeom` for a path. Its points are in the view box, which PowerPoint stretches to the shape, so the path keeps
/// the view box as its size. PowerPoint keeps no fill rule, so an even-odd path, or one with several subpaths, is
/// reported: overlapping parts may fill differently. A view box with an origin other than 0, 0 is refused: PresentationML
/// has no place for the origin, and dropping it would move every point.
pub fn cust_geom_xml(path: &PathSpec, node_id: &str, losses: &mut Vec<Diagnostic>) -> DltResult<String> {
    let (width, height) = (path.view_box.width, path.view_box.height);
    if width <= 0 || height <= 0 {
        return Err(DltError::invalid(format!("{node_id}: the path view box must be positive")));
    }
    if path.view_box.x != 0 || path.view_box.y != 0 {
        return Err(DltError::unsupported(format!(
            "{node_id}: a view box with an origin other than 0, 0 cannot be written: PowerPoint keeps only the size of a path"
        )));
    }
    let subpaths = path.commands.iter().filter(|c| matches!(c, PathCommand::MoveTo { .. })).count();
    if path.fill_rule == FillRule::Evenodd || subpaths > 1 {
        losses.push(loss(node_id, "path", "PowerPoint keeps no fill rule; overlapping parts may fill differently"));
    }
    let mut body = String::new();
    for command in &path.commands {
        body.push_str(&match *command {
            PathCommand::MoveTo { x, y } => format!("<a:moveTo><a:pt x=\"{x}\" y=\"{y}\"/></a:moveTo>"),
            PathCommand::LineTo { x, y } => format!("<a:lnTo><a:pt x=\"{x}\" y=\"{y}\"/></a:lnTo>"),
            PathCommand::CubicTo { x1, y1, x2, y2, x, y } => format!(
                "<a:cubicBezTo><a:pt x=\"{x1}\" y=\"{y1}\"/><a:pt x=\"{x2}\" y=\"{y2}\"/><a:pt x=\"{x}\" y=\"{y}\"/></a:cubicBezTo>"
            ),
            PathCommand::Close => "<a:close/>".to_string(),
        });
    }
    Ok(format!(
        "<a:custGeom><a:avLst/><a:gdLst/><a:ahLst/><a:cxnLst/><a:rect l=\"l\" t=\"t\" r=\"r\" b=\"b\"/><a:pathLst><a:path w=\"{width}\" h=\"{height}\">{body}</a:path></a:pathLst></a:custGeom>"
    ))
}

/// `a:lnSpc` with the exact line height: the line height multiple times the font size, in 1/100 pt. PowerPoint places
/// exact line spacing by its own rule, so the line can move; that is always reported.
pub fn line_spacing_xml(style: &Style, node_id: &str, losses: &mut Vec<Diagnostic>) -> DltResult<Option<String>> {
    let Some(milli) = style.line_height_milli else { return Ok(None) };
    if milli == 0 {
        return Err(DltError::invalid(format!("{node_id}: the line height must be positive")));
    }
    let denominator = 1000 * 127;
    let product = style.font_size * i64::from(milli);
    let centi = (product + denominator / 2) / denominator;
    let message = if product % denominator != 0 {
        "the spacing is rounded to 1/100 pt, and PowerPoint places exact spacing by its own rule; the line can move"
    } else {
        "PowerPoint places exact spacing by its own rule; the line can move"
    };
    losses.push(loss(node_id, "line_height", message));
    Ok(Some(format!("<a:lnSpc><a:spcPts val=\"{centi}\"/></a:lnSpc>")))
}

/// The `spc` attribute of a run: the letter spacing in 1/100 pt, after every grapheme. A value off the 1/100 pt grid is
/// rounded, and reported.
pub fn letter_spacing_attr(style: &Style, node_id: &str, losses: &mut Vec<Diagnostic>) -> DltResult<String> {
    let Some(emu) = style.letter_spacing else { return Ok(String::new()) };
    let grid = quantize(emu, 127)?;
    if grid != emu {
        losses.push(loss(node_id, "letter_spacing", "rounded to 1/100 pt"));
    }
    Ok(format!(" spc=\"{}\"", grid / 127))
}

/// The `a:rPr` of a text run. Without an opacity or a letter spacing it is exactly the v1.1 run properties; otherwise
/// the colour carries its alpha and the run carries `spc`.
pub fn run_props_xml(style: &Style, node_id: &str, losses: &mut Vec<Diagnostic>) -> DltResult<String> {
    if style.opacity_milli.is_none() && style.letter_spacing.is_none() {
        return generated_rpr(style);
    }
    let opacity = opacity_of(style.opacity_milli, node_id)?;
    let font = pxml::escape_attr(&style.font_family)?;
    let spc = letter_spacing_attr(style, node_id, losses)?;
    let colour = srgb(&style.color, opacity, node_id, "color", losses)?;
    Ok(format!(
        "<a:rPr lang=\"en-US\" sz=\"{}\" b=\"{}\" i=\"{}\"{spc} dirty=\"0\"><a:solidFill>{colour}</a:solidFill><a:latin typeface=\"{font}\"/><a:ea typeface=\"{font}\"/></a:rPr>",
        centipoints(style.font_size)?,
        u8::from(style.bold),
        u8::from(style.italic),
    ))
}

/// The `a:pPr` of a text paragraph: left aligned, with the exact line height when it is set.
pub fn para_props_xml(style: &Style, node_id: &str, losses: &mut Vec<Diagnostic>) -> DltResult<String> {
    match line_spacing_xml(style, node_id, losses)? {
        None => Ok("<a:pPr algn=\"l\"/>".into()),
        Some(spacing) => Ok(format!("<a:pPr algn=\"l\">{spacing}</a:pPr>")),
    }
}

/// The `bodyPr` anchor of a text box: top, middle, or bottom. The export sizes the box to the text line (see
/// `text_box_xml`), so a middle or bottom anchor cannot move the text. The anchor is written so that the box keeps the
/// intent when edited, and the position is reported as a loss.
pub fn anchor_of(style: &Style, node_id: &str, losses: &mut Vec<Diagnostic>) -> &'static str {
    let (anchor, moves) = match style.vertical_align {
        Some(VerticalAlign::Middle) => ("ctr", true),
        Some(VerticalAlign::Bottom) => ("b", true),
        Some(VerticalAlign::Top) | None => ("t", false),
    };
    if moves {
        losses.push(loss(
            node_id,
            "vertical_align",
            "the text box is sized to its line, so the anchor cannot move the text; it stays at its origin",
        ));
    }
    anchor
}

/// The reason a field is not written for `kind`, or `None` when the kind's writer maps it. Every field is listed, so a
/// new field fails to compile here until it has a mapping or a reason (nothing is dropped silently).
fn unmapped_reason(field: StyleField, kind: Kind) -> Option<&'static str> {
    use StyleField as F;
    match (field, kind) {
        (F::Opacity, _) => None,
        (F::CornerRadius, Kind::Shape) => None,
        (F::CornerRadius, Kind::Text) => Some("text has no corner radius in PowerPoint; not written"),
        (F::CornerRadius, Kind::Path) => Some("a path has no corner radius in PowerPoint; not written"),
        (F::CornerRadius, Kind::Line) => Some("a line has no corner radius in PowerPoint; not written"),
        (F::Shadows, Kind::Text) => Some("a text shadow is not written in this release"),
        (F::Shadows, _) => None,
        (F::Gradient, Kind::Shape | Kind::Path) => None,
        (F::Gradient, Kind::Text) => Some("text has no gradient fill in PowerPoint; not written"),
        (F::Gradient, Kind::Line) => Some("a line has no gradient fill in PowerPoint; not written"),
        (F::LineHeight | F::LetterSpacing | F::VerticalAlign, Kind::Text) => None,
        (F::LineHeight | F::LetterSpacing | F::VerticalAlign, _) => Some("this primitive has no text; not written"),
        (F::FontWeight | F::Underline | F::Strikethrough, _) => Some("not written in this release"),
        (F::StrokeStart | F::StrokeEnd, Kind::Line) => None,
        (F::StrokeStart | F::StrokeEnd, _) => Some("line-end markers apply only to connectors"),
        // The outline fields are written by every kind that has an outline; text has none.
        (F::StrokeDash | F::StrokeCap | F::StrokeJoin, Kind::Text) => Some("text has no outline in PowerPoint; not written"),
        (F::StrokeDash | F::StrokeCap | F::StrokeJoin, _) => None,
        // The v1.1 fields are written by the v1.1 path and never reach this table as unset v1.2 values.
        (F::FontFamily | F::FontSize | F::Color | F::Fill | F::Stroke | F::StrokeWidth | F::Bold | F::Italic | F::Align, _) => None,
    }
}

/// Reports every v1.2 field of `style` that the kind does not write.
pub fn unmapped_losses(style: &Style, kind: Kind, node_id: &str, losses: &mut Vec<Diagnostic>) {
    for field in style.non_default_v12_fields() {
        if let Some(reason) = unmapped_reason(field, kind) {
            losses.push(loss(node_id, field.name(), reason));
        }
    }
}
