//! Pure page queries in page EMU (core validates snapshot/transform and converts points first).
//!
//! Inputs are a RenderPage's primitives and source_map only (no hidden renderer state).
//! Text regions are SourceRegions with `logical_range`; their `grapheme_rects` hold one absolute
//! rect per grapheme of the range. If an engine omits grapheme_rects, they are derived from the
//! Text primitive with the same node_id and bounds (or spread evenly as a last resort).
//!
//! TextPosition: `grapheme_index` is a caret boundary (0..=len); `trailing = true` means the
//! caret sticks to the end of the preceding grapheme (end of a visual line at a wrap boundary).
use designllm_contracts::*;

struct TextRegion<'a> { region: &'a SourceRegion, range: &'a TextRange, rects: Vec<Rect> }

fn text_regions(page: &RenderPage) -> Vec<TextRegion<'_>> {
    page.source_map.iter().filter_map(|region| {
        let range = region.logical_range.as_ref()?;
        Some(TextRegion { region, range, rects: region_rects(page, region, range) })
    }).collect()
}

fn region_rects(page: &RenderPage, region: &SourceRegion, range: &TextRange) -> Vec<Rect> {
    let n = range.end.saturating_sub(range.start) as usize;
    if region.grapheme_rects.len() == n { return region.grapheme_rects.clone(); }
    for primitive in &page.primitives {
        if let Primitive::Text { node_id, bounds, origin, text, shaped, .. } = primitive {
            if node_id == &region.node_id && *bounds == region.bounds && unicode_count(text) == n && shaped.grapheme_rects.len() == n {
                return shaped.grapheme_rects.iter().map(|r| Rect { x: origin.x + r.x, y: region.bounds.y, width: r.width, height: region.bounds.height }).collect();
            }
        }
    }
    let b = region.bounds;
    (0..n).map(|i| {
        let x0 = b.x + b.width * i as i64 / n.max(1) as i64;
        let x1 = b.x + b.width * (i as i64 + 1) / n.max(1) as i64;
        Rect { x: x0, y: b.y, width: x1 - x0, height: b.height }
    }).collect()
}

fn unicode_count(text: &str) -> usize {
    use unicode_segmentation::UnicodeSegmentation;
    text.graphemes(true).count()
}

fn vertical_distance(r: Rect, y: i64) -> i64 {
    if y < r.y { r.y - y } else if y > r.y + r.height { y - (r.y + r.height) } else { 0 }
}

fn horizontal_distance(r: Rect, x: i64) -> i64 {
    if x < r.x { r.x - x } else if x > r.x + r.width { x - (r.x + r.width) } else { 0 }
}

fn position_in(t: &TextRegion<'_>, x: i64) -> TextPosition {
    let start = t.range.start;
    for (i, r) in t.rects.iter().enumerate() {
        if x < r.x + r.width / 2 {
            return TextPosition { paragraph_id: t.range.paragraph_id.clone(), grapheme_index: start + i as u32, trailing: false };
        }
    }
    let end = t.range.end;
    TextPosition { paragraph_id: t.range.paragraph_id.clone(), grapheme_index: end, trailing: end > start }
}

/// Node and logical text position at a page point.
pub fn hit_test(page: &RenderPage, point: Point) -> DltResult<Option<HitTestResult>> {
    let regions = text_regions(page);
    // 1. A text line box containing the point.
    if let Some(t) = regions.iter().rev().find(|t| t.region.bounds.contains(point)) {
        return Ok(Some(HitTestResult { node_id: t.region.node_id.clone(), position: Some(position_in(t, point.x)) }));
    }
    // 2. Same visual line of a paragraph whose block contains the point, or the nearest line.
    let paragraph_blocks: Vec<&SourceRegion> = page.source_map.iter()
        .filter(|r| r.logical_range.is_none() && r.bounds.contains(point) && regions.iter().any(|t| t.range.paragraph_id == r.node_id))
        .collect();
    if let Some(block) = paragraph_blocks.iter().min_by_key(|r| i128::from(r.bounds.width) * i128::from(r.bounds.height)) {
        let lines: Vec<&TextRegion> = regions.iter().filter(|t| t.range.paragraph_id == block.node_id).collect();
        if let Some(best) = lines.iter().min_by_key(|t| (vertical_distance(t.region.bounds, point.y), horizontal_distance(t.region.bounds, point.x))) {
            return Ok(Some(HitTestResult { node_id: best.region.node_id.clone(), position: Some(position_in(best, point.x)) }));
        }
    }
    // 3. The smallest non-text region containing the point.
    let hit = page.source_map.iter()
        .filter(|r| r.logical_range.is_none() && r.bounds.contains(point))
        .min_by_key(|r| i128::from(r.bounds.width) * i128::from(r.bounds.height));
    if let Some(r) = hit { return Ok(Some(HitTestResult { node_id: r.node_id.clone(), position: None })); }
    // 4. Primitives without regions (engines that only emit primitives).
    for primitive in page.primitives.iter().rev() {
        let (id, bounds) = match primitive {
            Primitive::Text { node_id, bounds, .. } | Primitive::Shape { node_id, bounds, .. } | Primitive::Image { node_id, bounds, .. } | Primitive::Path { node_id, bounds, .. } => (node_id, *bounds),
            Primitive::Line { node_id, from, to, style, .. } => {
                let pad = style.stroke_width.max(EMU_PER_PT * 2);
                (node_id, Rect { x: from.x.min(to.x) - pad, y: from.y.min(to.y) - pad, width: (from.x - to.x).abs() + 2 * pad, height: (from.y - to.y).abs() + 2 * pad })
            }
        };
        if bounds.contains(point) { return Ok(Some(HitTestResult { node_id: id.clone(), position: None })); }
    }
    Ok(None)
}

fn paragraph_regions<'a>(regions: &'a [TextRegion<'a>], paragraph_id: &str) -> DltResult<Vec<&'a TextRegion<'a>>> {
    let found: Vec<&TextRegion> = regions.iter().filter(|t| t.range.paragraph_id == paragraph_id).collect();
    if found.is_empty() {
        let mut e = DltError::new(ErrorCode::NotFound, format!("Paragraph {paragraph_id} has no text on this page"));
        e.affected_ids = vec![paragraph_id.to_string()];
        return Err(e);
    }
    Ok(found)
}

/// Caret rectangle (zero width, line height) for a text position on this page.
pub fn caret_rect(page: &RenderPage, position: &TextPosition) -> DltResult<Rect> {
    let regions = text_regions(page);
    let lines = paragraph_regions(&regions, &position.paragraph_id)?;
    let idx = position.grapheme_index;
    let containing: Vec<&&TextRegion> = lines.iter().filter(|t| t.range.start <= idx && idx <= t.range.end).collect();
    let chosen = if position.trailing {
        containing.iter().find(|t| t.range.end == idx && t.range.end > t.range.start).or_else(|| containing.first())
    } else {
        containing.iter().find(|t| idx < t.range.end || t.range.start == t.range.end).or_else(|| containing.iter().find(|t| t.range.start == idx)).or_else(|| containing.last())
    };
    let Some(t) = chosen else {
        return Err(DltError::invalid(format!("Grapheme position {idx} of paragraph {} is not on this page", position.paragraph_id)));
    };
    let x = if idx < t.range.end {
        t.rects[(idx - t.range.start) as usize].x
    } else if let Some(last) = t.rects.last() {
        last.x + last.width
    } else {
        t.region.bounds.x
    };
    Ok(Rect { x, y: t.region.bounds.y, width: 0, height: t.region.bounds.height })
}

/// Selection rectangles of a range on this page, one per visual line.
pub fn selection_rects(page: &RenderPage, range: &TextRange) -> DltResult<Vec<Rect>> {
    if range.start > range.end { return Err(DltError::invalid("Selection start is after its end")); }
    let regions = text_regions(page);
    let lines = paragraph_regions(&regions, &range.paragraph_id)?;
    let mut out: Vec<Rect> = Vec::new();
    for t in lines {
        let a = range.start.max(t.range.start);
        let b = range.end.min(t.range.end);
        if a >= b { continue; }
        let r0 = t.rects[(a - t.range.start) as usize];
        let r1 = t.rects[(b - 1 - t.range.start) as usize];
        let rect = Rect { x: r0.x, y: t.region.bounds.y, width: r1.x + r1.width - r0.x, height: t.region.bounds.height };
        match out.last_mut() {
            Some(last) if last.y == rect.y && last.height == rect.height && rect.x <= last.x + last.width && rect.x >= last.x => {
                let right = (last.x + last.width).max(rect.x + rect.width);
                last.width = right - last.x;
            }
            _ => out.push(rect),
        }
    }
    Ok(out)
}
