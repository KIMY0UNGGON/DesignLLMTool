//! Direct run and paragraph properties: parse from `w:rPr`/`w:pPr`, and rewrite the modelled
//! children while keeping every other child verbatim and in OOXML schema order.

use std::collections::BTreeSet;

use designllm_contracts::{DltResult, ParagraphFormat, Style};
use roxmltree::Node;
use serde::{Deserialize, Serialize};

use crate::tree::{self, is_w, w_attr, Part};
use crate::units;

/// Modelled run properties. `None` means "not specified directly by this element".
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RunProps {
    pub font_family: Option<String>,
    pub font_size: Option<i64>,
    pub color: Option<String>,
    pub bold: Option<bool>,
    pub italic: Option<bool>,
}

/// Direct-formatting names used in the native mapping of a run.
pub const RUN_NAMES: [&str; 5] = ["font_family", "font_size", "color", "bold", "italic"];
/// Direct-formatting names used in the native mapping of a paragraph.
pub const PARA_NAMES: [&str; 5] = ["align", "space_before", "space_after", "line_spacing", "indent"];

/// ST_OnOff: an element with no `w:val` is on; `0`, `false` or `off` is off.
pub fn on_off(val: Option<&str>) -> bool {
    !matches!(val.map(|v| v.trim().to_ascii_lowercase()).as_deref(), Some("0") | Some("false") | Some("off"))
}

impl RunProps {
    /// Parses the modelled properties of a `w:rPr` (or none).
    pub fn parse(rpr: Option<Node<'_, '_>>) -> Self {
        let mut out = Self::default();
        let Some(rpr) = rpr else { return out };
        for child in tree::elements(rpr) {
            if is_w(child, "rFonts") {
                let family = ["ascii", "hAnsi", "eastAsia", "cs"]
                    .iter()
                    .find_map(|a| w_attr(child, a).filter(|v| !v.is_empty()));
                out.font_family = family.map(str::to_string);
            } else if is_w(child, "b") {
                out.bold = Some(on_off(w_attr(child, "val")));
            } else if is_w(child, "i") {
                out.italic = Some(on_off(w_attr(child, "val")));
            } else if is_w(child, "sz") {
                out.font_size = w_attr(child, "val")
                    .and_then(units::parse_int)
                    .and_then(|hp| units::half_points_to_emu(hp).ok());
            } else if is_w(child, "color") {
                out.color = w_attr(child, "val").and_then(units::color_model);
            }
        }
        out
    }

    /// Names of the properties that this element sets directly.
    pub fn direct(&self) -> BTreeSet<String> {
        let mut names = BTreeSet::new();
        if self.font_family.is_some() {
            names.insert("font_family".to_string());
        }
        if self.font_size.is_some() {
            names.insert("font_size".to_string());
        }
        if self.color.is_some() {
            names.insert("color".to_string());
        }
        if self.bold.is_some() {
            names.insert("bold".to_string());
        }
        if self.italic.is_some() {
            names.insert("italic".to_string());
        }
        names
    }
}

/// Modelled paragraph properties parsed from `w:pPr`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ParaProps {
    pub align: Option<String>,
    pub space_before: Option<i64>,
    pub space_after: Option<i64>,
    /// 0 = single (240 auto); >0 = exact pitch in EMU; <0 = auto line of |value|/240 lines.
    pub line_spacing: Option<i64>,
    pub indent: Option<i64>,
    /// `w:pageBreakBefore`: `Some(true)` when on, `Some(false)` when set off (`w:val="0"`), `None` when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page_break_before: Option<bool>,
}

fn align_model(val: &str) -> Option<&'static str> {
    Some(match val {
        "left" | "start" => "left",
        "center" => "center",
        "right" | "end" => "right",
        "both" | "distribute" | "justify" => "justify",
        _ => return None,
    })
}

fn align_file(model: &str) -> &'static str {
    match model {
        "center" => "center",
        "right" => "right",
        "justify" => "both",
        _ => "left",
    }
}

impl ParaProps {
    pub fn parse(ppr: Option<Node<'_, '_>>) -> Self {
        let mut out = Self::default();
        let Some(ppr) = ppr else { return out };
        for child in tree::elements(ppr) {
            if is_w(child, "jc") {
                out.align = w_attr(child, "val").and_then(align_model).map(str::to_string);
            } else if is_w(child, "spacing") {
                out.space_before = w_attr(child, "before")
                    .and_then(units::parse_int)
                    .and_then(|t| units::twips_to_emu(t).ok());
                out.space_after = w_attr(child, "after")
                    .and_then(units::parse_int)
                    .and_then(|t| units::twips_to_emu(t).ok());
                let line = w_attr(child, "line").and_then(units::parse_int);
                out.line_spacing = match (line, w_attr(child, "lineRule")) {
                    (Some(l), Some("exact")) | (Some(l), Some("atLeast")) => units::twips_to_emu(l).ok(),
                    (Some(240), _) => Some(0),
                    (Some(l), _) => Some(-l),
                    (None, _) => None,
                };
            } else if is_w(child, "ind") {
                // Model indent is the first-line indent: positive for firstLine, negative for hanging.
                let first = w_attr(child, "firstLine").and_then(units::parse_int);
                let hanging = w_attr(child, "hanging").and_then(units::parse_int);
                out.indent = match (first, hanging) {
                    (Some(f), _) => units::twips_to_emu(f).ok(),
                    (None, Some(h)) => units::twips_to_emu(-h).ok(),
                    (None, None) => None,
                };
            } else if is_w(child, "pageBreakBefore") {
                out.page_break_before = Some(on_off(w_attr(child, "val")));
            }
        }
        out
    }

    pub fn direct(&self) -> BTreeSet<String> {
        let mut names = BTreeSet::new();
        if self.align.is_some() {
            names.insert("align".to_string());
        }
        if self.space_before.is_some() {
            names.insert("space_before".to_string());
        }
        if self.space_after.is_some() {
            names.insert("space_after".to_string());
        }
        if self.line_spacing.is_some() {
            names.insert("line_spacing".to_string());
        }
        if self.indent.is_some() {
            names.insert("indent".to_string());
        }
        if self.page_break_before.is_some() {
            names.insert("page_break_before".to_string());
        }
        names
    }
}

/// Schema order of `w:rPr` children (ECMA-376 CT_RPr).
pub fn rpr_rank(key: &str) -> Option<u32> {
    const ORDER: &[&str] = &[
        "w:rStyle", "w:rFonts", "w:b", "w:bCs", "w:i", "w:iCs", "w:caps", "w:smallCaps", "w:strike", "w:dstrike",
        "w:outline", "w:shadow", "w:emboss", "w:imprint", "w:noProof", "w:snapToGrid", "w:vanish", "w:webHidden",
        "w:color", "w:spacing", "w:w", "w:kern", "w:position", "w:sz", "w:szCs", "w:highlight", "w:u", "w:effect",
        "w:bdr", "w:shd", "w:fitText", "w:vertAlign", "w:rtl", "w:cs", "w:em", "w:lang", "w:eastAsianLayout",
        "w:specVanish", "w:oMath",
    ];
    ORDER.iter().position(|k| *k == key).map(|p| p as u32 + 1)
}

/// Schema order of `w:pPr` children (ECMA-376 CT_PPr).
pub fn ppr_rank(key: &str) -> Option<u32> {
    const ORDER: &[&str] = &[
        "w:pStyle", "w:keepNext", "w:keepLines", "w:pageBreakBefore", "w:framePr", "w:widowControl", "w:numPr",
        "w:suppressLineNumbers", "w:pBdr", "w:shd", "w:tabs", "w:suppressAutoHyphens", "w:kinsoku", "w:wordWrap",
        "w:overflowPunct", "w:topLinePunct", "w:autoSpaceDE", "w:autoSpaceDN", "w:bidi", "w:adjustRightInd",
        "w:snapToGrid", "w:spacing", "w:ind", "w:contextualSpacing", "w:mirrorIndents", "w:suppressOverlap", "w:jc",
        "w:textDirection", "w:textAlignment", "w:textboxTightWrap", "w:outlineLvl", "w:divId", "w:cnfStyle",
        "w:rPr", "w:sectPr", "w:pPrChange",
    ];
    ORDER.iter().position(|k| *k == key).map(|p| p as u32 + 1)
}

/// Schema order of `w:sectPr` children (ECMA-376 CT_SectPr).
pub fn sect_rank(key: &str) -> Option<u32> {
    const ORDER: &[&str] = &[
        "w:headerReference", "w:footerReference", "w:footnotePr", "w:endnotePr", "w:type", "w:pgSz", "w:pgMar",
        "w:paperSrc", "w:pgBorders", "w:lnNumType", "w:pgNumType", "w:cols", "w:formProt", "w:vAlign",
        "w:noEndnote", "w:titlePg", "w:textDirection", "w:bidi", "w:rtlGutter", "w:docGrid", "w:printerSettings",
        "w:sectPrChange",
    ];
    ORDER.iter().position(|k| *k == key).map(|p| p as u32 + 1)
}

/// Schema order of `w:tcPr` children (ECMA-376 CT_TcPr).
pub fn tcpr_rank(key: &str) -> Option<u32> {
    const ORDER: &[&str] = &[
        "w:cnfStyle", "w:tcW", "w:gridSpan", "w:hMerge", "w:vMerge", "w:tcBorders", "w:shd", "w:noWrap", "w:tcMar",
        "w:textDirection", "w:tcFitText", "w:vAlign", "w:hideMark", "w:headers", "w:cellIns", "w:cellDel",
        "w:cellMerge", "w:tcPrChange",
    ];
    ORDER.iter().position(|k| *k == key).map(|p| p as u32 + 1)
}

/// Schema order of `w:tblPr` children (ECMA-376 CT_TblPr).
pub fn tblpr_rank(key: &str) -> Option<u32> {
    const ORDER: &[&str] = &[
        "w:tblStyle", "w:tblpPr", "w:tblOverlap", "w:bidiVisual", "w:tblStyleRowBandSize", "w:tblStyleColBandSize",
        "w:tblW", "w:jc", "w:tblCellSpacing", "w:tblInd", "w:tblBorders", "w:shd", "w:tblLayout", "w:tblCellMar",
        "w:tblLook", "w:tblCaption", "w:tblDescription", "w:tblPrChange",
    ];
    ORDER.iter().position(|k| *k == key).map(|p| p as u32 + 1)
}

/// Rewrites the modelled run properties of an `w:rPr` child list.
///
/// A property that the model marks direct is written from `style`. A property that is no longer
/// direct is removed only when the original element set it directly, so properties that the model
/// does not represent (for example a theme-only color) are left alone.
pub fn apply_run_props(
    parts: &mut Vec<Part>,
    direct: &BTreeSet<String>,
    style: &Style,
    original: &RunProps,
) -> DltResult<()> {
    if direct.contains("font_family") {
        let family = style.font_family.as_str();
        if tree::has_part(parts, "w:rFonts") {
            let mut part = parts.iter().find(|p| p.key == "w:rFonts").cloned().expect("present");
            tree::patch_part_attrs(
                &mut part,
                &[
                    ("w:ascii", Some(family.to_string())),
                    ("w:hAnsi", Some(family.to_string())),
                    ("w:eastAsia", Some(family.to_string())),
                    ("w:cs", Some(family.to_string())),
                ],
            )?;
            tree::set_part(parts, "w:rFonts", 2, part.bytes);
        } else {
            let markup = format!(
                "<w:rFonts w:ascii=\"{f}\" w:hAnsi=\"{f}\" w:eastAsia=\"{f}\" w:cs=\"{f}\"/>",
                f = designllm_package::xml::escape_attr(family)?
            );
            tree::set_part(parts, "w:rFonts", 2, markup.into_bytes());
        }
    } else if original.font_family.is_some() {
        remove_font_attrs(parts)?;
    }

    if direct.contains("font_size") {
        let (half_points, _) = units::to_half_points(style.font_size)?;
        let markup_sz = format!("<w:sz w:val=\"{half_points}\"/>");
        let markup_cs = format!("<w:szCs w:val=\"{half_points}\"/>");
        tree::set_part(parts, "w:sz", 24, markup_sz.into_bytes());
        tree::set_part(parts, "w:szCs", 25, markup_cs.into_bytes());
    } else if original.font_size.is_some() {
        tree::remove_part(parts, "w:sz");
        tree::remove_part(parts, "w:szCs");
    }

    if direct.contains("color") {
        let color = units::color_file(&style.color)?;
        tree::set_part(parts, "w:color", 19, format!("<w:color w:val=\"{color}\"/>").into_bytes());
    } else if original.color.is_some() {
        tree::remove_part(parts, "w:color");
    }

    if direct.contains("bold") {
        let markup = if style.bold { "<w:b/>" } else { "<w:b w:val=\"0\"/>" };
        tree::set_part(parts, "w:b", 3, markup.as_bytes().to_vec());
    } else if original.bold.is_some() {
        tree::remove_part(parts, "w:b");
    }

    if direct.contains("italic") {
        let markup = if style.italic { "<w:i/>" } else { "<w:i w:val=\"0\"/>" };
        tree::set_part(parts, "w:i", 5, markup.as_bytes().to_vec());
    } else if original.italic.is_some() {
        tree::remove_part(parts, "w:i");
    }
    Ok(())
}

/// Removes the four font-family attributes from `w:rFonts` and drops the element when nothing else is
/// left in it.
fn remove_font_attrs(parts: &mut Vec<Part>) -> DltResult<()> {
    let Some(index) = parts.iter().position(|p| p.key == "w:rFonts") else { return Ok(()) };
    let mut part = parts[index].clone();
    tree::patch_part_attrs(
        &mut part,
        &[("w:ascii", None), ("w:hAnsi", None), ("w:eastAsia", None), ("w:cs", None)],
    )?;
    let tag = tree::StartTag::parse(&part.bytes[..tree::start_tag_len(&part.bytes)?])?;
    if tag.attrs.is_empty() {
        parts.remove(index);
    } else {
        parts[index] = part;
    }
    Ok(())
}

/// Rewrites the modelled paragraph properties of a `w:pPr` child list.
///
/// `format` holds the effective values (direct or inherited). A property is written only when the model
/// marks it direct; a property that the original element set directly and that is no longer direct is
/// removed.
pub fn apply_para_props(
    parts: &mut Vec<Part>,
    direct: &BTreeSet<String>,
    format: &ParagraphFormat,
    original: &ParaProps,
    rounding: &mut Vec<(String, i64, i64, &'static str)>,
) -> DltResult<()> {
    if direct.contains("align") {
        let markup = format!("<w:jc w:val=\"{}\"/>", align_file(&format.align));
        tree::set_part(parts, "w:jc", 27, markup.into_bytes());
    } else if original.align.is_some() {
        tree::remove_part(parts, "w:jc");
    }

    // An explicit `w:val="0"` is written for a false value, so that a page break inherited from the paragraph style is
    // switched off as well.
    if direct.contains("page_break_before") {
        let markup = if format.page_break_before { "<w:pageBreakBefore/>" } else { "<w:pageBreakBefore w:val=\"0\"/>" };
        tree::set_part(parts, "w:pageBreakBefore", 4, markup.as_bytes().to_vec());
    } else if original.page_break_before.is_some() {
        tree::remove_part(parts, "w:pageBreakBefore");
    }

    let spacing_directed = direct.contains("space_before") || direct.contains("space_after") || direct.contains("line_spacing");
    let spacing_original = original.space_before.is_some() || original.space_after.is_some() || original.line_spacing.is_some();
    if spacing_directed || spacing_original {
        let mut attrs: Vec<(&str, Option<String>)> = Vec::new();
        if direct.contains("space_before") {
            let (twips, applied) = units::to_twips(format.space_before)?;
            if applied != format.space_before {
                rounding.push(("space_before".into(), format.space_before, applied, "twip"));
            }
            attrs.push(("w:before", Some(twips.to_string())));
        } else if original.space_before.is_some() {
            attrs.push(("w:before", None));
        }
        if direct.contains("space_after") {
            let (twips, applied) = units::to_twips(format.space_after)?;
            if applied != format.space_after {
                rounding.push(("space_after".into(), format.space_after, applied, "twip"));
            }
            attrs.push(("w:after", Some(twips.to_string())));
        } else if original.space_after.is_some() {
            attrs.push(("w:after", None));
        }
        if direct.contains("line_spacing") {
            let line = format.line_spacing;
            if line == 0 {
                attrs.push(("w:line", Some("240".into())));
                attrs.push(("w:lineRule", Some("auto".into())));
            } else if line < 0 {
                attrs.push(("w:line", Some((-line).to_string())));
                attrs.push(("w:lineRule", Some("auto".into())));
            } else {
                let (twips, applied) = units::to_twips(line)?;
                if applied != line {
                    rounding.push(("line_spacing".into(), line, applied, "twip"));
                }
                attrs.push(("w:line", Some(twips.to_string())));
                attrs.push(("w:lineRule", Some("exact".into())));
            }
        } else if original.line_spacing.is_some() {
            attrs.push(("w:line", None));
            attrs.push(("w:lineRule", None));
        }
        let existing = parts.iter().position(|p| p.key == "w:spacing");
        let mut part = match existing {
            Some(i) => parts[i].clone(),
            None => Part { key: "w:spacing".into(), rank: 22, bytes: b"<w:spacing/>".to_vec() },
        };
        let edits: Vec<(&str, Option<String>)> = attrs.iter().map(|(n, v)| (*n, v.clone())).collect();
        tree::patch_part_attrs(&mut part, &edits)?;
        let tag = tree::StartTag::parse(&part.bytes[..tree::start_tag_len(&part.bytes)?])?;
        match (tag.attrs.is_empty(), existing) {
            (true, Some(i)) => {
                parts.remove(i);
            }
            (true, None) => {}
            (false, Some(i)) => parts[i] = part,
            (false, None) => tree::set_part(parts, "w:spacing", 22, part.bytes),
        }
    }

    // First-line indent: positive -> w:firstLine, negative -> w:hanging. The block (left) indent is not
    // modelled and is left as it is.
    if direct.contains("indent") || original.indent.is_some() {
        let (twips, applied) = if direct.contains("indent") {
            units::to_twips(format.indent)?
        } else {
            (0, 0)
        };
        if direct.contains("indent") && applied != format.indent {
            rounding.push(("indent".into(), format.indent, applied, "twip"));
        }
        let (first, hanging) = if !direct.contains("indent") {
            (None, None)
        } else if twips > 0 {
            (Some(twips.to_string()), None)
        } else if twips < 0 {
            (None, Some((-twips).to_string()))
        } else {
            (None, None)
        };
        let edits: Vec<(&str, Option<String>)> = vec![("w:firstLine", first), ("w:hanging", hanging)];
        let existing = parts.iter().position(|p| p.key == "w:ind");
        match existing {
            Some(i) => {
                let mut part = parts[i].clone();
                tree::patch_part_attrs(&mut part, &edits)?;
                let tag = tree::StartTag::parse(&part.bytes[..tree::start_tag_len(&part.bytes)?])?;
                if tag.attrs.is_empty() {
                    parts.remove(i);
                } else {
                    parts[i] = part;
                }
            }
            None => {
                let mut part = Part { key: "w:ind".into(), rank: 23, bytes: b"<w:ind/>".to_vec() };
                tree::patch_part_attrs(&mut part, &edits)?;
                let tag = tree::StartTag::parse(&part.bytes[..tree::start_tag_len(&part.bytes)?])?;
                if !tag.attrs.is_empty() {
                    tree::set_part(parts, "w:ind", 23, part.bytes);
                }
            }
        }
    }
    Ok(())
}

/// `w:jc` value for an alignment model value.
pub fn align_value(model: &str) -> &'static str {
    align_file(model)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_props_parse_modelled_children_and_ignore_the_rest() {
        let xml = "<w:document xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\"><w:rPr><w:rFonts w:ascii=\"Arial\" w:eastAsia=\"Malgun Gothic\"/><w:b/><w:i w:val=\"0\"/><w:sz w:val=\"32\"/><w:color w:val=\"17365D\"/><w:u w:val=\"single\"/></w:rPr></w:document>";
        let doc = roxmltree::Document::parse(xml).unwrap();
        let rpr = doc.root_element().first_element_child();
        let props = RunProps::parse(rpr);
        assert_eq!(props.font_family.as_deref(), Some("Arial"));
        assert_eq!(props.bold, Some(true));
        assert_eq!(props.italic, Some(false));
        assert_eq!(props.font_size, Some(16 * 12_700));
        assert_eq!(props.color.as_deref(), Some("#17365D"));
        assert_eq!(props.direct().len(), 5);
    }

    #[test]
    fn para_props_map_spacing_lines_and_alignment() {
        let xml = "<w:document xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\"><w:pPr><w:jc w:val=\"both\"/><w:spacing w:before=\"240\" w:after=\"120\" w:line=\"360\" w:lineRule=\"auto\"/><w:ind w:left=\"720\"/></w:pPr></w:document>";
        let doc = roxmltree::Document::parse(xml).unwrap();
        let props = ParaProps::parse(doc.root_element().first_element_child());
        assert_eq!(props.align.as_deref(), Some("justify"));
        assert_eq!(props.space_before, Some(240 * 635));
        assert_eq!(props.space_after, Some(120 * 635));
        assert_eq!(props.line_spacing, Some(-360));
        // w:left is a block indent, not the first-line indent the model stores; it is left in the XML.
        assert_eq!(props.indent, None);
        let hanging = "<w:document xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\"><w:pPr><w:ind w:firstLine=\"720\"/></w:pPr></w:document>";
        let doc = roxmltree::Document::parse(hanging).unwrap();
        assert_eq!(ParaProps::parse(doc.root_element().first_element_child()).indent, Some(720 * 635));
    }

    #[test]
    fn page_break_before_reads_on_off_values_and_is_direct_when_present() {
        let ns = crate::tree::NS_W;
        let parse = |ppr: &str| {
            let xml = format!("<w:document xmlns:w=\"{ns}\"><w:pPr>{ppr}</w:pPr></w:document>");
            let doc = roxmltree::Document::parse(&xml).unwrap();
            ParaProps::parse(doc.root_element().first_element_child())
        };
        assert_eq!(parse("<w:pageBreakBefore/>").page_break_before, Some(true));
        assert_eq!(parse("<w:pageBreakBefore w:val=\"1\"/>").page_break_before, Some(true));
        assert_eq!(parse("<w:pageBreakBefore w:val=\"0\"/>").page_break_before, Some(false));
        assert_eq!(parse("<w:pageBreakBefore w:val=\"false\"/>").page_break_before, Some(false));
        assert_eq!(parse("<w:pageBreakBefore w:val=\"off\"/>").page_break_before, Some(false));
        assert_eq!(parse("<w:jc w:val=\"left\"/>").page_break_before, None);
        let on = parse("<w:pageBreakBefore/>");
        assert!(on.direct().contains("page_break_before"));
        assert!(!parse("<w:jc w:val=\"left\"/>").direct().contains("page_break_before"));
    }

    #[test]
    fn page_break_before_off_is_written_explicitly_and_on_is_written_in_schema_order() {
        let mut parts = vec![
            Part { key: "w:keepNext".into(), rank: 2, bytes: b"<w:keepNext/>".to_vec() },
            Part { key: "w:jc".into(), rank: 27, bytes: b"<w:jc w:val=\"left\"/>".to_vec() },
        ];
        let original = ParaProps::default();
        let mut format = ParagraphFormat { page_break_before: true, ..ParagraphFormat::default() };
        let direct: BTreeSet<String> = ["page_break_before".to_string()].into_iter().collect();
        let mut rounding = Vec::new();
        apply_para_props(&mut parts, &direct, &format, &original, &mut rounding).unwrap();
        let keys: Vec<&str> = parts.iter().map(|p| p.key.as_str()).collect();
        assert_eq!(keys, vec!["w:keepNext", "w:pageBreakBefore", "w:jc"]);
        assert_eq!(parts[1].bytes, b"<w:pageBreakBefore/>".to_vec());

        format.page_break_before = false;
        apply_para_props(&mut parts, &direct, &format, &original, &mut rounding).unwrap();
        assert_eq!(parts[1].bytes, b"<w:pageBreakBefore w:val=\"0\"/>".to_vec());

        // A direct element that the model no longer marks direct is removed.
        let none = BTreeSet::new();
        let original_on = ParaProps { page_break_before: Some(true), ..ParaProps::default() };
        apply_para_props(&mut parts, &none, &format, &original_on, &mut rounding).unwrap();
        assert!(!parts.iter().any(|p| p.key == "w:pageBreakBefore"));
    }

    #[test]
    fn align_file_values_round_trip_model_names() {
        assert_eq!(align_value("justify"), "both");
        assert_eq!(align_value("center"), "center");
        assert_eq!(align_value("unknown"), "left");
    }
}
