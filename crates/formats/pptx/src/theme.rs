//! Theme colours and fonts, colour-map lookup, colour modifiers, and theme edits.
//!
//! Resolution produces an explicit `#RRGGBB` value. The model stores that value, and the writer
//! compares resolved values, so an unedited scheme colour is never rewritten. Modifiers that cannot be
//! applied exactly are approximated and reported through `render_notes`.
//!
//! A theme edit (`SetTheme`) rewrites only the colour slot and font face elements it names, in the theme of the
//! slide master and of the notes master (`theme_parts`). Every other byte of those parts is kept.

use std::collections::BTreeMap;
use std::ops::Range;

use designllm_contracts::{DltError, DltResult, Style, ThemePatch, THEME_COLOR_SLOTS};
use designllm_package::opc;
use designllm_package::xml::{self as pxml, Splice};
use designllm_package::Package;
use roxmltree::{Document, Node};

use crate::ns::{self, NS_A, NS_P, NS_R};
use crate::xmlx::{attr_i64, child, children, is_el, malformed, span};

/// A colour as written in the source: an sRGB or system value, or a scheme reference, plus modifiers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ColorSpec {
    pub base: ColorBase,
    /// Modifier name and value, in document order (for example `("lumMod", 75000)`).
    pub modifiers: Vec<(String, i64)>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ColorBase {
    /// `a:srgbClr/@val`, uppercase hex.
    Srgb(String),
    /// `a:schemeClr/@val`, for example `accent1`, `bg1`, or `phClr`.
    Scheme(String),
    /// `a:sysClr/@lastClr`, uppercase hex.
    System(String),
}

/// Theme fill style from `a:fmtScheme/a:fillStyleLst`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FillStyle {
    Solid(ColorSpec),
    /// Gradient, pattern, or picture fills. Named for diagnostics.
    Other(String),
}

/// Theme line style from `a:fmtScheme/a:lnStyleLst`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LineStyle {
    pub width: Option<i64>,
    pub color: Option<ColorSpec>,
}

/// Theme colours, fonts, and style matrix.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Theme {
    /// Colour name (`dk1`, `lt1`, `accent1`, ...) to uppercase hex.
    pub colors: BTreeMap<String, String>,
    pub major_latin: String,
    pub minor_latin: String,
    /// East Asian faces (`a:ea`) of the major and minor fonts. Empty when the theme names none.
    pub major_ea: String,
    pub minor_ea: String,
    pub fill_styles: Vec<FillStyle>,
    pub line_styles: Vec<LineStyle>,
}

/// Maps the logical colour names that slides use (`bg1`, `tx1`, ...) to theme colour names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ColorMap {
    pub map: BTreeMap<String, String>,
}

impl Default for ColorMap {
    fn default() -> Self {
        let map = [
            ("bg1", "lt1"),
            ("tx1", "dk1"),
            ("bg2", "lt2"),
            ("tx2", "dk2"),
        ]
        .iter()
        .map(|(a, b)| (a.to_string(), b.to_string()))
        .collect();
        Self { map }
    }
}

impl ColorMap {
    /// Reads `p:clrMap` attributes. Missing attributes keep the standard mapping.
    pub fn from_clr_map(node: Node<'_, '_>) -> Self {
        let mut out = Self::default();
        for attribute in node.attributes() {
            out.map.insert(attribute.name().to_string(), attribute.value().to_string());
        }
        out
    }
}

/// Everything needed to turn a colour spec into hex.
pub struct ColorContext<'a> {
    pub theme: &'a Theme,
    pub map: &'a ColorMap,
    /// Colour that `phClr` resolves to inside a style reference, as uppercase hex.
    pub placeholder: Option<String>,
}

/// Result of resolving a colour: the hex value, or `None` when the reference cannot be resolved,
/// and notes for modifiers that were approximated or ignored.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resolved {
    pub hex: Option<String>,
    pub notes: Vec<String>,
}

impl Theme {
    /// Parses a theme part.
    pub fn parse(xml: &[u8]) -> DltResult<Self> {
        let (doc, _) = crate::xmlx::parse(xml)?;
        let root = doc.root_element();
        if root.tag_name().name() != "theme" {
            return Err(malformed("theme part root is not a:theme"));
        }
        let mut theme = Theme::default();
        let Some(elements) = child(root, NS_A, "themeElements") else {
            return Err(malformed("theme has no themeElements"));
        };
        if let Some(scheme) = child(elements, NS_A, "clrScheme") {
            for node in scheme.children().filter(|n| n.is_element()) {
                let name = node.tag_name().name().to_string();
                let spec = node.children().find(|c| c.is_element()).and_then(parse_color_element);
                if let Some(hex) = spec.as_ref().and_then(base_hex) {
                    theme.colors.insert(name, hex);
                }
            }
        }
        if let Some(fonts) = child(elements, NS_A, "fontScheme") {
            theme.major_latin = font_face(child(fonts, NS_A, "majorFont"), "latin");
            theme.minor_latin = font_face(child(fonts, NS_A, "minorFont"), "latin");
            theme.major_ea = font_face(child(fonts, NS_A, "majorFont"), "ea");
            theme.minor_ea = font_face(child(fonts, NS_A, "minorFont"), "ea");
        }
        if let Some(fmt) = child(elements, NS_A, "fmtScheme") {
            if let Some(list) = child(fmt, NS_A, "fillStyleLst") {
                for fill in list.children().filter(|n| n.is_element()) {
                    theme.fill_styles.push(parse_fill_style(fill));
                }
            }
            if let Some(list) = child(fmt, NS_A, "lnStyleLst") {
                for line in children(list, NS_A, "ln") {
                    theme.line_styles.push(LineStyle {
                        width: attr_i64(line, "w"),
                        color: child_color(line),
                    });
                }
            }
        }
        Ok(theme)
    }

    /// Resolves a colour spec in this theme.
    pub fn resolve(&self, spec: &ColorSpec, context: &ColorContext<'_>) -> Resolved {
        let mut notes = Vec::new();
        let base_hex = match &spec.base {
            ColorBase::Srgb(hex) | ColorBase::System(hex) => Some(hex.clone()),
            ColorBase::Scheme(name) => {
                if name == "phClr" {
                    context.placeholder.clone()
                } else {
                    let theme_name = context.map.map.get(name.as_str()).cloned().unwrap_or_else(|| name.clone());
                    self.colors.get(&theme_name).cloned()
                }
            }
        };
        let Some(mut rgb) = base_hex.and_then(|h| parse_hex(&h)) else {
            return Resolved { hex: None, notes };
        };
        for (modifier, value) in &spec.modifiers {
            match modifier.as_str() {
                "lumMod" | "lumOff" => {
                    let (h, s, l) = rgb_to_hsl(rgb);
                    let l2 = match modifier.as_str() {
                        "lumMod" => l * (*value as f64) / 100_000.0,
                        _ => l + (*value as f64) / 100_000.0,
                    };
                    rgb = hsl_to_rgb(h, s, l2.clamp(0.0, 1.0));
                }
                "tint" => {
                    let t = (*value as f64 / 100_000.0).clamp(0.0, 1.0);
                    rgb = rgb.map(|c| (c as f64 * t + 255.0 * (1.0 - t)).round() as u8);
                    notes.push("color_tint_approximated".to_string());
                }
                "shade" => {
                    let s = (*value as f64 / 100_000.0).clamp(0.0, 1.0);
                    rgb = rgb.map(|c| (c as f64 * s).round() as u8);
                    notes.push("color_shade_approximated".to_string());
                }
                other => notes.push(format!("color_modifier_{other}_ignored")),
            }
        }
        Resolved { hex: Some(format!("#{:02X}{:02X}{:02X}", rgb[0], rgb[1], rgb[2])), notes }
    }
}

/// Parses a colour element (`srgbClr`, `schemeClr`, `sysClr`) and its modifier children.
pub fn parse_color_element(node: Node<'_, '_>) -> Option<ColorSpec> {
    if node.tag_name().namespace() != Some(NS_A) {
        return None;
    }
    let base = match node.tag_name().name() {
        "srgbClr" => ColorBase::Srgb(node.attribute("val")?.to_ascii_uppercase()),
        "schemeClr" => ColorBase::Scheme(node.attribute("val")?.to_string()),
        "sysClr" => ColorBase::System(node.attribute("lastClr")?.to_ascii_uppercase()),
        _ => return None,
    };
    let modifiers = node
        .children()
        .filter(|c| c.is_element())
        .filter_map(|c| Some((c.tag_name().name().to_string(), attr_i64(c, "val")?)))
        .collect();
    Some(ColorSpec { base, modifiers })
}

/// The colour element that is a direct child of `node` (for example the `solidFill` colour), if any.
pub fn child_color(node: Node<'_, '_>) -> Option<ColorSpec> {
    let fill = child(node, NS_A, "solidFill")?;
    parse_color_element(fill.children().find(|c| c.is_element())?)
}

fn base_hex(spec: &ColorSpec) -> Option<String> {
    match &spec.base {
        ColorBase::Srgb(hex) | ColorBase::System(hex) if hex.len() == 6 => Some(hex.clone()),
        _ => None,
    }
}

/// `typeface` of the `a:latin` or `a:ea` (`local`) child of a major or minor font.
fn font_face(font: Option<Node<'_, '_>>, local: &str) -> String {
    font.and_then(|f| child(f, NS_A, local))
        .and_then(|l| l.attribute("typeface"))
        .unwrap_or("")
        .to_string()
}

fn parse_fill_style(node: Node<'_, '_>) -> FillStyle {
    if is_el(node, NS_A, "solidFill") {
        if let Some(spec) = node.children().find(|c| c.is_element()).and_then(parse_color_element) {
            return FillStyle::Solid(spec);
        }
    }
    FillStyle::Other(node.tag_name().name().to_string())
}

fn parse_hex(hex: &str) -> Option<[u8; 3]> {
    if hex.len() != 6 {
        return None;
    }
    let v = u32::from_str_radix(hex, 16).ok()?;
    Some([((v >> 16) & 0xFF) as u8, ((v >> 8) & 0xFF) as u8, (v & 0xFF) as u8])
}

fn rgb_to_hsl(rgb: [u8; 3]) -> (f64, f64, f64) {
    let r = f64::from(rgb[0]) / 255.0;
    let g = f64::from(rgb[1]) / 255.0;
    let b = f64::from(rgb[2]) / 255.0;
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let l = (max + min) / 2.0;
    if (max - min).abs() < f64::EPSILON {
        return (0.0, 0.0, l);
    }
    let d = max - min;
    let s = if l > 0.5 { d / (2.0 - max - min) } else { d / (max + min) };
    let h = if (max - r).abs() < f64::EPSILON {
        (g - b) / d + if g < b { 6.0 } else { 0.0 }
    } else if (max - g).abs() < f64::EPSILON {
        (b - r) / d + 2.0
    } else {
        (r - g) / d + 4.0
    };
    (h / 6.0, s, l)
}

fn hsl_to_rgb(h: f64, s: f64, l: f64) -> [u8; 3] {
    if s == 0.0 {
        let v = (l * 255.0).round() as u8;
        return [v, v, v];
    }
    let q = if l < 0.5 { l * (1.0 + s) } else { l + s - l * s };
    let p = 2.0 * l - q;
    let hue = |t: f64| {
        let mut t = t;
        if t < 0.0 {
            t += 1.0;
        }
        if t > 1.0 {
            t -= 1.0;
        }
        if t < 1.0 / 6.0 {
            p + (q - p) * 6.0 * t
        } else if t < 0.5 {
            q
        } else if t < 2.0 / 3.0 {
            p + (q - p) * (2.0 / 3.0 - t) * 6.0
        } else {
            p
        }
    };
    [
        (hue(h + 1.0 / 3.0) * 255.0).round() as u8,
        (hue(h) * 255.0).round() as u8,
        (hue(h - 1.0 / 3.0) * 255.0).round() as u8,
    ]
}

/// Normalizes a `#RRGGBB` or `RRGGBB` string to `#RRGGBB`. Returns `None` for other input.
pub fn normalize_hex(value: &str) -> Option<String> {
    let trimmed = value.trim().trim_start_matches('#');
    if trimmed.len() == 6 && trimmed.chars().all(|c| c.is_ascii_hexdigit()) {
        Some(format!("#{}", trimmed.to_ascii_uppercase()))
    } else {
        None
    }
}

/// Font face used when neither the run nor the theme names one.
pub const FALLBACK_FONT: &str = "Pretendard";

/// Builds the explicit font family value for a run from its `latin` element and theme.
pub fn font_from_theme(typeface: &str, theme: &Theme) -> String {
    match typeface {
        "+mj-lt" => non_empty_or(&theme.major_latin),
        "+mn-lt" => non_empty_or(&theme.minor_latin),
        "" => non_empty_or(&theme.minor_latin),
        other => other.to_string(),
    }
}

fn non_empty_or(value: &str) -> String {
    if value.is_empty() {
        FALLBACK_FONT.to_string()
    } else {
        value.to_string()
    }
}

/// Copies the font fields of a style into a fresh run style with neutral shape-level fields.
pub fn run_style(font_family: String, font_size: i64, color: String, bold: bool, italic: bool, align: &str) -> Style {
    Style {
        font_family,
        font_size,
        color,
        fill: "none".into(),
        stroke: "none".into(),
        stroke_width: 0,
        bold,
        italic,
        align: align.to_string(),
        ..Style::default()
    }
}

/// Longest theme font face, in characters, that `checked_patch` accepts.
pub const THEME_FONT_MAX_CHARS: usize = 64;

/// The font fields of a patch: field name, font (`majorFont`/`minorFont`), face element (`latin`/`ea`), and value.
fn font_fields(patch: &ThemePatch) -> [(&'static str, &'static str, &'static str, Option<&str>); 4] {
    [
        ("major_latin", "majorFont", "latin", patch.major_latin.as_deref()),
        ("minor_latin", "minorFont", "latin", patch.minor_latin.as_deref()),
        ("major_ea", "majorFont", "ea", patch.major_ea.as_deref()),
        ("minor_ea", "minorFont", "ea", patch.minor_ea.as_deref()),
    ]
}

/// The patch with every colour written as uppercase `#RRGGBB`. Refused with INVALID_ARGUMENT: an empty patch, a slot
/// outside `THEME_COLOR_SLOTS`, a colour that is not `#RRGGBB`, and a font face that is empty, longer than
/// `THEME_FONT_MAX_CHARS`, or holds a control character, `<`, or `&`.
pub fn checked_patch(patch: &ThemePatch) -> DltResult<ThemePatch> {
    if patch.is_empty() {
        return Err(DltError::invalid("set_theme needs at least one colour or font"));
    }
    let mut out = patch.clone();
    for (slot, value) in out.colors.iter_mut() {
        if !THEME_COLOR_SLOTS.contains(&slot.as_str()) {
            return Err(DltError::invalid(format!("theme colour {slot:?} is not one of {}", THEME_COLOR_SLOTS.join(", "))));
        }
        // `normalize_hex` also takes a value without `#` or with spaces; a theme colour is exactly `#RRGGBB`.
        let normalized = (value.len() == 7 && value.starts_with('#')).then(|| normalize_hex(value)).flatten();
        let Some(normalized) = normalized else {
            return Err(DltError::invalid(format!("theme colour {slot} {value:?} must be #RRGGBB")));
        };
        *value = normalized;
    }
    for (field, _, _, value) in font_fields(&out) {
        let Some(face) = value else { continue };
        if face.trim().is_empty() {
            return Err(DltError::invalid(format!("theme font {field} cannot be empty")));
        }
        if face.chars().count() > THEME_FONT_MAX_CHARS {
            return Err(DltError::invalid(format!("theme font {field} is longer than {THEME_FONT_MAX_CHARS} characters")));
        }
        if face.chars().any(|c| c.is_control() || c == '<' || c == '&') || pxml::check_xml_chars(face).is_err() {
            return Err(DltError::invalid(format!("theme font {field} cannot hold a control character, < or &")));
        }
    }
    Ok(out)
}

/// `theme` with the values of `patch`: what a theme part reads as after the patch is written into it.
pub fn edited_theme(mut theme: Theme, patch: &ThemePatch) -> Theme {
    for (slot, value) in &patch.colors {
        theme.colors.insert(slot.clone(), value.trim_start_matches('#').to_ascii_uppercase());
    }
    for (target, value) in [
        (&mut theme.major_latin, &patch.major_latin), (&mut theme.minor_latin, &patch.minor_latin),
        (&mut theme.major_ea, &patch.major_ea), (&mut theme.minor_ea, &patch.minor_ea),
    ] {
        if let Some(value) = value {
            value.clone_into(target);
        }
    }
    theme
}

/// The theme part of the slide master, then the theme part of each notes master that is a different part. A deck
/// with more than one slide master, or a master without a theme, is refused (UNSUPPORTED_OPERATION): a theme edit
/// names one theme.
pub fn theme_parts(package: &Package) -> DltResult<Vec<String>> {
    let (doc, _) = crate::xmlx::parse(package.part(ns::PRESENTATION_PART)?)?;
    let root = doc.root_element();
    let rels = opc::parse_rels(package.part(ns::PRESENTATION_RELS_PART)?)?;
    let masters = child(root, NS_P, "sldMasterIdLst").map(|list| children(list, NS_P, "sldMasterId")).unwrap_or_default();
    let [master] = masters.as_slice() else {
        return Err(DltError::unsupported(format!("a theme edit needs a deck with one slide master; this deck has {}", masters.len())));
    };
    let master = listed_part(&rels, *master)?;
    let mut parts = vec![related_theme(package, &master)?.ok_or_else(|| DltError::unsupported("the slide master has no theme to edit"))?];
    // The schema allows one notes master; every listed one is taken, so none is left unedited or unchecked.
    let notes = child(root, NS_P, "notesMasterIdLst").map(|list| children(list, NS_P, "notesMasterId")).unwrap_or_default();
    for entry in notes {
        if let Some(theme) = related_theme(package, &listed_part(&rels, entry)?)? {
            if !parts.contains(&theme) {
                parts.push(theme);
            }
        }
    }
    Ok(parts)
}

/// The part that a presentation list entry (`p:sldMasterId`, `p:notesMasterId`) names through its relationship.
fn listed_part(rels: &[opc::Relationship], entry: Node<'_, '_>) -> DltResult<String> {
    let id = entry.attribute((NS_R, "id")).ok_or_else(|| malformed("a master entry has no r:id"))?;
    let rel = rels.iter().find(|r| r.id == id).ok_or_else(|| malformed(format!("relationship {id} is missing")))?;
    opc::resolve_target(ns::PRESENTATION_PART, &rel.target)
}

fn related_theme(package: &Package, owner: &str) -> DltResult<Option<String>> {
    let Ok(bytes) = package.part(&opc::rels_part_for(owner)) else { return Ok(None) };
    opc::parse_rels(bytes)?
        .iter()
        .find(|r| r.rel_type == ns::REL_THEME && !r.external)
        .map(|r| opc::resolve_target(owner, &r.target))
        .transpose()
}

/// One element that a patch rewrites: a colour slot gets the colour, a font face element gets the face.
enum ThemeEdit<'p> {
    Color(&'p str),
    Face(&'p str),
}

/// The elements `patch` rewrites, in document order: `a:clrScheme/a:<slot>` for each colour and
/// `a:fontScheme/a:majorFont|a:minorFont/a:latin|a:ea` for each face. A missing element is refused.
fn theme_edits<'p>(doc: &Document<'_>, offset: usize, patch: &'p ThemePatch) -> DltResult<Vec<(Range<usize>, ThemeEdit<'p>)>> {
    let root = doc.root_element();
    if !is_el(root, NS_A, "theme") {
        return Err(malformed("theme part root is not a:theme"));
    }
    let elements = child(root, NS_A, "themeElements").ok_or_else(|| malformed("theme has no themeElements"))?;
    let mut out = Vec::new();
    if !patch.colors.is_empty() {
        let scheme = child(elements, NS_A, "clrScheme").ok_or_else(|| DltError::unsupported("the theme has no colour scheme"))?;
        for (slot, value) in &patch.colors {
            let element = child(scheme, NS_A, slot).ok_or_else(|| DltError::unsupported(format!("the theme colour scheme has no {slot}")))?;
            out.push((span(element, offset), ThemeEdit::Color(value)));
        }
    }
    for (field, font, face, value) in font_fields(patch) {
        let Some(value) = value else { continue };
        let element = child(elements, NS_A, "fontScheme")
            .and_then(|scheme| child(scheme, NS_A, font))
            .and_then(|font| child(font, NS_A, face))
            .ok_or_else(|| DltError::unsupported(format!("the theme font scheme has no {font} {face} face for {field}")))?;
        out.push((span(element, offset), ThemeEdit::Face(value)));
    }
    out.sort_by_key(|(range, _)| range.start);
    Ok(out)
}

/// Byte ranges of the elements `patch` rewrites in a theme part, in document order. Bytes outside them are the bytes
/// a theme edit keeps.
pub fn edited_element_ranges(xml: &[u8], patch: &ThemePatch) -> DltResult<Vec<Range<usize>>> {
    let (doc, offset) = crate::xmlx::parse(xml)?;
    Ok(theme_edits(&doc, offset, patch)?.into_iter().map(|(range, _)| range).collect())
}

/// Writes `patch` into a theme part. A colour slot's content becomes one `a:srgbClr`, and a face is set as the
/// `typeface` of its `a:latin` or `a:ea`. Every other byte is kept.
pub fn splice_theme(xml: &[u8], patch: &ThemePatch) -> DltResult<Vec<u8>> {
    let patch = checked_patch(patch)?;
    let (doc, offset) = crate::xmlx::parse(xml)?;
    let mut splices = Vec::new();
    for (range, edit) in theme_edits(&doc, offset, &patch)? {
        match edit {
            ThemeEdit::Color(hex) => {
                // The slot's own prefix is bound to the DrawingML namespace, the namespace of `srgbClr`.
                let name = pxml::qname(xml, range.start)?;
                let prefix = name.rsplit_once(':').map_or(String::new(), |(prefix, _)| format!("{prefix}:"));
                let fragment = format!("<{prefix}srgbClr val=\"{}\"/>", hex.trim_start_matches('#'));
                splices.push(match pxml::content_range(xml, &range)? {
                    Some(content) => Splice { range: content, replacement: fragment.into_bytes() },
                    None => pxml::insert_child(xml, &range, None, &fragment)?,
                });
            }
            ThemeEdit::Face(face) => splices.push(pxml::set_attribute(xml, &range, "typeface", face)?),
        }
    }
    pxml::apply_checked(xml, splices)
}

/// `package` with `patch` written into each part of `theme_parts`.
pub fn with_theme(mut package: Package, patch: &ThemePatch) -> DltResult<Package> {
    for part in theme_parts(&package)? {
        let bytes = splice_theme(package.part(&part)?, patch)?;
        package.set_part(&part, bytes)?;
    }
    Ok(package)
}

/// Writes into `target` the theme parts of `original` with `patch`: `target` is a copy of `original` whose other parts
/// are kept. One copy can then take several patches in turn, each from the original theme bytes.
pub fn write_theme_from(target: &mut Package, original: &Package, patch: &ThemePatch) -> DltResult<()> {
    for part in theme_parts(original)? {
        target.set_part(&part, splice_theme(original.part(&part)?, patch)?)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_and_hsl_round_trip_is_close() {
        let rgb = [0x31, 0x5A, 0x7D];
        let (h, s, l) = rgb_to_hsl(rgb);
        let back = hsl_to_rgb(h, s, l);
        for (a, b) in rgb.iter().zip(back.iter()) {
            assert!((i32::from(*a) - i32::from(*b)).abs() <= 1, "{rgb:?} vs {back:?}");
        }
    }

    #[test]
    fn normalize_hex_accepts_only_six_digits() {
        assert_eq!(normalize_hex("17365d").as_deref(), Some("#17365D"));
        assert_eq!(normalize_hex("#17365D").as_deref(), Some("#17365D"));
        assert_eq!(normalize_hex("1736"), None);
        assert_eq!(normalize_hex("GGGGGG"), None);
    }

    #[test]
    fn scheme_colour_resolves_through_colour_map_and_reports_unhandled_modifiers() {
        let theme = Theme {
            colors: BTreeMap::from([("dk1".to_string(), "000000".to_string()), ("accent1".to_string(), "315A7D".to_string())]),
            ..Theme::default()
        };
        let map = ColorMap::default();
        let context = ColorContext { theme: &theme, map: &map, placeholder: None };
        let tx1 = theme.resolve(&ColorSpec { base: ColorBase::Scheme("tx1".into()), modifiers: vec![] }, &context);
        assert_eq!(tx1.hex.as_deref(), Some("#000000"));
        let accent = theme.resolve(
            &ColorSpec { base: ColorBase::Scheme("accent1".into()), modifiers: vec![("alpha".into(), 50_000)] },
            &context,
        );
        assert_eq!(accent.hex.as_deref(), Some("#315A7D"));
        assert_eq!(accent.notes, vec!["color_modifier_alpha_ignored".to_string()]);
        let unknown = theme.resolve(&ColorSpec { base: ColorBase::Scheme("accent9".into()), modifiers: vec![] }, &context);
        assert_eq!(unknown.hex, None);
    }

    fn patch(colors: &[(&str, &str)]) -> ThemePatch {
        ThemePatch { colors: colors.iter().map(|(slot, value)| (slot.to_string(), value.to_string())).collect(), ..ThemePatch::default() }
    }

    fn blank_theme() -> Vec<u8> {
        let deck = Package::open(&crate::blank_deck("theme", designllm_contracts::Size::slide()).unwrap()).unwrap();
        deck.part("ppt/theme/theme1.xml").unwrap().to_vec()
    }

    #[test]
    fn theme_patch_check_normalizes_colours_and_refuses_bad_values() {
        let checked = checked_patch(&patch(&[("accent1", "#a1b2c3")])).unwrap();
        assert_eq!(checked.colors["accent1"], "#A1B2C3");
        let face = |value: &str| ThemePatch { minor_ea: Some(value.into()), ..ThemePatch::default() };
        for bad in [
            ThemePatch::default(),
            patch(&[("accent7", "#000000")]),
            patch(&[("accent1", "A1B2C3")]),
            patch(&[("accent1", "#A1B2C")]),
            patch(&[("accent1", "#GGGGGG")]),
            face(""),
            face("   "),
            face(&"가".repeat(THEME_FONT_MAX_CHARS + 1)),
            face("A\u{0007}B"),
            face("A<B"),
            face("A&B"),
        ] {
            let error = checked_patch(&bad).expect_err("refused");
            assert_eq!(error.code, designllm_contracts::ErrorCode::InvalidArgument, "{bad:?}");
        }
        assert!(checked_patch(&face(&"가".repeat(THEME_FONT_MAX_CHARS))).is_ok());
    }

    #[test]
    fn theme_splice_rewrites_only_the_named_elements() {
        let original = blank_theme();
        let edit = ThemePatch { minor_ea: Some("맑은 \"고딕\"".into()), ..patch(&[("dk1", "#102030"), ("accent1", "#a1b2c3")]) };
        let saved = splice_theme(&original, &edit).unwrap();
        let before = edited_element_ranges(&original, &edit).unwrap();
        let after = edited_element_ranges(&saved, &edit).unwrap();
        assert_eq!(before.len(), 3);
        let outside = |xml: &[u8], ranges: &[Range<usize>]| {
            let mut parts = Vec::new();
            let mut cursor = 0;
            for range in ranges {
                parts.push(xml[cursor..range.start].to_vec());
                cursor = range.end;
            }
            parts.push(xml[cursor..].to_vec());
            parts
        };
        assert_eq!(outside(&original, &before), outside(&saved, &after), "bytes outside the edited elements are kept");
        let text = String::from_utf8(saved.clone()).unwrap();
        assert!(text.contains("<a:dk1><a:srgbClr val=\"102030\"/></a:dk1>"), "a system colour slot becomes sRGB: {text}");
        assert!(text.contains("<a:accent1><a:srgbClr val=\"A1B2C3\"/></a:accent1>"));
        let theme = Theme::parse(&saved).unwrap();
        assert_eq!(theme, edited_theme(Theme::parse(&original).unwrap(), &edit));
        assert_eq!(theme.minor_ea, "맑은 \"고딕\"");
        assert_eq!((theme.major_ea.as_str(), theme.minor_latin.as_str()), ("", "Pretendard"));
    }

    #[test]
    fn theme_splice_keeps_the_prefix_of_the_scheme_and_refuses_a_missing_slot() {
        let xml = br#"<theme xmlns="http://schemas.openxmlformats.org/drawingml/2006/main"><themeElements><clrScheme name="x"><accent1/></clrScheme><fontScheme name="x"><majorFont><latin typeface="A"/></majorFont><minorFont><latin typeface="B"/></minorFont></fontScheme></themeElements></theme>"#;
        let saved = splice_theme(xml, &patch(&[("accent1", "#010203")])).unwrap();
        assert!(String::from_utf8(saved).unwrap().contains("<accent1><srgbClr val=\"010203\"/></accent1>"));
        let missing = splice_theme(xml, &patch(&[("accent2", "#010203")])).expect_err("no accent2 slot");
        assert_eq!(missing.code, designllm_contracts::ErrorCode::UnsupportedOperation);
        let no_ea = splice_theme(xml, &ThemePatch { major_ea: Some("C".into()), ..ThemePatch::default() }).expect_err("no a:ea");
        assert_eq!(no_ea.code, designllm_contracts::ErrorCode::UnsupportedOperation);
    }

    #[test]
    fn theme_parts_of_a_blank_deck_is_its_master_theme() {
        let deck = Package::open(&crate::blank_deck("theme", designllm_contracts::Size::slide()).unwrap()).unwrap();
        assert_eq!(theme_parts(&deck).unwrap(), vec!["ppt/theme/theme1.xml".to_string()]);
        let edited = with_theme(deck.clone(), &patch(&[("accent2", "#000001")])).unwrap();
        assert_eq!(Theme::parse(edited.part("ppt/theme/theme1.xml").unwrap()).unwrap().colors["accent2"], "000001");
        assert_eq!(edited.part("ppt/slideMasters/slideMaster1.xml").unwrap(), deck.part("ppt/slideMasters/slideMaster1.xml").unwrap());
    }
}
