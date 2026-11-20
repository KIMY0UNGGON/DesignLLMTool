//! Single-line shaping with per-grapheme font fallback (rustybuzz), integer EMU output.
use crate::encode::GlyphSource;
use crate::fonts::{Candidate, FontBook};
use designllm_contracts::*;
use unicode_segmentation::UnicodeSegmentation;

/// a*b/c rounded half away from zero, exact integer arithmetic.
pub fn mul_div_round(a: i64, b: i64, c: i64) -> i64 {
    if c == 0 { return 0; }
    let n = i128::from(a) * i128::from(b);
    let c = i128::from(c);
    let q = n / c;
    let r = n % c;
    let adj = if 2 * r.abs() >= c.abs() { n.signum() * c.signum() } else { 0 };
    (q + adj) as i64
}

/// Graphemes drawn without glyphs and with zero advance (layout handles tabs and breaks).
pub fn is_control_grapheme(g: &str) -> bool {
    g.chars().all(|c| c.is_control() || matches!(c, '\u{2028}' | '\u{2029}'))
}

struct Seg { start: usize, end: usize, face: Option<usize>, graphemes: std::ops::Range<usize> }

pub(crate) fn shape(book: &FontBook, text: &str, style: &Style) -> DltResult<ShapedText> {
    let size = style.font_size;
    if size <= 0 || size > 10_000 * EMU_PER_PT {
        return Err(DltError::invalid(format!("Font size {size} EMU is outside 1..=10000 pt")));
    }
    let weight = crate::requested_font_weight(style);
    let requested = book.resolve(&style.font_family, weight, style.italic);
    let mut chain: Vec<Candidate> = Vec::new();
    if let Some(c) = requested { chain.push(c); }
    for c in book.fallbacks(weight, style.italic) { if !chain.contains(&c) { chain.push(c); } }
    let mut loaded: Vec<Option<Option<usize>>> = vec![None; chain.len()];
    let mut face_at = |k: usize, book: &FontBook| -> Option<usize> {
        if loaded[k].is_none() { loaded[k] = Some(book.load(chain[k])); }
        loaded[k].unwrap()
    };
    let primary = (0..chain.len()).find_map(|k| face_at(k, book)).unwrap_or(book.regular);

    let graphemes: Vec<(usize, &str)> = text.grapheme_indices(true).collect();
    let mut missing: Vec<String> = Vec::new();
    let mut assigned: Vec<Option<usize>> = Vec::with_capacity(graphemes.len());
    for (_, g) in &graphemes {
        if is_control_grapheme(g) { assigned.push(None); continue; }
        let mut found = None;
        for k in 0..chain.len() {
            if let Some(id) = face_at(k, book) {
                if book.face(id).covers(g) { found = Some(id); break; }
            }
        }
        let face = match found {
            Some(id) => id,
            None => {
                missing.push(g.to_string());
                let base = g.chars().next().unwrap_or(' ');
                (0..chain.len()).filter_map(|k| face_at(k, book)).find(|id| book.face(*id).has_char(base)).unwrap_or(primary)
            }
        };
        assigned.push(Some(face));
    }
    // Segments of consecutive graphemes with the same face (controls are their own segments).
    let mut segs: Vec<Seg> = Vec::new();
    for (i, (start, g)) in graphemes.iter().enumerate() {
        let end = start + g.len();
        match segs.last_mut() {
            Some(last) if last.face.is_some() && last.face == assigned[i] && last.end == *start => { last.end = end; last.graphemes.end = i + 1; }
            _ => segs.push(Seg { start: *start, end, face: assigned[i], graphemes: i..i + 1 }),
        }
    }
    let n = graphemes.len();
    let starts: Vec<usize> = graphemes.iter().map(|(s, _)| *s).collect();
    let grapheme_of = |byte: usize| -> usize { match starts.binary_search(&byte) { Ok(k) => k, Err(k) => k.saturating_sub(1) } };
    let spaced = style.letter_spacing.is_some_and(|v| v != 0);
    const NO_LIGATURES: [rustybuzz::Feature; 2] = [
        rustybuzz::Feature { tag: rustybuzz::ttf_parser::Tag::from_bytes(b"liga"), value: 0, start: 0, end: u32::MAX },
        rustybuzz::Feature { tag: rustybuzz::ttf_parser::Tag::from_bytes(b"clig"), value: 0, start: 0, end: u32::MAX },
    ];
    let mut glyphs: Vec<GlyphMetric> = Vec::new();
    let mut caret = vec![0i64; n + 1];
    let mut used: Vec<usize> = Vec::new();
    let mut pen = 0i64;
    let primary_face = book.face(primary);
    let scale = |v: i64, upem: u16| mul_div_round(v, size, i64::from(upem));
    let mut ascent = scale(i64::from(primary_face.ascender), primary_face.units_per_em);
    let mut descent = scale(-i64::from(primary_face.descender), primary_face.units_per_em);
    let mut line_gap = scale(i64::from(primary_face.line_gap), primary_face.units_per_em);
    let cap_height = scale(i64::from(primary_face.cap_height), primary_face.units_per_em);
    for seg in &segs {
        let Some(face_id) = seg.face else {
            for k in seg.graphemes.clone() { caret[k] = pen; }
            continue;
        };
        let face = book.face(face_id);
        if !used.contains(&face_id) { used.push(face_id); }
        let upem = face.units_per_em;
        ascent = ascent.max(scale(i64::from(face.ascender), upem));
        descent = descent.max(scale(-i64::from(face.descender), upem));
        line_gap = line_gap.max(scale(i64::from(face.line_gap), upem));
        let mut buffer = rustybuzz::UnicodeBuffer::new();
        buffer.push_str(&text[seg.start..seg.end]);
        buffer.set_direction(rustybuzz::Direction::LeftToRight);
        buffer.guess_segment_properties();
        // CSS Text 3: no optional ligatures when letter spacing is not zero (as Chromium: liga, clig off).
        let out = rustybuzz::shape(&face.hb, if spaced { &NO_LIGATURES[..] } else { &[] }, buffer);
        let mut pen_units = 0i64;
        let first = glyphs.len();
        let mut pen_xs: Vec<i64> = Vec::new();
        for (info, pos) in out.glyph_infos().iter().zip(out.glyph_positions()) {
            let pen_x = pen + scale(pen_units, upem);
            let cluster = seg.start + info.cluster as usize;
            let gs = grapheme_of(cluster) as u32;
            glyphs.push(GlyphMetric {
                glyph_id: info.glyph_id,
                font_key: face.key.clone(),
                position: Point { x: pen + scale(pen_units + i64::from(pos.x_offset), upem), y: -scale(i64::from(pos.y_offset), upem) },
                advance_x: 0,
                grapheme_start: gs,
                grapheme_end: gs + 1,
            });
            pen_xs.push(pen_x);
            pen_units += i64::from(pos.x_advance);
        }
        let seg_end = pen + scale(pen_units, upem);
        for k in first..glyphs.len() {
            let next = if k + 1 < glyphs.len() { pen_xs[k + 1 - first] } else { seg_end };
            glyphs[k].advance_x = next - pen_xs[k - first];
        }
        // Clusters: [cluster start grapheme, next cluster start grapheme or segment end).
        let mut cluster_starts: Vec<u32> = glyphs[first..].iter().map(|g| g.grapheme_start).collect();
        cluster_starts.sort_unstable();
        cluster_starts.dedup();
        for (ci, &gs) in cluster_starts.iter().enumerate() {
            let ge = cluster_starts.get(ci + 1).copied().unwrap_or(seg.graphemes.end as u32);
            let members: Vec<usize> = (first..glyphs.len()).filter(|&k| glyphs[k].grapheme_start == gs).collect();
            let x0 = members.iter().map(|&k| pen_xs[k - first]).min().unwrap_or(pen);
            let adv: i64 = members.iter().map(|&k| glyphs[k].advance_x).sum();
            for &k in &members { glyphs[k].grapheme_end = ge; }
            let span = i64::from(ge - gs).max(1);
            for g in gs..ge { caret[g as usize] = x0 + mul_div_round(adv, i64::from(g - gs), span); }
        }
        // Graphemes of the segment without any glyph keep the running pen.
        let covered: std::collections::BTreeSet<u32> = cluster_starts.iter().copied().collect();
        if cluster_starts.is_empty() || cluster_starts[0] as usize > seg.graphemes.start {
            let limit = cluster_starts.first().copied().unwrap_or(seg.graphemes.end as u32);
            for g in seg.graphemes.start as u32..limit { if !covered.contains(&g) { caret[g as usize] = pen; } }
        }
        pen = seg_end;
    }
    caret[n] = pen;
    // v1.2 letter spacing: after every grapheme (CSS), not after tabs and line breaks.
    if let Some(ls) = style.letter_spacing.filter(|v| *v != 0) {
        let mut shift = vec![0i64; n + 1];
        for g in 0..n { shift[g + 1] = shift[g] + if is_control_grapheme(graphemes[g].1) { 0 } else { ls }; }
        for (k, g) in caret.iter_mut().enumerate() { *g += shift[k]; }
        let count = glyphs.len();
        for i in 0..count {
            let (gs, ge) = (glyphs[i].grapheme_start as usize, glyphs[i].grapheme_end as usize);
            glyphs[i].position.x += shift[gs.min(n)];
            let last_of_cluster = i + 1 == count || glyphs[i + 1].grapheme_start != glyphs[i].grapheme_start;
            if last_of_cluster { glyphs[i].advance_x += shift[ge.min(n)] - shift[gs.min(n)]; }
        }
        pen = caret[n];
    }
    let grapheme_rects: Vec<Rect> = caret.windows(2).map(|w| Rect { x: w[0], y: -ascent, width: w[1] - w[0], height: ascent + descent }).collect();
    let fonts_used: Vec<FontFaceRef> = used.iter().map(|id| book.face(*id).face_ref()).collect();
    // v1.2 line height: the line box is font size x line_height_milli / 1000, half the leading above.
    let (height, baseline, line_gap) = match style.line_height_milli {
        Some(m) => { let line = mul_div_round(style.font_size, i64::from(m), 1000); let lead = line - ascent - descent; (line, ascent + lead.div_euclid(2), lead.max(0)) }
        None => (ascent + descent + line_gap, line_gap / 2 + ascent, line_gap),
    };
    Ok(ShapedText {
        size: Size { width: pen, height },
        baseline,
        ascent,
        descent,
        line_gap,
        cap_height,
        glyphs,
        caret_x: caret,
        grapheme_rects,
        fonts_used,
        missing_glyphs: missing,
    })
}

/// Top offset of text `content_height` high in a box `box_height` high for v1.2 `vertical_align`
/// (None and top: 0, middle: half the free space rounded down, bottom: all of it; negative when the
/// text overflows). One rule for every layout, so engines and HTML place text boxes alike.
/// Font metrics make decoration strokes identical in PNG, SVG and PDF.
pub(crate) fn text_decorations(
    glyphs: &GlyphSource<'_>,
    origin: Point,
    style: &Style,
    shaped: &ShapedText,
) -> DltResult<Vec<(Point, Point, i64)>> {
    if shaped.size.width <= 0 {
        return Ok(Vec::new());
    }
    let face = shaped.fonts_used.first().map(|font| glyphs.face(shaped, &font.key)).transpose()?;
    let scale = |metric: i16| {
        face.as_ref()
            .map(|face| mul_div_round(i64::from(metric), style.font_size, i64::from(face.units_per_em)))
    };
    let thickness = (style.font_size / 20).max(1);
    let end_x = origin.x.saturating_add(shaped.size.width);
    let mut lines = Vec::with_capacity(2);
    if style.underline == Some(true) {
        let metrics = face.as_ref().and_then(|face| face.ttf.underline_metrics());
        let (offset, width) = metrics
            .and_then(|metrics| Some((scale(metrics.position)?, scale(metrics.thickness)?.abs().max(1))))
            .unwrap_or((shaped.descent / 2, thickness));
        let y = origin.y.saturating_sub(offset);
        lines.push((Point { x: origin.x, y }, Point { x: end_x, y }, width));
    }
    if style.strikethrough == Some(true) {
        let metrics = face.as_ref().and_then(|face| face.ttf.strikeout_metrics());
        let (offset, width) = metrics
            .and_then(|metrics| Some((scale(metrics.position)?, scale(metrics.thickness)?.abs().max(1))))
            .unwrap_or((-shaped.cap_height / 2, thickness));
        let y = origin.y.saturating_sub(offset);
        lines.push((Point { x: origin.x, y }, Point { x: end_x, y }, width));
    }
    Ok(lines)
}

pub fn text_block_offset(box_height: i64, content_height: i64, align: Option<VerticalAlign>) -> i64 {
    let free = box_height - content_height;
    match align { Some(VerticalAlign::Middle) => free.div_euclid(2), Some(VerticalAlign::Bottom) => free, Some(VerticalAlign::Top) | None => 0 }
}
