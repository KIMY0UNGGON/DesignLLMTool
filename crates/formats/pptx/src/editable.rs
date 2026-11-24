//! Native PresentationML objects of the slide edit path, each with one canonical form.
//!
//! - The outer shadow of a preset rectangle or ellipse: `a:effectLst` with one `a:outerShdw`. The offset lies on one
//!   axis (`dist` and `dir`), `blurRad` is the blur, and the colour is an `a:srgbClr` whose `a:alpha` is
//!   `round(A * 100000 / 255)` for a colour alpha `A`. An alpha of 255 writes no `a:alpha`.
//! - The custom geometry of a vector path: `a:custGeom` with the empty guide lists, the standard rectangle, and one
//!   `a:path` that has only `w` and `h`. Its first command is `moveTo`; the others are `lnTo`, `cubicBezTo`, and `close`,
//!   with integer points.
//!
//! The writer produces these forms. The reader takes a source object as a model value only when it has one of them,
//! and the guard refuses the values these forms cannot hold. Any other source form stays an opaque region: its bytes
//! are kept, and it is not editable.

use designllm_contracts::{Diagnostic, DltError, DltResult, FillRule, PathCommand, PathSpec, Rect, Shadow};
use roxmltree::Node as XNode;

use crate::ns::NS_A;
use crate::v12;
use crate::xmlx::is_el;

/// Largest absolute value of a DrawingML coordinate or length in EMU (ST_Coordinate, ST_PositiveCoordinate).
pub const MAX_COORDINATE: i64 = 27_273_042_316_900;
/// An `a:alpha` value of a fully opaque colour (100000 is 100 %).
const OPAQUE_ALPHA: u32 = 100_000;
/// The rotation of a direction in 1/60000 degree for the four axis directions: right, down, left, up.
const DIR_RIGHT: i64 = 0;
const DIR_DOWN: i64 = 5_400_000;
const DIR_LEFT: i64 = 10_800_000;
const DIR_UP: i64 = 16_200_000;
/// A direction is an angle in `0..21600000` (360 degrees in 1/60000 degree).
const DIR_TURN: i64 = 21_600_000;

/// `#RRGGBB` or `#RRGGBBAA` as its upper-case six digits and its alpha (255 when absent). `None` for any other text.
pub fn parse_colour(colour: &str) -> Option<(String, u8)> {
    let body = colour.strip_prefix('#')?;
    if !body.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    match body.len() {
        6 => Some((body.to_ascii_uppercase(), 255)),
        8 => Some((body[..6].to_ascii_uppercase(), u8::from_str_radix(&body[6..], 16).ok()?)),
        _ => None,
    }
}

/// The shadow with its colour in the stored form: upper case, and without an alpha of 255. The reader returns exactly this
/// form, so a shadow stored in it is equal to its saved and re-read value.
pub fn canonical_shadow(shadow: &Shadow) -> Shadow {
    let color = match parse_colour(&shadow.color) {
        Some((rgb, 255)) => format!("#{rgb}"),
        Some((rgb, alpha)) => format!("#{rgb}{alpha:02X}"),
        None => shadow.color.clone(),
    };
    Shadow { x: shadow.x, y: shadow.y, blur: shadow.blur, spread: shadow.spread, color }
}

/// `a:alpha` of a colour alpha `A` (0..=255): `round(A * 100000 / 255)`, half up.
fn alpha_value(alpha: u8) -> u32 {
    (200_000 * u32::from(alpha) + 255) / 510
}

/// The colour alpha of an `a:alpha` value (0..=100000): `round(value * 255 / 100000)`, half up. The inverse of `alpha_value`
/// for every alpha: the value is within 0.00128 of `A * 100000 / 255`, so the rounding returns `A`.
fn alpha_of(value: u32) -> u8 {
    ((510 * value + 100_000) / 200_000) as u8
}

/// `a:srgbClr` for a colour, with its alpha as `a:alpha` when the colour is not opaque.
fn colour_xml(colour: &str, node_id: &str) -> DltResult<String> {
    let (rgb, alpha) = parse_colour(colour)
        .ok_or_else(|| DltError::invalid(format!("{node_id}: shadow colour {colour:?} must be #RRGGBB or #RRGGBBAA")))?;
    let value = alpha_value(alpha);
    if value >= OPAQUE_ALPHA {
        return Ok(format!("<a:srgbClr val=\"{rgb}\"/>"));
    }
    Ok(format!("<a:srgbClr val=\"{rgb}\"><a:alpha val=\"{value}\"/></a:srgbClr>"))
}

/// `a:effectLst` with the one outer shadow of a shape, or nothing when it has none. A blur is written as `blurRad`, which
/// A blur above 0 adds a loss entry because its appearance may change. The guard has checked the shadow already; a
/// shadow outside the editable form is refused here as well.
pub(crate) fn effect_list_xml(shadows: &[Shadow], node_id: &str, losses: &mut Vec<Diagnostic>) -> DltResult<String> {
    let Some(shadow) = shadows.first() else { return Ok(String::new()) };
    if shadows.len() > 1 {
        return Err(DltError::unsupported(format!("{node_id}: only one shadow can be written")));
    }
    if shadow.blur < 0 {
        return Err(DltError::invalid(format!("{node_id}: a shadow blur cannot be negative")));
    }
    let (dist, dir) = match (shadow.x, shadow.y) {
        (0, 0) => (0, 0),
        (x, 0) => (x.abs(), if x > 0 { DIR_RIGHT } else { DIR_LEFT }),
        (0, y) => (y.abs(), if y > 0 { DIR_DOWN } else { DIR_UP }),
        _ => return Err(DltError::unsupported(format!("{node_id}: a diagonal shadow offset cannot be written exactly"))),
    };
    if shadow.blur > 0 {
        losses.push(v12::loss(node_id, "shadows", "the blur is written as blurRad; its appearance may change"));
    }
    let colour = colour_xml(&shadow.color, node_id)?;
    Ok(format!(
        "<a:effectLst><a:outerShdw blurRad=\"{}\" dist=\"{dist}\" dir=\"{dir}\" algn=\"ctr\" rotWithShape=\"0\">{colour}</a:outerShdw></a:effectLst>",
        shadow.blur
    ))
}

/// The outer shadows of an `a:effectLst`, when the list has the editable form: none, or one `a:outerShdw` of the form
/// `effect_list_xml` writes. An unscaled, unskewed shadow that is not rotated with the shape is the only form read; `algn`
/// does not move such a shadow, so any value is accepted. `None` for any other content, which then stays opaque.
pub(crate) fn read_effect_list(list: XNode<'_, '_>) -> Option<Vec<Shadow>> {
    let mut shadows = Vec::new();
    for node in list.children().filter(|n| n.is_element()) {
        if !is_el(node, NS_A, "outerShdw") {
            return None;
        }
        shadows.push(read_outer_shadow(node)?);
    }
    (shadows.len() <= 1).then_some(shadows)
}

fn read_outer_shadow(node: XNode<'_, '_>) -> Option<Shadow> {
    if node.attribute("sx").is_some_and(|v| v != "100000") || node.attribute("sy").is_some_and(|v| v != "100000") {
        return None;
    }
    if node.attribute("kx").is_some_and(|v| v != "0") || node.attribute("ky").is_some_and(|v| v != "0") {
        return None;
    }
    if node.attribute("rotWithShape") != Some("0") {
        return None;
    }
    let blur = attribute_number(node, "blurRad")?;
    let dist = attribute_number(node, "dist")?;
    let dir = attribute_number(node, "dir")?;
    if !(0..=MAX_COORDINATE).contains(&blur) || !(0..=MAX_COORDINATE).contains(&dist) || !(0..DIR_TURN).contains(&dir) {
        return None;
    }
    let (x, y) = match (dist, dir) {
        (0, _) => (0, 0),
        (d, DIR_RIGHT) => (d, 0),
        (d, DIR_DOWN) => (0, d),
        (d, DIR_LEFT) => (-d, 0),
        (d, DIR_UP) => (0, -d),
        _ => return None,
    };
    let children: Vec<XNode<'_, '_>> = node.children().filter(|n| n.is_element()).collect();
    let [colour] = children.as_slice() else { return None };
    if !is_el(*colour, NS_A, "srgbClr") {
        return None;
    }
    let rgb = colour.attribute("val")?;
    if rgb.len() != 6 || !rgb.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let alpha_children: Vec<XNode<'_, '_>> = colour.children().filter(|n| n.is_element()).collect();
    let alpha = match alpha_children.as_slice() {
        [] => 255,
        [alpha] if is_el(*alpha, NS_A, "alpha") => {
            let value: u32 = alpha.attribute("val")?.parse().ok()?;
            if value > OPAQUE_ALPHA {
                return None;
            }
            // Only the value the writer gives a colour alpha below 255 is read. Any other value, and an explicit opaque
            // alpha (which the writer never writes), would come back as a different XML form, so the list stays as written.
            let colour_alpha = alpha_of(value);
            if colour_alpha == 255 || alpha_value(colour_alpha) != value {
                return None;
            }
            colour_alpha
        }
        _ => return None,
    };
    let color = if alpha == 255 { format!("#{}", rgb.to_ascii_uppercase()) } else { format!("#{}{alpha:02X}", rgb.to_ascii_uppercase()) };
    Some(Shadow { x, y, blur, spread: 0, color })
}

/// An integer attribute, with the schema default 0 when it is absent. `None` when the text does not parse.
fn attribute_number(node: XNode<'_, '_>, name: &str) -> Option<i64> {
    match node.attribute(name) {
        None => Some(0),
        Some(text) => text.parse().ok(),
    }
}

/// `a:custGeom` of a vector path in its editable form, or nothing for any other custom geometry. The writer's form comes from
/// `v12::cust_geom_xml`.
pub(crate) fn read_custom_geometry(geometry: XNode<'_, '_>) -> Option<PathSpec> {
    let parts: Vec<XNode<'_, '_>> = geometry.children().filter(|n| n.is_element()).collect();
    let [av, gd, ah, cxn, rect, path_list] = parts.as_slice() else { return None };
    for (node, name) in [(av, "avLst"), (gd, "gdLst"), (ah, "ahLst"), (cxn, "cxnLst")] {
        if !is_el(*node, NS_A, name) || node.children().any(|c| c.is_element()) {
            return None;
        }
    }
    let standard_rect = is_el(*rect, NS_A, "rect")
        && rect.attribute("l") == Some("l")
        && rect.attribute("t") == Some("t")
        && rect.attribute("r") == Some("r")
        && rect.attribute("b") == Some("b");
    if !standard_rect || !is_el(*path_list, NS_A, "pathLst") {
        return None;
    }
    let paths: Vec<XNode<'_, '_>> = path_list.children().filter(|n| n.is_element()).collect();
    let [path] = paths.as_slice() else { return None };
    if !is_el(*path, NS_A, "path") {
        return None;
    }
    // Only the size is an attribute of the path. The fill, stroke, and extrusion flags keep their defaults.
    if path.attributes().any(|a| a.namespace().is_some() || !matches!(a.name(), "w" | "h")) {
        return None;
    }
    let width: i64 = path.attribute("w")?.parse().ok()?;
    let height: i64 = path.attribute("h")?.parse().ok()?;
    if !(1..=MAX_COORDINATE).contains(&width) || !(1..=MAX_COORDINATE).contains(&height) {
        return None;
    }
    let commands = path.children().filter(|n| n.is_element()).map(read_command).collect::<Option<Vec<_>>>()?;
    // A path starts at a point, so its first command is `moveTo` (the guard refuses any other first command as well).
    if !matches!(commands.first(), Some(PathCommand::MoveTo { .. })) {
        return None;
    }
    Some(PathSpec { view_box: Rect { x: 0, y: 0, width, height }, fill_rule: FillRule::Nonzero, commands })
}

fn read_command(node: XNode<'_, '_>) -> Option<PathCommand> {
    if node.tag_name().namespace() != Some(NS_A) {
        return None;
    }
    let points = node.children().filter(|n| n.is_element()).map(read_point).collect::<Option<Vec<_>>>()?;
    match (node.tag_name().name(), points.as_slice()) {
        ("moveTo", [(x, y)]) => Some(PathCommand::MoveTo { x: *x, y: *y }),
        ("lnTo", [(x, y)]) => Some(PathCommand::LineTo { x: *x, y: *y }),
        ("cubicBezTo", [(x1, y1), (x2, y2), (x, y)]) => {
            Some(PathCommand::CubicTo { x1: *x1, y1: *y1, x2: *x2, y2: *y2, x: *x, y: *y })
        }
        ("close", []) => Some(PathCommand::Close),
        _ => None,
    }
}

/// A point `a:pt` with integer coordinates in the DrawingML range. Guide names, which the empty guide list does not define,
/// are not numbers and are refused.
fn read_point(node: XNode<'_, '_>) -> Option<(i64, i64)> {
    if !is_el(node, NS_A, "pt") {
        return None;
    }
    let x: i64 = node.attribute("x")?.parse().ok()?;
    let y: i64 = node.attribute("y")?.parse().ok()?;
    let in_range = |v: i64| v.unsigned_abs() <= MAX_COORDINATE as u64;
    (in_range(x) && in_range(y)).then_some((x, y))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wrap(fragment: &str) -> String {
        format!("<root xmlns:a=\"{NS_A}\">{fragment}</root>")
    }

    /// Runs `read` on the first element of a fragment that is wrapped in a root element declaring the `a:` prefix.
    fn with_element<R>(fragment: &str, read: impl for<'a, 'i> FnOnce(XNode<'a, 'i>) -> R) -> R {
        let text = wrap(fragment);
        let document = roxmltree::Document::parse(&text).expect("well-formed test document");
        let element = document.root_element().children().find(|n| n.is_element()).expect("an element child");
        read(element)
    }

    #[test]
    fn every_alpha_round_trips_through_its_a_alpha_value() {
        for alpha in 0..=255u8 {
            assert_eq!(alpha_of(alpha_value(alpha)), alpha, "alpha {alpha}");
        }
    }

    #[test]
    fn an_opaque_colour_writes_no_alpha() {
        assert_eq!(colour_xml("#00ff00FF", "n").unwrap(), "<a:srgbClr val=\"00FF00\"/>");
        assert_eq!(parse_colour("#00ff00").unwrap(), ("00FF00".to_string(), 255));
    }

    #[test]
    fn a_shadow_writes_its_axis_offset_and_reads_back_equal() {
        let cases = [
            Shadow { x: 0, y: 0, blur: 0, spread: 0, color: "#000000".into() },
            Shadow { x: 0, y: 2_000, blur: 0, spread: 0, color: "#000000FF".into() },
            Shadow { x: -3_000, y: 0, blur: 4_000, spread: 0, color: "#00000040".into() },
            Shadow { x: 0, y: -5_000, blur: 0, spread: 0, color: "#ABCDEF01".into() },
            Shadow { x: 9_525, y: 0, blur: 0, spread: 0, color: "#ffffff00".into() },
        ];
        for shadow in cases {
            let mut losses = Vec::new();
            let xml = effect_list_xml(std::slice::from_ref(&shadow), "n", &mut losses).unwrap();
            let read = with_element(&xml, read_effect_list).expect("the writer's form is read");
            assert_eq!(read, vec![canonical_shadow(&shadow)], "{xml}");
            assert_eq!(losses.is_empty(), shadow.blur == 0, "a blur is reported: {xml}");
        }
    }

    #[test]
    fn a_shadow_outside_the_editable_form_is_not_read() {
        let forms = [
            // Diagonal direction.
            "<a:effectLst><a:outerShdw dist=\"100\" dir=\"2700000\" rotWithShape=\"0\"><a:srgbClr val=\"000000\"/></a:outerShdw></a:effectLst>",
            // Scaled shadow.
            "<a:effectLst><a:outerShdw dist=\"100\" dir=\"0\" sx=\"50000\" rotWithShape=\"0\"><a:srgbClr val=\"000000\"/></a:outerShdw></a:effectLst>",
            // Not rotated with the shape is required explicitly.
            "<a:effectLst><a:outerShdw dist=\"100\" dir=\"0\"><a:srgbClr val=\"000000\"/></a:outerShdw></a:effectLst>",
            // A second effect in the list.
            "<a:effectLst><a:glow rad=\"1\"/></a:effectLst>",
            // A colour that is not an srgbClr.
            "<a:effectLst><a:outerShdw dist=\"100\" dir=\"0\" rotWithShape=\"0\"><a:schemeClr val=\"tx1\"/></a:outerShdw></a:effectLst>",
        ];
        for form in forms {
            assert!(with_element(form, read_effect_list).is_none(), "{form}");
        }
    }

    #[test]
    fn an_alpha_the_writer_does_not_write_is_not_read() {
        // The writer gives a colour alpha A below 255 the value `alpha_value(A)`, and gives an opaque colour no `a:alpha`.
        // Any other value would be written back in a different form, so the effect list stays as written.
        for value in [12_345, 99_609, 99_999, 100_000] {
            let form = format!(
                "<a:effectLst><a:outerShdw dist=\"100\" dir=\"0\" rotWithShape=\"0\"><a:srgbClr val=\"000000\"><a:alpha val=\"{value}\"/></a:srgbClr></a:outerShdw></a:effectLst>"
            );
            assert!(with_element(&form, read_effect_list).is_none(), "{form}");
        }
        // The value the writer gives alpha 254 is read.
        let form = "<a:effectLst><a:outerShdw dist=\"100\" dir=\"0\" rotWithShape=\"0\"><a:srgbClr val=\"000000\"><a:alpha val=\"99608\"/></a:srgbClr></a:outerShdw></a:effectLst>";
        let expected = Shadow { x: 100, y: 0, blur: 0, spread: 0, color: "#000000FE".into() };
        assert_eq!(with_element(form, read_effect_list), Some(vec![expected]));
    }

    #[test]
    fn an_empty_effect_list_is_the_no_shadow_form() {
        assert_eq!(with_element("<a:effectLst/>", read_effect_list), Some(Vec::new()));
    }

    #[test]
    fn a_path_writes_its_geometry_and_reads_back_equal() {
        let path = PathSpec {
            view_box: Rect { x: 0, y: 0, width: 24_000, height: 12_000 },
            fill_rule: FillRule::Nonzero,
            commands: vec![
                PathCommand::MoveTo { x: 0, y: 0 },
                PathCommand::LineTo { x: 24_000, y: 0 },
                PathCommand::CubicTo { x1: 24_000, y1: 6_000, x2: 12_000, y2: 12_000, x: 0, y: 12_000 },
                PathCommand::Close,
            ],
        };
        let mut losses = Vec::new();
        let xml = v12::cust_geom_xml(&path, "n", &mut losses).unwrap();
        assert!(losses.is_empty(), "one subpath with the nonzero rule has no loss");
        assert_eq!(with_element(&xml, read_custom_geometry), Some(path));
    }

    #[test]
    fn a_path_with_extra_geometry_is_not_read() {
        let forms = [
            "<a:custGeom><a:avLst/><a:gdLst><a:gd name=\"g\" fmla=\"val 1\"/></a:gdLst><a:ahLst/><a:cxnLst/><a:rect l=\"l\" t=\"t\" r=\"r\" b=\"b\"/><a:pathLst><a:path w=\"10\" h=\"10\"><a:moveTo><a:pt x=\"0\" y=\"0\"/></a:moveTo><a:close/></a:path></a:pathLst></a:custGeom>",
            "<a:custGeom><a:avLst/><a:gdLst/><a:ahLst/><a:cxnLst/><a:rect l=\"l\" t=\"t\" r=\"r\" b=\"b\"/><a:pathLst><a:path w=\"10\" h=\"10\" fill=\"none\"><a:moveTo><a:pt x=\"0\" y=\"0\"/></a:moveTo><a:close/></a:path></a:pathLst></a:custGeom>",
            "<a:custGeom><a:avLst/><a:gdLst/><a:ahLst/><a:cxnLst/><a:rect l=\"l\" t=\"t\" r=\"r\" b=\"b\"/><a:pathLst><a:path w=\"10\" h=\"10\"><a:moveTo><a:pt x=\"w\" y=\"0\"/></a:moveTo><a:close/></a:path></a:pathLst></a:custGeom>",
            "<a:custGeom><a:avLst/><a:gdLst/><a:ahLst/><a:cxnLst/><a:rect l=\"l\" t=\"t\" r=\"r\" b=\"b\"/><a:pathLst><a:path w=\"10\" h=\"10\"><a:quadBezTo><a:pt x=\"0\" y=\"0\"/><a:pt x=\"1\" y=\"1\"/></a:quadBezTo></a:path></a:pathLst></a:custGeom>",
        ];
        for form in forms {
            assert!(with_element(form, read_custom_geometry).is_none(), "{form}");
        }
    }

    #[test]
    fn a_path_that_does_not_start_with_move_to_is_not_read() {
        let form = "<a:custGeom><a:avLst/><a:gdLst/><a:ahLst/><a:cxnLst/><a:rect l=\"l\" t=\"t\" r=\"r\" b=\"b\"/><a:pathLst><a:path w=\"10\" h=\"10\"><a:lnTo><a:pt x=\"0\" y=\"0\"/></a:lnTo><a:close/></a:path></a:pathLst></a:custGeom>";
        assert!(with_element(form, read_custom_geometry).is_none(), "{form}");
    }

    #[test]
    fn a_view_box_origin_is_refused_rather_than_dropped() {
        let path = PathSpec {
            view_box: Rect { x: 1_000, y: 0, width: 24_000, height: 12_000 },
            fill_rule: FillRule::Nonzero,
            commands: vec![PathCommand::MoveTo { x: 0, y: 0 }, PathCommand::Close],
        };
        let mut losses = Vec::new();
        assert!(v12::cust_geom_xml(&path, "n", &mut losses).is_err(), "an origin would move every point");
    }

    #[test]
    fn canonical_shadow_drops_an_opaque_alpha_and_upper_cases_the_colour() {
        let shadow = canonical_shadow(&Shadow { x: 0, y: 1, blur: 0, spread: 0, color: "#abcdefff".into() });
        assert_eq!(shadow.color, "#ABCDEF");
        let shadow = canonical_shadow(&Shadow { x: 0, y: 1, blur: 0, spread: 0, color: "#abcdef40".into() });
        assert_eq!(shadow.color, "#ABCDEF40");
    }
}
