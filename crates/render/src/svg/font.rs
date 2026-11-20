//! Subset the bundled static TrueType faces for editable SVG. Keep glyph IDs stable so the original
//! shaping and positioning tables still work. PDF's CID subset omits the Unicode map a browser needs.
use crate::fonts::Face;
use designllm_contracts::ShapedText;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use ttf_parser::{gsub, loca, opentype_layout::Coverage, GlyphId, Tag};

pub(super) struct EmbeddedFont {
    pub face: Arc<Face>,
    used: BTreeSet<u16>,
}

impl EmbeddedFont {
    pub fn new(face: Arc<Face>) -> Self {
        Self { face, used: BTreeSet::new() }
    }

    pub fn include(&mut self, text: &str, shaped: &ShapedText) {
        self.used.extend(text.chars().filter_map(|c| self.face.ttf.glyph_index(c).map(|g| g.0)));
        self.used.extend(shaped.glyphs.iter().filter(|g| g.font_key == self.face.key)
            .filter_map(|g| u16::try_from(g.glyph_id).ok()));
    }

    pub fn subset(&self) -> Option<Vec<u8>> {
        let face = &self.face.ttf;
        if !self.face.data.starts_with(&[0, 1, 0, 0]) || !face.is_subsetting_allowed() {
            return None;
        }
        let raw = face.raw_face();
        let table = |tag| raw.table(Tag::from_bytes(tag));
        let glyphs = table(b"glyf")?;
        let locations = loca::Table::parse(face.tables().maxp.number_of_glyphs,
            face.tables().head.index_to_location_format, table(b"loca")?)?;
        let mut keep = self.used.clone();
        keep.insert(0); // .notdef
        keep_substitutions(face, &mut keep);
        let mut pending: Vec<_> = keep.iter().copied().collect();
        while let Some(id) = pending.pop() {
            if let Some(range) = locations.glyph_range(GlyphId(id)) {
                for_each_component(glyphs.get(range)?, |component| {
                    if keep.insert(component) { pending.push(component); }
                })?;
            }
        }
        // Keep mappings to all retained glyphs: rasterizers use reference letters for auto-hinting.
        let cmap = unicode_map(face, &keep)?;

        let mut glyf = Vec::new();
        let mut loca = Vec::new();
        for id in 0..face.number_of_glyphs() {
            loca.extend_from_slice(&u32::try_from(glyf.len()).ok()?.to_be_bytes());
            if keep.contains(&id) {
                if let Some(range) = locations.glyph_range(GlyphId(id)) {
                    glyf.extend_from_slice(glyphs.get(range)?);
                    glyf.resize(glyf.len().next_multiple_of(2), 0);
                }
            }
        }
        loca.extend_from_slice(&u32::try_from(glyf.len()).ok()?.to_be_bytes());

        let records = raw.table_records;
        let mut out = self.face.data.get(..12 + usize::from(records.len()) * 16)?.to_vec();
        let mut adjustment = None;
        for (i, record) in records.into_iter().enumerate() {
            let start = out.len();
            let original = raw.table(record.tag)?;
            match &record.tag.to_bytes() {
                b"glyf" => out.extend_from_slice(&glyf),
                b"loca" => out.extend_from_slice(&loca),
                b"cmap" => out.extend_from_slice(&cmap),
                b"head" => {
                    out.extend_from_slice(original);
                    out.get_mut(start + 8..start + 12)?.fill(0);
                    out.get_mut(start + 50..start + 52)?.copy_from_slice(&1u16.to_be_bytes());
                    adjustment = Some(start + 8);
                }
                b"post" => {
                    // Version 3 keeps the font metrics and omits the list of glyph names.
                    out.extend_from_slice(original.get(..32)?);
                    out[start..start + 4].copy_from_slice(&0x0003_0000u32.to_be_bytes());
                }
                // The shipped faces have an empty signature table and no variation or colour data.
                b"DSIG" if original == [0, 0, 0, 1, 0, 0, 0, 0] => out.extend_from_slice(original),
                b"GDEF" | b"GPOS" | b"GSUB" | b"OS/2" | b"hhea" | b"hmtx" | b"maxp" | b"name" => out.extend_from_slice(original),
                _ => return None,
            }
            let length = u32::try_from(out.len() - start).ok()?;
            out.resize(out.len().next_multiple_of(4), 0);
            let sum = checksum(&out[start..]);
            let entry = 12 + i * 16;
            out[entry + 4..entry + 8].copy_from_slice(&sum.to_be_bytes());
            out[entry + 8..entry + 12].copy_from_slice(&u32::try_from(start).ok()?.to_be_bytes());
            out[entry + 12..entry + 16].copy_from_slice(&length.to_be_bytes());
        }
        let start = adjustment?;
        let sum = 0xb1b0_afbau32.wrapping_sub(checksum(&out));
        out[start..start + 4].copy_from_slice(&sum.to_be_bytes());
        (out.len() < self.face.data.len()).then_some(out)
    }
}

fn checksum(padded: &[u8]) -> u32 {
    padded.chunks_exact(4).map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]])).fold(0, u32::wrapping_add)
}

// Include aliases for shaped glyphs as well, e.g. a precomposed syllable produced from Hangul Jamo.
fn unicode_map(face: &ttf_parser::Face<'_>, used: &BTreeSet<u16>) -> Option<Vec<u8>> {
    let mut mappings = BTreeMap::new();
    for sub in face.tables().cmap?.subtables.into_iter().filter(|s| s.is_unicode()) {
        if matches!(sub.format, ttf_parser::cmap::Format::UnicodeVariationSequences(_)) {
            return None;
        }
        sub.codepoints(|cp| {
            if let Some(id) = sub.glyph_index(cp).filter(|id| used.contains(&id.0)) {
                mappings.entry(cp).or_insert(u32::from(id.0));
            }
        });
    }
    // One Windows full-Unicode format 12 subtable. Consecutive codes with consecutive IDs form a group.
    let mut groups: Vec<(u32, u32, u32)> = Vec::new();
    for (cp, id) in mappings {
        if let Some((first, last, start_id)) = groups.last_mut() {
            if cp == *last + 1 && id == *start_id + cp - *first {
                *last = cp;
                continue;
            }
        }
        groups.push((cp, cp, id));
    }
    let n = groups.len() as u32;
    let mut cmap = vec![0, 0, 0, 1, 0, 3, 0, 10, 0, 0, 0, 12, 0, 12, 0, 0];
    for value in [16 + 12 * n, 0, n] { cmap.extend_from_slice(&value.to_be_bytes()); }
    for (first, last, id) in groups {
        for value in [first, last, id] { cmap.extend_from_slice(&value.to_be_bytes()); }
    }
    Some(cmap)
}

// Readers may select different OpenType features. Retain substitution outputs even when the
// renderer selected a different alternate. Contextual lookups dispatch to these same lookups.
fn keep_substitutions(face: &ttf_parser::Face<'_>, keep: &mut BTreeSet<u16>) {
    use gsub::{SingleSubstitution, SubstitutionSubtable as Sub};
    let Some(gsub) = face.tables().gsub else { return };
    for lookup in gsub.lookups {
        for sub in lookup.subtables.into_iter::<Sub>() {
            match sub {
                Sub::Single(SingleSubstitution::Format1 { coverage, delta }) => {
                    let mut add = |id: u16| { keep.insert(id.wrapping_add_signed(delta)); };
                    match coverage {
                        Coverage::Format1 { glyphs } => glyphs.into_iter().for_each(|g| add(g.0)),
                        Coverage::Format2 { records } => {
                            for r in records { (r.start.0..=r.end.0).for_each(&mut add); }
                        }
                    }
                }
                Sub::Single(SingleSubstitution::Format2 { substitutes, .. }) => keep.extend(substitutes.into_iter().map(|g| g.0)),
                Sub::ReverseChainSingle(s) => keep.extend(s.substitutes.into_iter().map(|g| g.0)),
                Sub::Multiple(s) => for seq in s.sequences { keep.extend(seq.substitutes.into_iter().map(|g| g.0)); },
                Sub::Alternate(s) => for set in s.alternate_sets { keep.extend(set.alternates.into_iter().map(|g| g.0)); },
                Sub::Ligature(s) => for set in s.ligature_sets { keep.extend(set.into_iter().map(|lig| lig.glyph.0)); },
                Sub::Context(_) | Sub::ChainContext(_) => {}
            }
        }
    }
}

fn for_each_component(data: &[u8], mut visit: impl FnMut(u16)) -> Option<()> {
    let word = |offset: usize| Some(u16::from_be_bytes(data.get(offset..offset + 2)?.try_into().ok()?));
    if (word(0)? as i16) >= 0 { return Some(()); }
    let mut pos = 10; // after the contour count and bounds
    loop {
        let flags = word(pos)?;
        visit(word(pos + 2)?);
        if flags & 0x0020 == 0 { return Some(()); }
        let arguments = if flags & 0x0001 != 0 { 4 } else { 2 };
        let transform = if flags & 0x0080 != 0 { 8 } else if flags & 0x0040 != 0 { 4 } else if flags & 0x0008 != 0 { 2 } else { 0 };
        pos += 4 + arguments + transform;
    }
}
