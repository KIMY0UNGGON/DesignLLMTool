//! Region rendering (P1_PLAN `RenderRequest.region`): a part of a page as a page of its own, so the
//! encoders draw only what the board shows. Primitives move by the region origin (exact integer
//! EMU) and primitives whose ink cannot reach the region are dropped. The PNG encoder does not move
//! primitives: it draws page pixels through an integer translation (see `Renderer::encode_region`).
use designllm_contracts::*;

/// Conservative ink box of a primitive in page EMU (strokes, caps, miters, arrow heads, glyph
/// overhang and shadows included).
fn ink(p: &Primitive) -> Rect {
    let grow = |r: Rect, d: i64| Rect { x: r.x - d, y: r.y - d, width: r.width + 2 * d, height: r.height + 2 * d };
    let union = |a: Rect, b: Rect| { let (x, y) = (a.x.min(b.x), a.y.min(b.y)); Rect { x, y, width: (a.x + a.width).max(b.x + b.width) - x, height: (a.y + a.height).max(b.y + b.height) - y } };
    let with_shadows = |r: Rect, style: &Style| style.shadows.iter().fold(r, |acc, s| union(acc, grow(Rect { x: r.x + s.x, y: r.y + s.y, ..r }, s.spread.max(0) + (s.blur.max(0) * 3).div_euclid(2) + 1)));
    match p {
        Primitive::Text { bounds, style, .. } => grow(*bounds, style.font_size.max(0) + 1),
        // Miter joins reach 4 half widths (miter limit 4).
        Primitive::Shape { bounds, style, .. } => with_shadows(grow(*bounds, 2 * style.stroke_width.max(0) + 1), style),
        Primitive::Image { bounds, style, .. } => match style { Some(s) => with_shadows(grow(*bounds, 1), s), None => grow(*bounds, 1) },
        Primitive::Line { from, to, style, .. } => {
            let head = (style.stroke_width.max(EMU_PER_PT) * 4).max(4 * EMU_PER_PT);
            let r = Rect { x: from.x.min(to.x), y: from.y.min(to.y), width: (from.x - to.x).abs(), height: (from.y - to.y).abs() };
            grow(r, head + 2 * style.stroke_width.max(1) + 1)
        }
        Primitive::Path { bounds, path, style, .. } => {
            // Control points bound the curve; commands may leave the view box.
            let (sx, sy, ox, oy) = crate::effects::path_transform(path, *bounds);
            let pts = path.commands.iter().flat_map(|c| match *c {
                PathCommand::MoveTo { x, y } | PathCommand::LineTo { x, y } => vec![(x, y)],
                PathCommand::CubicTo { x1, y1, x2, y2, x, y } => vec![(x1, y1), (x2, y2), (x, y)],
                PathCommand::Close => vec![],
            });
            let hull = pts.fold(*bounds, |acc, (x, y)| {
                let (px, py) = ((ox + x as f64 * sx).floor() as i64, (oy + y as f64 * sy).floor() as i64);
                union(acc, Rect { x: px, y: py, width: 1, height: 1 })
            });
            with_shadows(grow(hull, 2 * style.stroke_width.max(0) + 1), style)
        }
    }
}

/// INVALID_ARGUMENT unless `region` has a positive size and lies inside the page.
pub(crate) fn check(page: &RenderPage, region: Rect) -> DltResult<()> {
    if region.width <= 0 || region.height <= 0 { return Err(DltError::invalid("render region must have a positive width and height")); }
    if region.x < 0 || region.y < 0 || region.x + region.width > page.size.width || region.y + region.height > page.size.height {
        return Err(DltError::invalid(format!("render region {},{} {}x{} is not inside the page ({}x{} EMU)", region.x, region.y, region.width, region.height, page.size.width, page.size.height)));
    }
    Ok(())
}

/// Whether `p` can draw into `area` (page EMU).
pub(crate) fn reaches(p: &Primitive, area: Rect) -> bool { overlaps(ink(p), area) }

fn overlaps(a: Rect, b: Rect) -> bool { a.x < b.x + b.width && b.x < a.x + a.width && a.y < b.y + b.height && b.y < a.y + a.height }

/// `region` (page EMU) of `page` as a page `region` in size: primitives and source regions moved by
/// −(region.x, region.y), primitives that cannot draw into the region dropped. INVALID_ARGUMENT
/// unless the region has a positive size and lies inside the page.
pub fn page_region(page: &RenderPage, region: Rect) -> DltResult<RenderPage> {
    check(page, region)?;
    let (dx, dy) = (region.x, region.y);
    let mv = |r: Rect| Rect { x: r.x - dx, y: r.y - dy, ..r };
    let pt = |p: Point| Point { x: p.x - dx, y: p.y - dy };
    let primitives = page.primitives.iter().filter(|p| overlaps(ink(p), region)).map(|p| match p.clone() {
        Primitive::Text { node_id, bounds, origin, text, style, shaped } => Primitive::Text { node_id, bounds: mv(bounds), origin: pt(origin), text, style, shaped },
        Primitive::Shape { node_id, bounds, shape, style } => Primitive::Shape { node_id, bounds: mv(bounds), shape, style },
        Primitive::Line { node_id, from, to, style, arrow } => Primitive::Line { node_id, from: pt(from), to: pt(to), style, arrow },
        Primitive::Image { node_id, bounds, asset, fit, image_crop_pixel_rect, style } => Primitive::Image { node_id, bounds: mv(bounds), asset, fit, image_crop_pixel_rect, style },
        Primitive::Path { node_id, bounds, path, style } => Primitive::Path { node_id, bounds: mv(bounds), path, style },
    }).collect();
    let source_map = page.source_map.iter().filter(|s| overlaps(s.bounds, region)).map(|s| SourceRegion { bounds: mv(s.bounds), grapheme_rects: s.grapheme_rects.iter().map(|r| mv(*r)).collect(), ..s.clone() }).collect();
    Ok(RenderPage { id: page.id.clone(), page_index: page.page_index, size: Size { width: region.width, height: region.height }, primitives, source_map })
}
