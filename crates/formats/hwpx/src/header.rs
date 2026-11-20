//! Header model (`Contents/header.xml`): fonts, character and paragraph properties, named styles.
//!
//! Existing `hh:charPr` and `hh:paraPr` elements are never edited. A changed format is written as a
//! clone with a new id, and identical clones are reused. `hh:style` entries may be repointed to a
//! clone. Derived model values use the native grid: 1 HWPUNIT = 127 EMU (1/100 pt for font height).
use std::collections::BTreeMap;

use designllm_contracts::{ParagraphFormat, Style, TextRun, EMU_PER_HWPUNIT, EMU_PER_PT};
pub use designllm_flow::line_spacing::hu_to_emu;
use designllm_flow::line_spacing::{model_percent, percent_line_pitch};
use roxmltree::Node;

use crate::xmlx::{render_key, Overrides, Xml, NS_HC, NS_HH};

/// Language slots of the font table, in the order `hh:charPr/hh:fontRef` attributes use.
pub const LANGS: [&str; 7] = ["HANGUL", "LATIN", "HANJA", "JAPANESE", "OTHER", "SYMBOL", "USER"];
/// `hh:fontRef` attribute names, parallel to [`LANGS`].
pub const FONT_REF_ATTRS: [&str; 7] = ["hangul", "latin", "hanja", "japanese", "other", "symbol", "user"];
/// Type information written on newly added `<hh:font>` entries.
pub(crate) const DEFAULT_TYPE_INFO: &str = r##"<hh:typeInfo familyType="FCAT_GOTHIC" weight="6" proportion="4" contrast="0" strokeVariation="1" armStyle="1" letterform="1" midline="1" xHeight="1"/>"##;

/// Converts EMU to HWPUNIT, rounding to nearest with ties away from zero.
pub fn emu_to_hu(emu: i64) -> i64 {
    let half = EMU_PER_HWPUNIT / 2;
    if emu >= 0 {
        (emu + half) / EMU_PER_HWPUNIT
    } else {
        -((-emu + half) / EMU_PER_HWPUNIT)
    }
}

pub fn is_el(node: Node<'_, '_>, ns: &str, local: &str) -> bool {
    node.is_element() && node.tag_name().name() == local && node.tag_name().namespace() == Some(ns)
}

pub fn child_el<'a, 'i>(node: Node<'a, 'i>, ns: &str, local: &str) -> Option<Node<'a, 'i>> {
    node.children().find(|c| is_el(*c, ns, local))
}

pub fn child_els<'a, 'i>(node: Node<'a, 'i>, ns: &str, local: &str) -> Vec<Node<'a, 'i>> {
    node.children().filter(|c| is_el(*c, ns, local)).collect()
}

pub fn descendant_els<'a, 'i>(node: Node<'a, 'i>, ns: &str, local: &str) -> Vec<Node<'a, 'i>> {
    node.descendants().filter(|c| is_el(*c, ns, local)).collect()
}

#[derive(Debug, Clone)]
pub struct FontList {
    pub lang: String,
    pub key: String,
    /// (id, face name) in document order.
    pub fonts: Vec<(u32, String)>,
}

#[derive(Debug, Clone)]
pub struct PCharPr {
    pub id: u32,
    pub key: String,
    pub bold_key: Option<String>,
    pub italic_key: Option<String>,
    pub underline_key: Option<String>,
    pub font_ref_key: Option<String>,
    /// Derived run style. Fields HWPX cannot store keep the model defaults.
    pub style: Style,
}

#[derive(Debug, Clone)]
pub struct PParaPr {
    pub id: u32,
    pub key: String,
    pub align_key: Option<String>,
    pub prev_keys: Vec<String>,
    pub next_keys: Vec<String>,
    pub intent_keys: Vec<String>,
    pub line_keys: Vec<String>,
    pub line: NativeLine,
    /// The compatibility branches agree on the native type and value.
    pub uniform_line: bool,
    /// Anchor of `hh:breakSetting`, which holds `pageBreakBefore` (None when the shape has no such element).
    pub break_key: Option<String>,
    /// Everything but the line pitch. `line_spacing` is 0 here: the pitch depends on the font size of
    /// each paragraph and is resolved by [`NativeLine::pitch`] when the paragraph is read.
    /// `line_spacing_percent` is the PERCENT value, except 100 (single spacing).
    /// `page_break_before` is the shape's own break (`hh:breakSetting/@pageBreakBefore`).
    pub format: ParagraphFormat,
}

/// Font height used when a paragraph has no run to take a size from: 10 pt.
pub(crate) const DEFAULT_FONT_SIZE: i64 = 10 * EMU_PER_PT;

/// Empty text runs produce no glyphs on reopen. With no glyphs the paragraph uses its base character shape.
pub(crate) fn paragraph_font_size(runs: &[TextRun], base: Option<i64>) -> i64 {
    runs.iter().filter(|r| !r.text.is_empty()).map(|r| r.style.font_size).max().or(base).unwrap_or(DEFAULT_FONT_SIZE)
}

/// The native kind and unscaled value. Different kinds can give exactly the same model pitch.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct NativeLine {
    pub kind: String,
    pub value: i64,
}

impl NativeLine {
    pub fn from_format(format: &ParagraphFormat) -> Result<Self, String> {
        if let Some(percent) = format.line_spacing_percent {
            return Ok(Self { kind: "PERCENT".into(), value: i64::from(percent) });
        }
        let emu = format.line_spacing;
        if emu == 0 {
            return Ok(Self { kind: "PERCENT".into(), value: 100 });
        }
        if emu < 0 || emu % EMU_PER_HWPUNIT != 0 {
            return Err(format!("line spacing {emu} must be nonnegative and on the native grid"));
        }
        Ok(Self { kind: "FIXED".into(), value: emu / EMU_PER_HWPUNIT })
    }

    /// A new paragraph inherits the native rule. A zero fixed pitch projects to single spacing.
    pub fn inherited(&self) -> Self {
        if self.approximated() || (self.kind == "FIXED" && self.value > 0) {
            return self.clone();
        }
        Self { kind: "PERCENT".into(), value: self.percent().map(i64::from).unwrap_or(100) }
    }

    pub fn approximated(&self) -> bool {
        matches!(self.kind.as_str(), "AT_LEAST" | "BETWEEN_LINES")
    }

    pub fn percent(&self) -> Option<u32> {
        if self.kind == "PERCENT" { model_percent(self.value) } else { None }
    }

    /// Line pitch in EMU for a paragraph whose largest font is `max_size` EMU, in the flow model's
    /// convention (0 = single, more than 0 = exact pitch). PERCENT scales the font size with the rule
    /// the HWP adapter shares (`designllm_flow::line_spacing`): 100 percent is single spacing, the
    /// model's 0, so that a single paragraph written as 0 reads back as 0. FIXED is exact.
    /// BETWEEN_LINES (font height plus a gap) and AT_LEAST (a minimum over the font height) need the
    /// font's natural line height, which the model does not carry, so the font size stands in for it.
    /// The second element is true for those approximated types.
    pub fn pitch(&self, max_size: i64) -> (i64, bool) {
        let value = self.value.max(0);
        match self.kind.as_str() {
            "PERCENT" => (percent_line_pitch(max_size, value), false),
            "FIXED" => (value.saturating_mul(EMU_PER_HWPUNIT), false),
            "BETWEEN_LINES" => (max_size.saturating_add(value.saturating_mul(EMU_PER_HWPUNIT)), true),
            "AT_LEAST" => (value.saturating_mul(EMU_PER_HWPUNIT).max(max_size), true),
            _ => (0, true),
        }
    }
}

#[derive(Debug, Clone)]
pub struct PStyle {
    pub id: u32,
    pub key: String,
    pub name: String,
    pub para_pr: u32,
    pub char_pr: u32,
}

#[derive(Debug, Clone, Default)]
pub struct PHeader {
    pub fonts: Vec<FontList>,
    pub char_list_key: Option<String>,
    pub para_list_key: Option<String>,
    pub style_list_key: Option<String>,
    pub border_fill_ids: Vec<u32>,
    pub char_prs: Vec<PCharPr>,
    pub para_prs: Vec<PParaPr>,
    pub styles: Vec<PStyle>,
    /// (list key, declared itemCnt, actual children) for every counted list.
    pub item_counts: Vec<(String, Option<usize>, usize)>,
}

impl PHeader {
    pub fn char_pr(&self, id: u32) -> Option<&PCharPr> {
        self.char_prs.iter().find(|c| c.id == id)
    }

    pub fn para_pr(&self, id: u32) -> Option<&PParaPr> {
        self.para_prs.iter().find(|p| p.id == id)
    }

    pub fn default_para(&self) -> Option<&PParaPr> {
        self.style_by_id(0).or(self.styles.first()).and_then(|s| self.para_pr(s.para_pr)).or(self.para_prs.first())
    }

    pub fn style_by_id(&self, id: u32) -> Option<&PStyle> {
        self.styles.iter().find(|s| s.id == id)
    }

    pub fn style_by_name(&self, name: &str) -> Option<&PStyle> {
        self.styles.iter().find(|s| s.name == name)
    }

    pub fn font_list(&self, lang: &str) -> Option<&FontList> {
        self.fonts.iter().find(|f| f.lang == lang)
    }

    /// Named style map for the flow model: style name -> style derived from its character shape.
    pub fn named_styles(&self) -> BTreeMap<String, Style> {
        let mut map = BTreeMap::new();
        for s in &self.styles {
            if let Some(cp) = self.char_pr(s.char_pr) {
                map.insert(s.name.clone(), cp.style.clone());
            }
        }
        map
    }

    pub fn validate_refs(&self) -> Result<(), String> {
        for s in &self.styles {
            if self.para_pr(s.para_pr).is_none() {
                return Err(format!("style {} refers to missing paraPr {}", s.name, s.para_pr));
            }
            if self.char_pr(s.char_pr).is_none() {
                return Err(format!("style {} refers to missing charPr {}", s.name, s.char_pr));
            }
        }
        Ok(())
    }
}

fn font_face_for(fonts: &[FontList], lang_index: usize, id: u32) -> String {
    fonts
        .iter()
        .find(|f| f.lang == LANGS[lang_index])
        .and_then(|f| f.fonts.iter().find(|(i, _)| *i == id))
        .map(|(_, face)| face.clone())
        .unwrap_or_default()
}

/// Parses the header model. Reference checks happen in [`PHeader::validate_refs`].
pub fn parse(xml: &Xml<'_>) -> Result<PHeader, String> {
    let root = xml.doc.root_element();
    if !is_el(root, NS_HH, "head") {
        return Err("header root is not hh:head".into());
    }
    let ref_list = child_el(root, NS_HH, "refList").ok_or("header has no hh:refList")?;
    let mut header = PHeader::default();

    if let Some(faces) = child_el(ref_list, NS_HH, "fontfaces") {
        header.item_counts.push((xml.key(faces).unwrap_or_default().to_string(), attr_count(faces, "itemCnt"), child_els(faces, NS_HH, "fontface").len()));
        for face in child_els(faces, NS_HH, "fontface") {
            let lang = face.attribute("lang").unwrap_or("").to_string();
            let mut fonts = Vec::new();
            for font in child_els(face, NS_HH, "font") {
                let id = xml.attr_u32(font, "id").ok_or("hh:font without id")?;
                fonts.push((id, font.attribute("face").unwrap_or("").to_string()));
            }
            header.fonts.push(FontList { lang, key: xml.key(face).unwrap_or_default().to_string(), fonts });
        }
    }
    let fonts_snapshot = header.fonts.clone();

    if let Some(list) = child_el(ref_list, NS_HH, "borderFills") {
        header.item_counts.push((xml.key(list).unwrap_or_default().to_string(), attr_count(list, "itemCnt"), child_els(list, NS_HH, "borderFill").len()));
        header.border_fill_ids = child_els(list, NS_HH, "borderFill").iter().filter_map(|n| xml.attr_u32(*n, "id")).collect();
    }

    if let Some(list) = child_el(ref_list, NS_HH, "charProperties") {
        let key = xml.key(list).unwrap_or_default().to_string();
        let items = child_els(list, NS_HH, "charPr");
        header.item_counts.push((key.clone(), attr_count(list, "itemCnt"), items.len()));
        header.char_list_key = Some(key);
        for node in items {
            header.char_prs.push(parse_char_pr(xml, node, &fonts_snapshot)?);
        }
    }

    if let Some(list) = child_el(ref_list, NS_HH, "paraProperties") {
        let key = xml.key(list).unwrap_or_default().to_string();
        let items = child_els(list, NS_HH, "paraPr");
        header.item_counts.push((key.clone(), attr_count(list, "itemCnt"), items.len()));
        header.para_list_key = Some(key);
        for node in items {
            header.para_prs.push(parse_para_pr(xml, node)?);
        }
    }

    if let Some(list) = child_el(ref_list, NS_HH, "styles") {
        let key = xml.key(list).unwrap_or_default().to_string();
        let items = child_els(list, NS_HH, "style");
        header.item_counts.push((key.clone(), attr_count(list, "itemCnt"), items.len()));
        header.style_list_key = Some(key);
        for node in items {
            header.styles.push(PStyle {
                id: xml.attr_u32(node, "id").ok_or("hh:style without id")?,
                key: xml.key(node).unwrap_or_default().to_string(),
                name: node.attribute("name").unwrap_or("").to_string(),
                para_pr: xml.attr_u32(node, "paraPrIDRef").unwrap_or(0),
                char_pr: xml.attr_u32(node, "charPrIDRef").unwrap_or(0),
            });
        }
    }
    for (name, child) in [("tabProperties", "tabPr"), ("numberings", "numbering")] {
        if let Some(list) = child_el(ref_list, NS_HH, name) {
            header.item_counts.push((xml.key(list).unwrap_or_default().to_string(), attr_count(list, "itemCnt"), child_els(list, NS_HH, child).len()));
        }
    }
    header.validate_refs()?;
    Ok(header)
}

fn attr_count(node: Node<'_, '_>, name: &str) -> Option<usize> {
    node.attribute(name)?.trim().parse().ok()
}

fn parse_char_pr(xml: &Xml<'_>, node: Node<'_, '_>, fonts: &[FontList]) -> Result<PCharPr, String> {
    let id = xml.attr_u32(node, "id").ok_or("hh:charPr without id")?;
    let height = xml.attr_i64(node, "height").unwrap_or(1000);
    let text_color = node.attribute("textColor").unwrap_or("#000000").to_string();
    let mut font_ids = [0u32; 7];
    let font_ref = child_el(node, NS_HH, "fontRef");
    if let Some(f) = font_ref {
        for (i, attr) in FONT_REF_ATTRS.iter().enumerate() {
            font_ids[i] = xml.attr_u32(f, attr).unwrap_or(0);
        }
    }
    let bold = child_el(node, NS_HH, "bold").and_then(|n| xml.key(n).map(str::to_string));
    let italic = child_el(node, NS_HH, "italic").and_then(|n| xml.key(n).map(str::to_string));
    let underline = child_el(node, NS_HH, "underline").and_then(|n| xml.key(n).map(str::to_string));
    let face = font_face_for(fonts, 0, font_ids[0]);
    let style = Style {
        font_family: face,
        font_size: hu_to_emu(height),
        color: text_color,
        bold: bold.is_some(),
        italic: italic.is_some(),
        ..Style::default()
    };
    Ok(PCharPr {
        id,
        key: xml.key(node).unwrap_or_default().to_string(),
        bold_key: bold,
        italic_key: italic,
        underline_key: underline,
        font_ref_key: font_ref.and_then(|n| xml.key(n).map(str::to_string)),
        style,
    })
}

fn parse_para_pr(xml: &Xml<'_>, node: Node<'_, '_>) -> Result<PParaPr, String> {
    let id = xml.attr_u32(node, "id").ok_or("hh:paraPr without id")?;
    let align_node = child_el(node, NS_HH, "align");
    let align_raw = align_node.and_then(|a| a.attribute("horizontal")).unwrap_or("JUSTIFY");
    let align = align_raw.to_ascii_lowercase();
    let prev_nodes = descendant_els(node, NS_HC, "prev");
    let next_nodes = descendant_els(node, NS_HC, "next");
    let intent_nodes = descendant_els(node, NS_HC, "intent");
    let line_nodes = descendant_els(node, NS_HH, "lineSpacing");
    let value_of = |n: &Node<'_, '_>| xml.attr_i64(*n, "value").unwrap_or(0);
    let line_type = line_nodes.first().and_then(|n| n.attribute("type")).unwrap_or("PERCENT").to_string();
    let line_value = line_nodes.first().and_then(|n| xml.attr_i64(*n, "value")).unwrap_or(if line_type == "PERCENT" { 160 } else { 0 });
    let line = NativeLine { kind: line_type, value: line_value };
    let break_node = child_el(node, NS_HH, "breakSetting");
    let format = ParagraphFormat {
        align,
        space_before: prev_nodes.first().map(|n| hu_to_emu(value_of(n))).unwrap_or(0),
        space_after: next_nodes.first().map(|n| hu_to_emu(value_of(n))).unwrap_or(0),
        indent: intent_nodes.first().map(|n| hu_to_emu(value_of(n))).unwrap_or(0),
        page_break_before: matches!(break_node.and_then(|n| n.attribute("pageBreakBefore")), Some("1" | "true")),
        line_spacing_percent: line.percent(),
        ..ParagraphFormat::default()
    };
    let key_of = |v: &[Node<'_, '_>]| -> Vec<String> { v.iter().filter_map(|n| xml.key(*n).map(str::to_string)).collect() };
    Ok(PParaPr {
        id,
        key: xml.key(node).unwrap_or_default().to_string(),
        align_key: align_node.and_then(|a| xml.key(a).map(str::to_string)),
        prev_keys: key_of(&prev_nodes),
        next_keys: key_of(&next_nodes),
        intent_keys: key_of(&intent_nodes),
        line_keys: key_of(&line_nodes),
        uniform_line: line_nodes.iter().all(|n| n.attribute("type") == Some(line.kind.as_str()) && xml.attr_i64(*n, "value") == Some(line.value)),
        line,
        break_key: break_node.and_then(|n| xml.key(n).map(str::to_string)),
        format,
    })
}

/// Allowed paragraph alignment names in the model and their HWPX spellings.
pub fn align_is_supported(align: &str) -> bool {
    matches!(align, "left" | "right" | "center" | "justify" | "distribute" | "distribute_space")
}

/// Plans header additions and checks each addition against existing properties.
pub struct HeaderPlan {
    pub ov: Overrides,
    char_norm: Vec<(String, u32)>,
    para_norm: Vec<(String, u32)>,
    char_added: Vec<(u32, Vec<u8>)>,
    para_added: Vec<(u32, Vec<u8>)>,
    font_added: [Vec<(u32, String)>; 7],
    char_next: u32,
    para_next: u32,
    font_next: [u32; 7],
    style_refs: BTreeMap<String, u32>,
}

impl HeaderPlan {
    pub fn new(xml: &Xml<'_>, h: &PHeader) -> Result<Self, String> {
        let mut char_norm = Vec::new();
        for cp in &h.char_prs {
            char_norm.push((normal_form(xml, &cp.key, &Overrides::default())?, cp.id));
        }
        let mut para_norm = Vec::new();
        for pp in &h.para_prs {
            para_norm.push((normal_form(xml, &pp.key, &Overrides::default())?, pp.id));
        }
        let mut font_next = [0u32; 7];
        for (i, lang) in LANGS.iter().enumerate() {
            let max = h.font_list(lang).and_then(|f| f.fonts.iter().map(|(id, _)| *id).max());
            font_next[i] = max.map(|m| m + 1).unwrap_or(0);
        }
        Ok(HeaderPlan {
            ov: Overrides::default(),
            char_norm,
            para_norm,
            char_added: Vec::new(),
            para_added: Vec::new(),
            font_added: Default::default(),
            char_next: h.char_prs.iter().map(|c| c.id).max().map(|m| m + 1).unwrap_or(0),
            para_next: h.para_prs.iter().map(|p| p.id).max().map(|m| m + 1).unwrap_or(0),
            font_next,
            style_refs: BTreeMap::new(),
        })
    }

    pub fn has_changes(&self) -> bool {
        !self.char_added.is_empty()
            || !self.para_added.is_empty()
            || self.font_added.iter().any(|f| !f.is_empty())
            || !self.style_refs.is_empty()
    }

    /// Font ids for a face name in all seven language slots, adding the face where missing.
    pub fn font_ids_for(&mut self, h: &PHeader, face: &str) -> Result<[u32; 7], String> {
        let mut ids = [0u32; 7];
        for (i, lang) in LANGS.iter().enumerate() {
            let list = h.font_list(lang).ok_or_else(|| format!("header has no {lang} font list"))?;
            if let Some((id, _)) = list.fonts.iter().find(|(_, f)| f.as_str() == face) {
                ids[i] = *id;
            } else if let Some((id, _)) = self.font_added[i].iter().find(|(_, f)| f.as_str() == face) {
                ids[i] = *id;
            } else {
                let id = self.font_next[i];
                self.font_next[i] += 1;
                self.font_added[i].push((id, face.to_string()));
                ids[i] = id;
            }
        }
        Ok(ids)
    }

    /// Id of a character shape for `style`, reusing the base when it already matches.
    pub fn char_pr_for(&mut self, xml: &Xml<'_>, h: &PHeader, base: &PCharPr, style: &Style) -> Result<u32, String> {
        if base.style == *style {
            return Ok(base.id);
        }
        let mut ov = Overrides::default();
        if style.font_family != base.style.font_family {
            let ids = self.font_ids_for(h, &style.font_family)?;
            let fr = base.font_ref_key.clone().ok_or("charPr without fontRef cannot change font")?;
            for (i, attr) in FONT_REF_ATTRS.iter().enumerate() {
                ov.set_attr(&fr, attr, ids[i].to_string());
            }
        }
        if style.font_size % EMU_PER_HWPUNIT != 0 {
            return Err(format!("font size {} is not on the native grid", style.font_size));
        }
        ov.set_attr(&base.key, "height", emu_to_hu(style.font_size).to_string());
        ov.set_attr(&base.key, "textColor", style.color.clone());
        toggle_child(&mut ov, base.bold_key.as_deref(), style.bold, base.underline_key.as_deref(), "bold", base.key.as_str());
        toggle_child(&mut ov, base.italic_key.as_deref(), style.italic, base.underline_key.as_deref(), "italic", base.key.as_str());
        self.intern(xml, &base.key, ov, 'c')
    }

    /// Reuse a shape only when its native line spacing also matches.
    pub fn para_pr_for(&mut self, xml: &Xml<'_>, base: &PParaPr, format: &ParagraphFormat, line: Option<&NativeLine>) -> Result<u32, String> {
        let b = &base.format;
        let line_edit = line.filter(|l| base.line != **l || !base.uniform_line);
        // The shape's own break stays on for every paragraph that uses it, so a paragraph that must not start
        // a page gets a clone without it. A break the paragraph sets itself (`pageBreak`) needs no clone.
        let break_cleared = b.page_break_before && !format.page_break_before;
        if !break_cleared && b.align == format.align && b.space_before == format.space_before && b.space_after == format.space_after && b.indent == format.indent && line_edit.is_none() {
            return Ok(base.id);
        }
        if !align_is_supported(&format.align) {
            return Err(format!("alignment {} is not supported by HWPX", format.align));
        }
        let mut ov = Overrides::default();
        if b.align != format.align {
            let key = base.align_key.clone().ok_or("paraPr without hh:align")?;
            ov.set_attr(&key, "horizontal", format.align.to_ascii_uppercase());
        }
        for (what, element, old, new, keys) in [
            ("space before", "hc:prev", b.space_before, format.space_before, &base.prev_keys),
            ("space after", "hc:next", b.space_after, format.space_after, &base.next_keys),
            ("indent", "hc:intent", b.indent, format.indent, &base.intent_keys),
        ] {
            if old != new {
                // A shape without the element cannot take the value, so the edit is refused, not dropped.
                if keys.is_empty() {
                    return Err(format!("paraPr without {element} cannot change its {what}"));
                }
                if new % EMU_PER_HWPUNIT != 0 {
                    return Err(format!("paragraph spacing {new} is not on the native grid"));
                }
                for key in keys {
                    ov.set_attr(key, "value", emu_to_hu(new).to_string());
                }
            }
        }
        if let Some(line) = line_edit {
            if base.line_keys.is_empty() {
                return Err("paraPr without hh:lineSpacing cannot change its line spacing".into());
            }
            for key in &base.line_keys {
                ov.set_attr(key, "type", &line.kind);
                ov.set_attr(key, "value", line.value.to_string());
            }
        }
        if break_cleared {
            let key = base.break_key.clone().ok_or("paraPr with a page break and no hh:breakSetting cannot clear it")?;
            ov.set_attr(&key, "pageBreakBefore", "0");
        }
        self.intern(xml, &base.key, ov, 'p')
    }

    pub fn set_style_char_pr(&mut self, style: &PStyle, char_pr: u32) {
        if style.char_pr != char_pr {
            self.style_refs.insert(style.key.clone(), char_pr);
        }
    }

    /// Renders the candidate, reuses an identical existing or planned clone, or plans a new one.
    fn intern(&mut self, xml: &Xml<'_>, key: &str, ov: Overrides, kind: char) -> Result<u32, String> {
        let norm = normal_form(xml, key, &ov)?;
        let (norm_list, next, added) = match kind {
            'c' => (&mut self.char_norm, &mut self.char_next, &mut self.char_added),
            _ => (&mut self.para_norm, &mut self.para_next, &mut self.para_added),
        };
        if let Some((_, id)) = norm_list.iter().find(|(n, _)| *n == norm) {
            return Ok(*id);
        }
        let id = *next;
        *next += 1;
        let mut with_id = ov;
        with_id.set_attr(key, "id", id.to_string());
        let bytes = render_key(xml, key, &with_id)?;
        added.push((id, bytes));
        norm_list.push((norm, id));
        Ok(id)
    }

    /// Final overrides for the header part. Takes the planned overrides out of the plan.
    pub fn finish(&mut self, xml: &Xml<'_>, h: &PHeader) -> Result<Overrides, String> {
        let mut ov = std::mem::take(&mut self.ov);
        if !self.char_added.is_empty() {
            let list = h.char_list_key.clone().ok_or("header has no charProperties list")?;
            let count = h.char_prs.len() + self.char_added.len();
            ov.append.insert(list.clone(), self.char_added.iter().map(|(_, b)| b.clone()).collect());
            ov.set_attr(&list, "itemCnt", count.to_string());
        }
        if !self.para_added.is_empty() {
            let list = h.para_list_key.clone().ok_or("header has no paraProperties list")?;
            let count = h.para_prs.len() + self.para_added.len();
            ov.append.insert(list.clone(), self.para_added.iter().map(|(_, b)| b.clone()).collect());
            ov.set_attr(&list, "itemCnt", count.to_string());
        }
        for (i, lang) in LANGS.iter().enumerate() {
            if self.font_added[i].is_empty() {
                continue;
            }
            let list = h.font_list(lang).ok_or_else(|| format!("header has no {lang} font list"))?;
            let existing = list.fonts.len();
            let bytes: Vec<Vec<u8>> = self.font_added[i]
                .iter()
                .map(|(id, face)| {
                    format!(
                        "<hh:font id=\"{id}\" face=\"{}\" type=\"TTF\" isEmbedded=\"0\">{DEFAULT_TYPE_INFO}</hh:font>",
                        crate::xmlx::esc_attr(face)
                    )
                    .into_bytes()
                })
                .collect();
            ov.append.insert(list.key.clone(), bytes);
            ov.set_attr(&list.key, "fontCnt", (existing + self.font_added[i].len()).to_string());
        }
        for (style_key, char_pr) in &self.style_refs {
            ov.set_attr(style_key, "charPrIDRef", char_pr.to_string());
        }
        let _ = xml;
        Ok(ov)
    }

}

/// Clone-comparison form: the element rendered with its `id` attribute fixed to a placeholder.
fn normal_form(xml: &Xml<'_>, key: &str, ov: &Overrides) -> Result<String, String> {
    let mut with_id = ov.clone();
    with_id.set_attr(key, "id", "@");
    let bytes = render_key(xml, key, &with_id)?;
    String::from_utf8(bytes).map_err(|e| e.to_string())
}

fn toggle_child(ov: &mut Overrides, child: Option<&str>, want: bool, underline: Option<&str>, name: &str, parent: &str) {
    match (child, want) {
        (Some(key), false) => {
            ov.drop.insert(key.to_string());
        }
        (None, true) => {
            let element = format!("<hh:{name}/>").into_bytes();
            match underline {
                Some(u) => ov.before.entry(u.to_string()).or_default().push(element),
                None => ov.append.entry(parent.to_string()).or_default().push(element),
            }
        }
        _ => {}
    }
}
