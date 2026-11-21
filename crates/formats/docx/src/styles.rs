//! Named styles from `word/styles.xml` and effective formatting.
//!
//! Effective run formatting is `direct > character style chain > paragraph style chain > defaults`
//! (ECMA-376 17.7.2). Named-style edits are stored as overrides on top of the original part, so the
//! original styles.xml stays the immutable source and the overrides are small JSON.

use std::collections::{BTreeMap, BTreeSet};

use designllm_contracts::{Diagnostic, DltResult, FlowStyleInfo, FlowStyleKind, ParagraphFormat, Style, EMU_PER_PT};
use designllm_package::xml as bx;
use serde::{Deserialize, Serialize};

use crate::diag;
use crate::props::{self, ParaProps, RunProps};
use crate::tree::{self, is_w, w_attr, StartTag, Tree};

/// Font used when no part of the style chain names one.
pub const FALLBACK_FONT: &str = "Pretendard";
/// Font size used when no part of the style chain sets one (10 pt, the OOXML default).
pub const FALLBACK_FONT_SIZE: i64 = 10 * EMU_PER_PT;
/// Maximum depth of a `basedOn` chain before the chain is treated as cyclic.
const MAX_CHAIN: usize = 32;

#[derive(Clone, Debug, Default)]
pub struct StyleDef {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub based_on: Option<String>,
    pub rpr: RunProps,
    pub ppr: ParaProps,
}

#[derive(Clone, Debug, Default)]
pub struct StyleTable {
    pub defaults_rpr: RunProps,
    pub defaults_ppr: ParaProps,
    pub styles: BTreeMap<String, StyleDef>,
    /// `styleId` of the default paragraph style (`w:default="1"`), if declared.
    pub default_paragraph: Option<String>,
}

/// Named-style edits recorded against the original styles.xml.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct StyleOverride {
    pub rpr: RunProps,
    pub ppr: ParaProps,
}

pub type Overrides = BTreeMap<String, StyleOverride>;

fn is_paragraph_style(kind: &str) -> bool {
    kind.is_empty() || kind == "paragraph"
}

impl StyleTable {
    /// Parses styles.xml. A missing part yields an empty table (all defaults).
    pub fn parse(bytes: Option<&[u8]>) -> DltResult<Self> {
        let Some(bytes) = bytes else { return Ok(Self::default()) };
        let tree = Tree::parse(bytes)?;
        let root = tree.root();
        let mut table = Self::default();
        for child in tree::elements(root) {
            if is_w(child, "docDefaults") {
                for part in tree::elements(child) {
                    if is_w(part, "rPrDefault") {
                        let rpr = tree::elements(part).into_iter().find(|n| is_w(*n, "rPr"));
                        table.defaults_rpr = RunProps::parse(rpr);
                    } else if is_w(part, "pPrDefault") {
                        let ppr = tree::elements(part).into_iter().find(|n| is_w(*n, "pPr"));
                        table.defaults_ppr = ParaProps::parse(ppr);
                    }
                }
            } else if is_w(child, "style") {
                let id = w_attr(child, "styleId").unwrap_or_default().to_string();
                if id.is_empty() {
                    continue;
                }
                let kind = w_attr(child, "type").unwrap_or_default().to_string();
                let is_default = w_attr(child, "default") == Some("1") || w_attr(child, "default") == Some("true");
                if is_paragraph_style(&kind) && is_default && table.default_paragraph.is_none() {
                    table.default_paragraph = Some(id.clone());
                }
                let name = tree::elements(child)
                    .into_iter()
                    .find(|n| is_w(*n, "name"))
                    .and_then(|n| w_attr(n, "val"))
                    .unwrap_or(&id)
                    .to_string();
                let based_on = tree::elements(child)
                    .into_iter()
                    .find(|n| is_w(*n, "basedOn"))
                    .and_then(|n| w_attr(n, "val"))
                    .map(str::to_string);
                let rpr = RunProps::parse(tree::elements(child).into_iter().find(|n| is_w(*n, "rPr")));
                let ppr = ParaProps::parse(tree::elements(child).into_iter().find(|n| is_w(*n, "pPr")));
                table.styles.insert(id.clone(), StyleDef { id, name, kind, based_on, rpr, ppr });
            }
        }
        Ok(table)
    }

    /// Applies recorded overrides. Only set fields are merged.
    pub fn apply_overrides(&mut self, overrides: &Overrides) {
        for (id, over) in overrides {
            if let Some(def) = self.styles.get_mut(id) {
                merge_run(&mut def.rpr, &over.rpr);
                merge_para(&mut def.ppr, &over.ppr);
            }
        }
    }

    /// Resolves a style id by `styleId` first, then by display name.
    pub fn resolve_name(&self, name: &str) -> Option<String> {
        if self.styles.contains_key(name) {
            return Some(name.to_string());
        }
        let matches: Vec<&StyleDef> = self.styles.values().filter(|d| d.name == name).collect();
        match matches.as_slice() {
            [one] => Some(one.id.clone()),
            _ => None,
        }
    }

    fn chain_run<T>(&self, start: Option<&str>, get: impl Fn(&RunProps) -> Option<T>) -> Option<T> {
        let mut id = start.map(str::to_string);
        for _ in 0..MAX_CHAIN {
            let current = id?;
            let def = self.styles.get(&current)?;
            if let Some(v) = get(&def.rpr) {
                return Some(v);
            }
            id = def.based_on.clone();
        }
        None
    }

    fn chain_para<T>(&self, start: Option<&str>, get: impl Fn(&ParaProps) -> Option<T>) -> Option<T> {
        let mut id = start.map(str::to_string);
        for _ in 0..MAX_CHAIN {
            let current = id?;
            let def = self.styles.get(&current)?;
            if let Some(v) = get(&def.ppr) {
                return Some(v);
            }
            id = def.based_on.clone();
        }
        None
    }

    /// A missing or non-paragraph pStyle takes the default paragraph style, as does a missing pStyle.
    fn paragraph_chain_start<'a>(&'a self, pstyle: Option<&'a str>) -> Option<&'a str> {
        pstyle.filter(|id| self.styles.get(*id).is_some_and(|style| is_paragraph_style(&style.kind)))
            .or(self.default_paragraph.as_deref())
    }

    /// Effective run style of a run with paragraph style `pstyle`, character style `rstyle` and direct props.
    pub fn run_style(&self, pstyle: Option<&str>, rstyle: Option<&str>, direct: &RunProps) -> Style {
        self.run_style_from_chain(self.paragraph_chain_start(pstyle), rstyle, direct)
    }

    fn run_style_from_chain(&self, para: Option<&str>, rstyle: Option<&str>, direct: &RunProps) -> Style {
        let font_family = direct
            .font_family
            .clone()
            .or_else(|| self.chain_run(rstyle, |r| r.font_family.clone()))
            .or_else(|| self.chain_run(para, |r| r.font_family.clone()))
            .or_else(|| self.defaults_rpr.font_family.clone())
            .unwrap_or_else(|| FALLBACK_FONT.to_string());
        let font_size = direct
            .font_size
            .or_else(|| self.chain_run(rstyle, |r| r.font_size))
            .or_else(|| self.chain_run(para, |r| r.font_size))
            .or(self.defaults_rpr.font_size)
            .unwrap_or(FALLBACK_FONT_SIZE);
        let color = direct
            .color
            .clone()
            .or_else(|| self.chain_run(rstyle, |r| r.color.clone()))
            .or_else(|| self.chain_run(para, |r| r.color.clone()))
            .or_else(|| self.defaults_rpr.color.clone())
            .unwrap_or_else(|| "#000000".to_string());
        let bold = direct
            .bold
            .or_else(|| self.chain_run(rstyle, |r| r.bold))
            .or_else(|| self.chain_run(para, |r| r.bold))
            .or(self.defaults_rpr.bold)
            .unwrap_or(false);
        let italic = direct
            .italic
            .or_else(|| self.chain_run(rstyle, |r| r.italic))
            .or_else(|| self.chain_run(para, |r| r.italic))
            .or(self.defaults_rpr.italic)
            .unwrap_or(false);
        Style { font_family, font_size, color, bold, italic, ..Style::default() }
    }

    /// Effective paragraph format of a paragraph with style `pstyle` and direct props.
    pub fn para_format(&self, pstyle: Option<&str>, direct: &ParaProps) -> ParagraphFormat {
        // The document defaults (pPrDefault) apply only below the chain that `paragraph_chain_start` names.
        let para = self.paragraph_chain_start(pstyle);
        let align = direct
            .align
            .clone()
            .or_else(|| self.chain_para(para, |p| p.align.clone()))
            .or_else(|| self.defaults_ppr.align.clone())
            .unwrap_or_else(|| "left".to_string());
        let space_before = direct
            .space_before
            .or_else(|| self.chain_para(para, |p| p.space_before))
            .or(self.defaults_ppr.space_before)
            .unwrap_or(0);
        let space_after = direct
            .space_after
            .or_else(|| self.chain_para(para, |p| p.space_after))
            .or(self.defaults_ppr.space_after)
            .unwrap_or(0);
        let line_spacing = direct
            .line_spacing
            .or_else(|| self.chain_para(para, |p| p.line_spacing))
            .or(self.defaults_ppr.line_spacing)
            .unwrap_or(0);
        let indent = direct
            .indent
            .or_else(|| self.chain_para(para, |p| p.indent))
            .or(self.defaults_ppr.indent)
            .unwrap_or(0);
        let page_break_before = direct
            .page_break_before
            .or_else(|| self.chain_para(para, |p| p.page_break_before))
            .or(self.defaults_ppr.page_break_before)
            .unwrap_or(false);
        ParagraphFormat { align, space_before, space_after, line_spacing, indent, page_break_before, ..ParagraphFormat::default() }
    }

    /// Model entry for a named style: the style's own chain with defaults applied.
    pub fn model_style(&self, id: &str) -> Style {
        let start = if self.styles.contains_key(id) { Some(id) } else { self.default_paragraph.as_deref() };
        let mut style = self.run_style_from_chain(start, None, &RunProps::default());
        if let Some(align) = self.chain_para(Some(id), |p| p.align.clone()) {
            style.align = align;
        }
        style
    }

    pub fn default_paragraph_style(&self) -> Option<&str> {
        self.default_paragraph.as_deref()
    }
}

fn merge_run(target: &mut RunProps, over: &RunProps) {
    if over.font_family.is_some() {
        target.font_family.clone_from(&over.font_family);
    }
    if over.font_size.is_some() {
        target.font_size = over.font_size;
    }
    if over.color.is_some() {
        target.color.clone_from(&over.color);
    }
    if over.bold.is_some() {
        target.bold = over.bold;
    }
    if over.italic.is_some() {
        target.italic = over.italic;
    }
}

fn merge_para(target: &mut ParaProps, over: &ParaProps) {
    if over.align.is_some() {
        target.align.clone_from(&over.align);
    }
}

/// Rewrites the `w:style` elements named in `overridden` so that their own `w:rPr` and `w:pPr`
/// match the merged style table. Every other byte of styles.xml is copied unchanged.
pub fn write_style_overrides(bytes: &[u8], table: &StyleTable, overridden: &BTreeSet<String>) -> DltResult<Vec<u8>> {
    let tree = Tree::parse(bytes)?;
    let mut splices = Vec::new();
    for style in tree::elements(tree.root()) {
        if !is_w(style, "style") {
            continue;
        }
        let Some(id) = w_attr(style, "styleId") else { continue };
        if !overridden.contains(id) {
            continue;
        }
        let Some(def) = table.styles.get(id) else { continue };
        let span = tree.span(style);
        let start_tag = tree.raw(&tree.start_tag(&span)?).to_vec();
        let end_name = StartTag::parse(&start_tag)?.name;
        let mut parts = tree::parts_of(&tree, style, style_rank);

        // Run props: own rPr children, rewritten from the merged values.
        let rpr_node = tree::elements(style).into_iter().find(|n| is_w(*n, "rPr"));
        let mut rpr_parts = match rpr_node {
            Some(node) => tree::parts_of(&tree, node, props::rpr_rank),
            None => Vec::new(),
        };
        let direct = def.rpr.direct();
        let values = Style {
            font_family: def.rpr.font_family.clone().unwrap_or_default(),
            font_size: def.rpr.font_size.unwrap_or(0),
            color: def.rpr.color.clone().unwrap_or_default(),
            bold: def.rpr.bold.unwrap_or(false),
            italic: def.rpr.italic.unwrap_or(false),
            ..Style::default()
        };
        props::apply_run_props(&mut rpr_parts, &direct, &values, &RunProps::default())?;
        if rpr_node.is_some() || !rpr_parts.is_empty() {
            let head = match rpr_node {
                Some(node) => tree.raw(&tree.start_tag(&tree.span(node))?).to_vec(),
                None => b"<w:rPr>".to_vec(),
            };
            tree::set_part(&mut parts, "w:rPr", 18, tree::assemble(&head, &rpr_parts, "w:rPr"));
        }

        // Paragraph alignment of a paragraph style.
        if let Some(align) = &def.ppr.align {
            let ppr_node = tree::elements(style).into_iter().find(|n| is_w(*n, "pPr"));
            let mut ppr_parts = match ppr_node {
                Some(node) => tree::parts_of(&tree, node, props::ppr_rank),
                None => Vec::new(),
            };
            let jc = format!("<w:jc w:val=\"{}\"/>", props::align_value(align));
            tree::set_part(&mut ppr_parts, "w:jc", 27, jc.into_bytes());
            let head = match ppr_node {
                Some(node) => tree.raw(&tree.start_tag(&tree.span(node))?).to_vec(),
                None => b"<w:pPr>".to_vec(),
            };
            tree::set_part(&mut parts, "w:pPr", 17, tree::assemble(&head, &ppr_parts, "w:pPr"));
        }
        let rebuilt = tree::assemble(&start_tag, &parts, &end_name);
        splices.push(bx::Splice { range: span, replacement: rebuilt });
    }
    if splices.is_empty() {
        return Ok(bytes.to_vec());
    }
    bx::apply_checked(bytes, splices)
}

/// Schema order of `w:style` children (ECMA-376 CT_Style).
pub fn style_rank(key: &str) -> Option<u32> {
    const ORDER: &[&str] = &[
        "w:name", "w:aliases", "w:basedOn", "w:next", "w:link", "w:autoRedefine", "w:hidden", "w:uiPriority",
        "w:semiHidden", "w:unhideWhenUsed", "w:qFormat", "w:locked", "w:personal", "w:personalCompose",
        "w:personalReply", "w:rsid", "w:pPr", "w:rPr", "w:tblPr", "w:trPr", "w:tcPr", "w:tblStylePr",
    ];
    ORDER.iter().position(|k| *k == key).map(|p| p as u32 + 1)
}

/// What `flow.styles` lists for one styles part: the styles in document order, and a diagnostic for each style that
/// was left out.
#[derive(Debug, Clone, Default)]
pub struct FlowStyleList {
    pub styles: Vec<FlowStyleInfo>,
    pub diagnostics: Vec<Diagnostic>,
}

/// The `flow.styles` list of a styles.xml part: every `w:style` element, in document order.
///
/// `id` is `w:styleId`. A style without one is skipped, because no paragraph or run can name it. A missing `w:type`
/// is a paragraph style (ECMA-376). A style whose `w:type` is not one of the four ST_StyleType values is skipped and
/// reported with a non-blocking diagnostic that names the id and the value. The flags are ST_OnOff values (see
/// `props::on_off`). `builtin` is the opposite of `w:customStyle`.
pub fn list_flow_styles(styles_xml: &[u8]) -> DltResult<FlowStyleList> {
    let tree = Tree::parse(styles_xml)?;
    let mut list = FlowStyleList::default();
    for style in tree::elements(tree.root()).into_iter().filter(|n| is_w(*n, "style")) {
        let Some(id) = w_attr(style, "styleId").filter(|id| !id.is_empty()) else { continue };
        let kind = match w_attr(style, "type") {
            None | Some("") | Some("paragraph") => FlowStyleKind::Paragraph,
            Some("character") => FlowStyleKind::Character,
            Some("table") => FlowStyleKind::Table,
            Some("numbering") => FlowStyleKind::Numbering,
            Some(other) => {
                list.diagnostics.push(diag::info(
                    diag::UNKNOWN_STYLE_TYPE,
                    format!("style {id} has an unknown w:type {other}, so it is not listed"),
                    None,
                ));
                continue;
            }
        };
        let children = tree::elements(style);
        let child = |local: &str| children.iter().copied().find(|n| is_w(*n, local));
        let on = |local: &str| child(local).is_some_and(|n| props::on_off(w_attr(n, "val")));
        list.styles.push(FlowStyleInfo {
            id: id.to_string(),
            kind,
            display_name: child("name").and_then(|n| w_attr(n, "val")).map(str::to_string),
            quick: on("qFormat"),
            hidden: on("semiHidden") || on("hidden"),
            builtin: !w_attr(style, "customStyle").is_some_and(|v| props::on_off(Some(v))),
        });
    }
    Ok(list)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::units;

    const STYLES: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
<w:styles xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:docDefaults><w:rPrDefault><w:rPr><w:rFonts w:ascii="Arial" w:eastAsia="Malgun Gothic"/><w:sz w:val="22"/></w:rPr></w:rPrDefault><w:pPrDefault><w:pPr><w:spacing w:after="160"/></w:pPr></w:pPrDefault></w:docDefaults><w:style w:type="paragraph" w:default="1" w:styleId="Normal"><w:name w:val="Normal"/></w:style><w:style w:type="paragraph" w:styleId="LunaTitle"><w:name w:val="Luna Fixture Title"/><w:basedOn w:val="Normal"/><w:pPr><w:spacing w:after="120"/></w:pPr><w:rPr><w:b/><w:color w:val="17365D"/><w:sz w:val="32"/></w:rPr></w:style><w:style w:type="character" w:styleId="LunaAccent"><w:name w:val="Luna Accent"/><w:rPr><w:color w:val="0066CC"/><w:i/></w:rPr></w:style></w:styles>"#;

    #[test]
    fn effective_run_style_follows_direct_then_character_then_paragraph_then_defaults() {
        let table = StyleTable::parse(Some(STYLES)).unwrap();
        // Title paragraph, plain run: bold + 16pt + 17365D from the paragraph style.
        let plain = table.run_style(Some("LunaTitle"), None, &RunProps::default());
        assert!(plain.bold);
        assert_eq!(plain.font_size, 16 * EMU_PER_PT);
        assert_eq!(plain.color, "#17365D");
        assert_eq!(plain.font_family, "Arial");
        // Character style overrides color and italic over the paragraph style, bold stays inherited.
        let accent = table.run_style(Some("LunaTitle"), Some("LunaAccent"), &RunProps::default());
        assert_eq!(accent.color, "#0066CC");
        assert!(accent.italic);
        assert!(accent.bold);
        // Direct props win over everything.
        let direct = RunProps { color: Some("#FF0000".into()), bold: Some(false), ..RunProps::default() };
        let styled = table.run_style(Some("LunaTitle"), Some("LunaAccent"), &direct);
        assert_eq!(styled.color, "#FF0000");
        assert!(!styled.bold);
    }

    #[test]
    fn paragraph_format_uses_style_chain_and_doc_defaults() {
        let table = StyleTable::parse(Some(STYLES)).unwrap();
        let title = table.para_format(Some("LunaTitle"), &ParaProps::default());
        assert_eq!(title.space_after, 120 * 635);
        let plain = table.para_format(None, &ParaProps::default());
        assert_eq!(plain.space_after, 160 * 635);
        assert_eq!(plain.align, "left");
        assert!(!plain.page_break_before);
    }

    #[test]
    fn page_break_before_follows_direct_then_style_chain_then_defaults() {
        let xml: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
<w:styles xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:docDefaults><w:pPrDefault><w:pPr><w:pageBreakBefore/></w:pPr></w:pPrDefault></w:docDefaults><w:style w:type="paragraph" w:default="1" w:styleId="Normal"><w:name w:val="Normal"/><w:pPr><w:pageBreakBefore w:val="0"/><w:spacing w:before="120" w:line="360" w:lineRule="auto"/><w:ind w:firstLine="200"/><w:jc w:val="center"/></w:pPr></w:style><w:style w:type="paragraph" w:styleId="Heading1"><w:name w:val="heading 1"/><w:basedOn w:val="Normal"/><w:pPr><w:pageBreakBefore/></w:pPr></w:style><w:style w:type="paragraph" w:styleId="Heading2"><w:name w:val="heading 2"/><w:basedOn w:val="Heading1"/></w:style></w:styles>"#;
        let table = StyleTable::parse(Some(xml)).unwrap();
        let on = ParaProps { page_break_before: Some(true), ..ParaProps::default() };
        let off = ParaProps { page_break_before: Some(false), ..ParaProps::default() };
        // Style chain: Heading2 -> Heading1 (on, inherited through basedOn).
        assert!(table.para_format(Some("Heading2"), &ParaProps::default()).page_break_before);
        // Direct value wins over the style: a direct off overrides the style's on, and the reverse.
        assert!(!table.para_format(Some("Heading1"), &off).page_break_before);
        assert!(table.para_format(Some("Normal"), &on).page_break_before);
        // Normal sets off, so a paragraph in it without a direct value is off, not the document default on.
        assert!(!table.para_format(Some("Normal"), &ParaProps::default()).page_break_before);
        // No paragraph style: the default paragraph style (Normal) applies.
        assert!(!table.para_format(None, &ParaProps::default()).page_break_before);
        // A style missing from the table takes the default paragraph style (Normal, off), not the document default (pPrDefault, on).
        assert!(!table.para_format(Some("Missing"), &ParaProps::default()).page_break_before);
    }

    #[test]
    fn a_missing_style_takes_the_default_paragraph_chain_for_every_paragraph_field() {
        let xml: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
<w:styles xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:docDefaults><w:pPrDefault><w:pPr><w:spacing w:after="160"/></w:pPr></w:pPrDefault></w:docDefaults><w:style w:type="paragraph" w:default="1" w:styleId="Normal"><w:name w:val="Normal"/><w:pPr><w:pageBreakBefore/><w:spacing w:before="120" w:line="360" w:lineRule="auto"/><w:ind w:firstLine="200"/><w:jc w:val="center"/></w:pPr></w:style></w:styles>"#;
        let table = StyleTable::parse(Some(xml)).unwrap();
        let normal = table.para_format(Some("Normal"), &ParaProps::default());
        let missing = table.para_format(Some("Missing"), &ParaProps::default());
        // Sanity: Normal sets each field, so a missing style that falls back to pPrDefault would differ.
        assert_eq!(normal.align, "center");
        assert_eq!(normal.space_before, 120 * 635);
        assert_ne!(normal.line_spacing, 0);
        assert_ne!(normal.indent, 0);
        assert!(normal.page_break_before);
        assert_eq!(normal.space_after, 160 * 635);
        assert_eq!(missing.align, normal.align);
        assert_eq!(missing.space_before, normal.space_before);
        assert_eq!(missing.line_spacing, normal.line_spacing);
        assert_eq!(missing.indent, normal.indent);
        assert_eq!(missing.page_break_before, normal.page_break_before);
        // space_after is not set by Normal, so both fall through to pPrDefault.
        assert_eq!(missing.space_after, normal.space_after);
    }

    #[test]
    fn a_missing_style_takes_the_default_paragraph_chain_for_run_format() {
        let xml: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
<w:styles xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:docDefaults><w:rPrDefault><w:rPr><w:sz w:val="22"/></w:rPr></w:rPrDefault></w:docDefaults><w:style w:type="paragraph" w:default="1" w:styleId="Normal"><w:name w:val="Normal"/><w:rPr><w:rFonts w:ascii="Batang"/><w:b/><w:i/><w:color w:val="112233"/><w:sz w:val="28"/></w:rPr></w:style></w:styles>"#;
        let table = StyleTable::parse(Some(xml)).unwrap();
        let normal = table.run_style(Some("Normal"), None, &RunProps::default());
        let missing = table.run_style(Some("NoSuchStyle"), None, &RunProps::default());
        // Sanity: Normal sets every run field, so a fallback to the document defaults (10 pt, Pretendard) would differ.
        assert_eq!(normal.font_family, "Batang");
        assert_eq!(normal.font_size, 14 * EMU_PER_PT);
        assert_eq!(normal.color, "#112233");
        assert!(normal.bold && normal.italic);
        assert_eq!(missing.font_family, normal.font_family);
        assert_eq!(missing.font_size, normal.font_size);
        assert_eq!(missing.color, normal.color);
        assert_eq!(missing.bold, normal.bold);
        assert_eq!(missing.italic, normal.italic);
        // No pStyle reads the same default paragraph chain.
        let none = table.run_style(None, None, &RunProps::default());
        assert_eq!(none.font_family, normal.font_family);
        assert_eq!(none.font_size, normal.font_size);
    }

    #[test]
    fn paragraph_style_type_controls_the_chain_and_default() {
        let xml = br#"<w:styles xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:style w:default="1" w:styleId="Normal"><w:pPr><w:jc w:val="center"/></w:pPr><w:rPr><w:sz w:val="28"/></w:rPr></w:style><w:style w:styleId="Implicit"><w:pPr><w:jc w:val="right"/></w:pPr><w:rPr><w:sz w:val="32"/></w:rPr></w:style><w:style w:type="character" w:styleId="Char"><w:pPr><w:jc w:val="left"/></w:pPr><w:rPr><w:sz w:val="40"/></w:rPr></w:style><w:style w:type="table" w:styleId="Table"><w:pPr><w:jc w:val="left"/></w:pPr><w:rPr><w:sz w:val="42"/></w:rPr></w:style><w:style w:type="numbering" w:styleId="Number"><w:rPr><w:sz w:val="44"/></w:rPr></w:style></w:styles>"#;
        let table = StyleTable::parse(Some(xml)).unwrap();
        assert_eq!(table.default_paragraph_style(), Some("Normal"));
        for id in ["Char", "Table", "Number", "Missing"] {
            assert_eq!(table.para_format(Some(id), &ParaProps::default()).align, "center");
            assert_eq!(table.run_style(Some(id), None, &RunProps::default()).font_size, 14 * EMU_PER_PT);
        }
        assert_eq!(table.para_format(Some("Implicit"), &ParaProps::default()).align, "right");
        assert_eq!(table.run_style(Some("Implicit"), None, &RunProps::default()).font_size, 16 * EMU_PER_PT);
        assert_eq!(table.model_style("Char").font_size, 20 * EMU_PER_PT);
        let listed = list_flow_styles(xml).unwrap();
        assert!(listed.styles.iter().any(|s| s.id == "Implicit" && s.kind == FlowStyleKind::Paragraph));
    }

    #[test]
    fn overrides_change_the_style_and_the_rewritten_styles_xml_is_consistent() {
        let mut table = StyleTable::parse(Some(STYLES)).unwrap();
        let mut overrides = Overrides::new();
        overrides.insert("LunaTitle".into(), StyleOverride { rpr: RunProps { font_size: Some(20 * EMU_PER_PT), ..RunProps::default() }, ppr: ParaProps::default() });
        table.apply_overrides(&overrides);
        assert_eq!(table.run_style(Some("LunaTitle"), None, &RunProps::default()).font_size, 20 * EMU_PER_PT);

        let set: BTreeSet<String> = ["LunaTitle".to_string()].into_iter().collect();
        let rewritten = write_style_overrides(STYLES, &table, &set).unwrap();
        let reparsed = StyleTable::parse(Some(&rewritten)).unwrap();
        assert_eq!(reparsed.run_style(Some("LunaTitle"), None, &RunProps::default()).font_size, 20 * EMU_PER_PT);
        // Unrelated styles are byte-identical.
        let accent_before = &STYLES[find(STYLES, b"<w:style w:type=\"character\"")..];
        assert!(rewritten.windows(accent_before.len()).any(|w| w == accent_before));
    }

    fn find(hay: &[u8], needle: &[u8]) -> usize {
        hay.windows(needle.len()).position(|w| w == needle).expect("needle present")
    }

    #[test]
    fn units_for_font_size_match_half_points() {
        let (hp, applied) = units::to_half_points(16 * EMU_PER_PT).unwrap();
        assert_eq!((hp, applied), (32, 32 * 6_350));
    }
}
