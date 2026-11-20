//! Rich text model for slide text bodies.
//!
//! A text body is a list of paragraphs. Each paragraph is a list of units. A unit is one character with
//! its style, a line break (`a:br`, logical U+000B), or a protected inline element (`a:fld` or any other
//! paragraph child, logical U+FFFC). Plain text joins paragraphs with `\n`. A protected unit cannot be
//! removed or restyled by a text edit, because its source bytes are copied rather than rebuilt.

use designllm_contracts::{grapheme_to_scalar, DltError, DltResult, ErrorCode, Style, StylePatch};

use crate::model::{ParagraphMap, SegmentMap};

pub const BREAK_CHAR: char = '\u{000B}';
pub const ATOM_CHAR: char = '\u{FFFC}';

/// One logical character of a paragraph.
#[derive(Clone, Debug, PartialEq)]
pub enum Unit {
    Char { ch: char, style: Style },
    Break,
    Atom { element: String, display: String, style: Style },
}

impl Unit {
    /// The logical character this unit contributes to the plain text.
    pub fn plain(&self) -> char {
        match self {
            Unit::Char { ch, .. } => *ch,
            Unit::Break => BREAK_CHAR,
            Unit::Atom { .. } => ATOM_CHAR,
        }
    }

    pub fn style(&self) -> Option<&Style> {
        match self {
            Unit::Char { style, .. } | Unit::Atom { style, .. } => Some(style),
            Unit::Break => None,
        }
    }
}

/// Paragraph-level formatting that the model edits.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParaProps {
    pub align: String,
    pub space_before: i64,
    pub space_after: i64,
    pub line_spacing_pct: u32,
}

impl Default for ParaProps {
    fn default() -> Self {
        Self { align: "left".into(), space_before: 0, space_after: 0, line_spacing_pct: 100 }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Para {
    pub props: ParaProps,
    pub units: Vec<Unit>,
}

impl Para {
    pub fn plain(&self) -> String {
        self.units.iter().map(Unit::plain).collect()
    }
}

/// Paragraphs of a text body.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RichText {
    pub paragraphs: Vec<Para>,
}

/// Flat editing unit: a character position, or a paragraph separator that carries the properties of the
/// paragraph that starts after it.
#[derive(Clone, Debug)]
enum Flat {
    Unit(Unit),
    Sep(ParaProps),
}

fn malformed(message: impl Into<String>) -> DltError {
    DltError::new(ErrorCode::IntegrityError, message)
}

impl RichText {
    /// Rich text from plain text: one paragraph per newline, every character in `style`, and `props` on every
    /// paragraph. Inline-element characters are refused, because they can only come from source structure.
    pub fn from_plain(text: &str, style: &Style, props: &ParaProps) -> DltResult<Self> {
        // Refused before anything is built: the run style keeps only the font fields, so a v1.2 value would be lost.
        crate::guard::check_style(style, "in text", None)?;
        if text.contains(ATOM_CHAR) {
            return Err(DltError::unsupported("inline fields cannot be created from plain text"));
        }
        let paragraphs = text
            .split('\n')
            .map(|line| Para {
                props: props.clone(),
                units: line
                    .chars()
                    .map(|ch| if ch == BREAK_CHAR { Unit::Break } else { Unit::Char { ch, style: canonical_run_style(style.clone()) } })
                    .collect(),
            })
            .collect();
        Ok(Self { paragraphs })
    }

    /// Builds the rich text from plain text and segment maps. The two must agree exactly.
    pub fn from_parts(text: &str, maps: &[ParagraphMap]) -> DltResult<Self> {
        if maps.is_empty() {
            if text.is_empty() {
                return Ok(Self::default());
            }
            return Err(malformed("text without paragraph structure"));
        }
        let texts: Vec<&str> = text.split('\n').collect();
        if texts.len() != maps.len() {
            return Err(malformed(format!(
                "paragraph count mismatch: {} text paragraphs for {} mapped paragraphs",
                texts.len(),
                maps.len()
            )));
        }
        let mut paragraphs = Vec::with_capacity(maps.len());
        for (para_text, map) in texts.iter().zip(maps) {
            let mut chars = para_text.chars();
            let mut units = Vec::new();
            for segment in &map.segments {
                match segment {
                    SegmentMap::Run { chars: count, style } => {
                        for _ in 0..*count {
                            let ch = chars.next().ok_or_else(|| malformed("run longer than its paragraph text"))?;
                            if ch == BREAK_CHAR || ch == ATOM_CHAR {
                                return Err(malformed("run contains a protected character"));
                            }
                            units.push(Unit::Char { ch, style: style.clone() });
                        }
                    }
                    SegmentMap::Break => {
                        if chars.next() != Some(BREAK_CHAR) {
                            return Err(malformed("line break missing from paragraph text"));
                        }
                        units.push(Unit::Break);
                    }
                    SegmentMap::Atom { element, display, style } => {
                        if chars.next() != Some(ATOM_CHAR) {
                            return Err(malformed("inline element missing from paragraph text"));
                        }
                        units.push(Unit::Atom { element: element.clone(), display: display.clone(), style: style.clone() });
                    }
                }
            }
            if chars.next().is_some() {
                return Err(malformed("paragraph text longer than its segments"));
            }
            paragraphs.push(Para { props: props_of(map), units });
        }
        Ok(Self { paragraphs })
    }

    /// Plain text (paragraphs joined by `\n`) and the segment maps that rebuild this rich text.
    pub fn to_parts(&self) -> (String, Vec<ParagraphMap>) {
        let text = self.plain();
        let maps = self.paragraphs.iter().map(map_of).collect();
        (text, maps)
    }

    pub fn plain(&self) -> String {
        self.paragraphs.iter().map(Para::plain).collect::<Vec<_>>().join("\n")
    }

    /// Number of flat positions: units plus one separator between paragraphs.
    pub fn flat_len(&self) -> usize {
        let units: usize = self.paragraphs.iter().map(|p| p.units.len()).sum();
        units + self.paragraphs.len().saturating_sub(1)
    }

    /// Converts a grapheme range of one paragraph to flat positions. Graphemes follow Unicode 16.0.
    pub fn flat_range(&self, paragraph: usize, grapheme_start: u32, grapheme_end: u32) -> DltResult<(usize, usize)> {
        let para = self
            .paragraphs
            .get(paragraph)
            .ok_or_else(|| DltError::new(ErrorCode::NotFound, format!("paragraph {paragraph} does not exist")))?;
        let text = para.plain();
        let start = grapheme_to_scalar(&text, grapheme_start)?;
        let end = grapheme_to_scalar(&text, grapheme_end)?;
        if start > end {
            return Err(DltError::invalid("text range start is after its end"));
        }
        let offset: usize = self.paragraphs[..paragraph].iter().map(|p| p.units.len() + 1).sum();
        Ok((offset + start, offset + end))
    }

    /// Replaces flat positions `start..end` with `insert`. Text edits never remove or restyle a
    /// protected unit. Inserted characters take `insert_style`, or else the style of the character before
    /// the insertion point, or else the character after it.
    pub fn replace(&mut self, start: usize, end: usize, insert: &str, insert_style: Option<Style>) -> DltResult<()> {
        let mut flat = self.flatten();
        if start > end || end > flat.len() {
            return Err(DltError::invalid("text range is outside the paragraph"));
        }
        if flat[start..end].iter().any(|f| matches!(f, Flat::Unit(Unit::Atom { .. }))) {
            return Err(DltError::unsupported(
                "text edit would remove or replace an inline field; the protected element is kept as-is",
            ));
        }
        let head_props = self.paragraphs.first().map(|p| p.props.clone()).unwrap_or_default();
        let current_props = props_before(&flat, start, &head_props);
        // A replacement takes the style of the first replaced character. An insertion takes the style of the
        // character before it, or of the character after it at the start of a paragraph.
        let char_style = |item: Option<&Flat>| match item {
            Some(Flat::Unit(Unit::Char { style, .. })) => Some(style.clone()),
            _ => None,
        };
        let style = if start < end {
            char_style(flat.get(start))
        } else {
            char_style(start.checked_sub(1).and_then(|i| flat.get(i))).or_else(|| char_style(flat.get(end)))
        }
        .or(insert_style)
        .map(canonical_run_style)
        .unwrap_or_else(|| canonical_run_style(Style::default()));

        let normalized = insert.replace("\r\n", "\n").replace('\r', "\n");
        let mut items = Vec::with_capacity(normalized.len());
        for ch in normalized.chars() {
            match ch {
                '\n' => items.push(Flat::Sep(current_props.clone())),
                BREAK_CHAR => items.push(Flat::Unit(Unit::Break)),
                ATOM_CHAR => {
                    return Err(DltError::unsupported("inline fields cannot be inserted as text"));
                }
                c if (c as u32) < 0x20 && c != '\t' => {
                    return Err(DltError::invalid(format!("control character U+{:04X} cannot be stored in text", c as u32)));
                }
                c => items.push(Flat::Unit(Unit::Char { ch: c, style: style.clone() })),
            }
        }
        flat.splice(start..end, items);
        self.rebuild(flat, head_props);
        Ok(())
    }

    /// Applies a font patch to characters in flat positions `start..end`. Protected units in the range
    /// are refused, so a reported style never differs from the saved one.
    pub fn restyle(&mut self, start: usize, end: usize, patch: &StylePatch) -> DltResult<()> {
        // Refused before the runs change: `apply` would store a v1.2 value that the saved runs cannot hold.
        crate::guard::check_patch(patch, "in text formatting", None)?;
        let mut flat = self.flatten();
        if start > end || end > flat.len() {
            return Err(DltError::invalid("style range is outside the paragraph"));
        }
        if flat[start..end].iter().any(|f| matches!(f, Flat::Unit(Unit::Atom { .. }))) {
            return Err(DltError::unsupported("style of an inline field cannot be changed"));
        }
        for item in &mut flat[start..end] {
            if let Flat::Unit(Unit::Char { style, .. }) = item {
                patch.apply(style);
                *style = canonical_run_style(style.clone());
            }
        }
        let head_props = self.paragraphs.first().map(|p| p.props.clone()).unwrap_or_default();
        self.rebuild(flat, head_props);
        Ok(())
    }

    /// Sets paragraph properties on one paragraph.
    pub fn set_props(&mut self, paragraph: usize, props: ParaProps) -> DltResult<()> {
        let para = self
            .paragraphs
            .get_mut(paragraph)
            .ok_or_else(|| DltError::new(ErrorCode::NotFound, format!("paragraph {paragraph} does not exist")))?;
        para.props = props;
        Ok(())
    }

    fn flatten(&self) -> Vec<Flat> {
        let mut out = Vec::with_capacity(self.flat_len());
        for (index, para) in self.paragraphs.iter().enumerate() {
            if index > 0 {
                out.push(Flat::Sep(para.props.clone()));
            }
            out.extend(para.units.iter().cloned().map(Flat::Unit));
        }
        out
    }

    fn rebuild(&mut self, flat: Vec<Flat>, head_props: ParaProps) {
        let mut paragraphs = vec![Para { props: head_props, units: Vec::new() }];
        for item in flat {
            match item {
                Flat::Sep(props) => paragraphs.push(Para { props, units: Vec::new() }),
                Flat::Unit(unit) => paragraphs.last_mut().expect("at least one paragraph").units.push(unit),
            }
        }
        self.paragraphs = paragraphs;
    }
}

/// Properties of the paragraph that contains flat position `position`.
fn props_before(flat: &[Flat], position: usize, head: &ParaProps) -> ParaProps {
    let mut current = head.clone();
    for item in &flat[..position.min(flat.len())] {
        if let Flat::Sep(props) = item {
            current = props.clone();
        }
    }
    current
}

fn props_of(map: &ParagraphMap) -> ParaProps {
    ParaProps {
        align: map.align.clone(),
        space_before: map.space_before,
        space_after: map.space_after,
        line_spacing_pct: map.line_spacing_pct,
    }
}

fn map_of(para: &Para) -> ParagraphMap {
    let mut segments: Vec<SegmentMap> = Vec::new();
    for unit in &para.units {
        match unit {
            Unit::Char { style, .. } => {
                if let Some(SegmentMap::Run { chars, style: last }) = segments.last_mut() {
                    if last == style {
                        *chars += 1;
                        continue;
                    }
                }
                segments.push(SegmentMap::Run { chars: 1, style: style.clone() });
            }
            Unit::Break => segments.push(SegmentMap::Break),
            Unit::Atom { element, display, style } => segments.push(SegmentMap::Atom {
                element: element.clone(),
                display: display.clone(),
                style: style.clone(),
            }),
        }
    }
    ParagraphMap {
        align: para.props.align.clone(),
        space_before: para.props.space_before,
        space_after: para.props.space_after,
        line_spacing_pct: para.props.line_spacing_pct,
        segments,
    }
}

/// Run styles carry font fields only. Shape-level fields (fill, stroke, stroke width) and the paragraph alignment
/// have fixed neutral values on runs, so every run style in the model has the same shape. The reader produces the same.
pub fn canonical_run_style(mut style: Style) -> Style {
    style.fill = "none".into();
    style.stroke = "none".into();
    style.stroke_width = 0;
    style.align = "left".into();
    style
}

/// Font-related equality. Shape-level fields (fill, stroke, stroke width) are not part of run formatting.
pub fn font_eq(a: &Style, b: &Style) -> bool {
    a.font_family == b.font_family
        && a.font_size == b.font_size
        && a.color == b.color
        && a.bold == b.bold
        && a.italic == b.italic
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::run_style;

    fn plain_style() -> Style {
        run_style("Arial".into(), 1800 * 127, "#000000".into(), false, false, "left")
    }

    fn bold_style() -> Style {
        run_style("Arial".into(), 1800 * 127, "#000000".into(), true, false, "left")
    }

    #[test]
    fn from_parts_and_to_parts_roundtrip_with_breaks_and_atoms() {
        let maps = vec![ParagraphMap {
            align: "center".into(),
            space_before: 0,
            space_after: 0,
            line_spacing_pct: 100,
            segments: vec![
                SegmentMap::Run { chars: 2, style: plain_style() },
                SegmentMap::Break,
                SegmentMap::Atom { element: "fld".into(), display: "3".into(), style: plain_style() },
                SegmentMap::Run { chars: 1, style: bold_style() },
            ],
        }];
        let text = format!("ab{BREAK_CHAR}{ATOM_CHAR}c");
        let rich = RichText::from_parts(&text, &maps).unwrap();
        assert_eq!(rich.paragraphs[0].units.len(), 5);
        let (rebuilt_text, rebuilt_maps) = rich.to_parts();
        assert_eq!(rebuilt_text, text);
        assert_eq!(rebuilt_maps, maps);
    }

    #[test]
    fn inconsistent_structure_is_an_integrity_error() {
        let maps = vec![ParagraphMap {
            align: "left".into(),
            space_before: 0,
            space_after: 0,
            line_spacing_pct: 100,
            segments: vec![SegmentMap::Run { chars: 5, style: plain_style() }],
        }];
        assert_eq!(RichText::from_parts("abc", &maps).unwrap_err().code, ErrorCode::IntegrityError);
        assert!(RichText::from_parts("a\nb", &maps).is_err());
    }

    #[test]
    fn insertion_takes_the_style_of_the_preceding_character() {
        let mut rich = RichText::from_parts(
            "AB",
            &[ParagraphMap {
                align: "left".into(),
                space_before: 0,
                space_after: 0,
                line_spacing_pct: 100,
                segments: vec![SegmentMap::Run { chars: 1, style: plain_style() }, SegmentMap::Run { chars: 1, style: bold_style() }],
            }],
        )
        .unwrap();
        rich.replace(1, 1, "x", None).unwrap();
        assert_eq!(rich.plain(), "AxB");
        let styles: Vec<bool> = rich.paragraphs[0].units.iter().map(|u| u.style().unwrap().bold).collect();
        assert_eq!(styles, vec![false, false, true]);
    }

    #[test]
    fn replacing_text_never_removes_a_protected_atom() {
        let mut rich = RichText::from_parts(
            &format!("a{ATOM_CHAR}b"),
            &[ParagraphMap {
                align: "left".into(),
                space_before: 0,
                space_after: 0,
                line_spacing_pct: 100,
                segments: vec![
                    SegmentMap::Run { chars: 1, style: plain_style() },
                    SegmentMap::Atom { element: "fld".into(), display: "1".into(), style: plain_style() },
                    SegmentMap::Run { chars: 1, style: plain_style() },
                ],
            }],
        )
        .unwrap();
        let error = rich.replace(0, 2, "z", None).unwrap_err();
        assert_eq!(error.code, ErrorCode::UnsupportedOperation);
        assert!(rich.replace(2, 3, "", None).is_ok(), "text after the field is editable");
        assert_eq!(rich.plain(), format!("a{ATOM_CHAR}"));
    }

    #[test]
    fn newline_splits_and_merges_paragraphs_keeping_the_first_paragraph_props() {
        let mut rich = RichText::from_parts(
            "ab",
            &[ParagraphMap {
                align: "right".into(),
                space_before: 0,
                space_after: 0,
                line_spacing_pct: 100,
                segments: vec![SegmentMap::Run { chars: 2, style: plain_style() }],
            }],
        )
        .unwrap();
        rich.replace(1, 1, "\n", None).unwrap();
        assert_eq!(rich.plain(), "a\nb");
        assert_eq!(rich.paragraphs.len(), 2);
        assert_eq!(rich.paragraphs[1].props.align, "right", "the split paragraph keeps the alignment");
        rich.replace(1, 2, "", None).unwrap();
        assert_eq!(rich.paragraphs.len(), 1, "removing the separator merges paragraphs");
        assert_eq!(rich.plain(), "ab");
    }

    #[test]
    fn restyle_applies_only_to_the_covered_units_and_refuses_atoms() {
        let mut rich = RichText::from_parts(
            "abcd",
            &[ParagraphMap {
                align: "left".into(),
                space_before: 0,
                space_after: 0,
                line_spacing_pct: 100,
                segments: vec![SegmentMap::Run { chars: 4, style: plain_style() }],
            }],
        )
        .unwrap();
        let patch = StylePatch { bold: Some(true), ..StylePatch::default() };
        rich.restyle(1, 3, &patch).unwrap();
        let bold: Vec<bool> = rich.paragraphs[0].units.iter().map(|u| u.style().unwrap().bold).collect();
        assert_eq!(bold, vec![false, true, true, false]);
        assert_eq!(rich.to_parts().1[0].segments.len(), 3, "runs split at the style boundary");
    }

    #[test]
    fn grapheme_ranges_map_to_flat_positions_across_paragraphs() {
        let rich = RichText::from_parts(
            "x\ne\u{0301}yz",
            &[
                ParagraphMap { align: "left".into(), space_before: 0, space_after: 0, line_spacing_pct: 100, segments: vec![SegmentMap::Run { chars: 1, style: plain_style() }] },
                ParagraphMap { align: "left".into(), space_before: 0, space_after: 0, line_spacing_pct: 100, segments: vec![SegmentMap::Run { chars: 4, style: plain_style() }] },
            ],
        )
        .unwrap();
        // Paragraph 1 is "e\u{0301}yz": graphemes e+acute, y, z. Grapheme 1..2 is "y".
        let (start, end) = rich.flat_range(1, 1, 2).unwrap();
        assert_eq!((start, end), (4, 5), "units: x=0, separator=1, e=2, combining=3, y=4");
        assert!(rich.flat_range(1, 2, 1).is_err());
    }
}
