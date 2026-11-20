//! PNG encoder (tiny-skia). Size follows the contract half-up `pixels()` rule.
use crate::effects::{self, faded};
use crate::encode::{Diags, GlyphSource, ShapeKind};
use crate::paint::{crop_decoded_image, crop_diagnostic_code, decode, parse_color, sniff, validate_jpeg_crop_orientation, ImageKind, Rgba};
use designllm_contracts::*;
use tiny_skia::{FillRule, Mask, Paint, PathBuilder, Pixmap, PixmapPaint, Stroke, Transform};

pub const MAX_RASTER_PIXELS: u64 = 400_000_000;
const OBLIQUE: f32 = 0.2126; // tan(12 degrees)

struct Outline(PathBuilder);
impl ttf_parser::OutlineBuilder for Outline {
    fn move_to(&mut self, x: f32, y: f32) { self.0.move_to(x, y); }
    fn line_to(&mut self, x: f32, y: f32) { self.0.line_to(x, y); }
    fn quad_to(&mut self, x1: f32, y1: f32, x: f32, y: f32) { self.0.quad_to(x1, y1, x, y); }
    fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x: f32, y: f32) { self.0.cubic_to(x1, y1, x2, y2, x, y); }
    fn close(&mut self) { self.0.close(); }
}

fn paint(c: Rgba) -> Paint<'static> {
    let mut p = Paint::default();
    p.set_color_rgba8(c.r, c.g, c.b, c.a);
    p.anti_alias = true;
    p
}

pub(crate) fn encode_png(glyphs: &GlyphSource<'_>, page: &RenderPage, profile: &RenderProfile, region: Option<Rect>, assets: &dyn AssetStore, diags: &mut Diags) -> DltResult<Vec<u8>> {
    // A region is drawn in page pixels moved by its origin: on whole pixels this is exact (integer
    // translations of the same coordinates), so the region equals the crop of the whole page.
    let area = region.unwrap_or(Rect { x: 0, y: 0, width: page.size.width, height: page.size.height });
    let w = pixels(area.width, profile.dpi)?;
    let h = pixels(area.height, profile.dpi)?;
    if u64::from(w) * u64::from(h) > MAX_RASTER_PIXELS {
        return Err(DltError::invalid(format!("Raster {w}x{h} exceeds {MAX_RASTER_PIXELS} pixels; lower the DPI")));
    }
    let mut pm = Pixmap::new(w, h).ok_or_else(|| DltError::invalid("Cannot allocate raster"))?;
    if !profile.transparent { pm.fill(tiny_skia::Color::WHITE); }
    let k = profile.dpi as f32 / EMU_PER_INCH as f32;
    let px = |v: i64| v as f32 * k;
    let origin = |v: i64| { let n = i128::from(v) * i128::from(profile.dpi); if n % i128::from(EMU_PER_INCH) == 0 { (n / i128::from(EMU_PER_INCH)) as f32 } else { px(v) } };
    let base = if region.is_some() { Transform::from_translate(-origin(area.x), -origin(area.y)) } else { Transform::identity() };
    let moved = |t: Transform| if region.is_some() { t.post_concat(base) } else { t };
    for primitive in &page.primitives {
        if region.is_some() && !crate::region::reaches(primitive, area) { continue; }
        match primitive {
            Primitive::Path { node_id, bounds, path, style } => draw_path(&mut pm, node_id, *bounds, path, style, k, base, diags),
            // v1.2 effects take their own branch; v1.1 styles keep the exact v1.1 drawing below.
            Primitive::Shape { node_id, bounds, shape, style } if crate::v12::has_effects(style) => {
                let kind = ShapeKind::parse(shape);
                if kind == ShapeKind::Unknown { diags.add("shape_approximated", format!("Shape '{shape}' is drawn as its bounding rectangle"), Some(node_id), false); }
                draw_shape_v12(&mut pm, node_id, *bounds, kind, style, k, base, diags);
            }
            Primitive::Line { node_id, from, to, style, arrow } if crate::v12::has_effects(style) => draw_line_v12(&mut pm, node_id, *from, *to, style, *arrow, k, base, diags),
            Primitive::Shape { node_id, bounds, shape, style } => {
                let kind = ShapeKind::parse(shape);
                if kind == ShapeKind::Unknown { diags.add("shape_approximated", format!("Shape '{shape}' is drawn as its bounding rectangle"), Some(node_id), false); }
                let Some(path) = shape_path(kind, px(bounds.x), px(bounds.y), px(bounds.width), px(bounds.height)) else { continue };
                if let Some(fill) = diags.color(&style.fill, node_id) { pm.fill_path(&path, &paint(fill), FillRule::Winding, base, None); }
                if style.stroke_width > 0 {
                    if let Some(stroke) = diags.color(&style.stroke, node_id) {
                        let s = Stroke { width: px(style.stroke_width).max(0.1), ..Stroke::default() };
                        pm.stroke_path(&path, &paint(stroke), &s, base, None);
                    }
                }
            }
            Primitive::Line { node_id, from, to, style, arrow } => {
                let Some(color) = diags.color(&style.stroke, node_id) else { continue };
                let width = px(style.stroke_width.max(1)).max(0.1);
                let mut pb = PathBuilder::new();
                pb.move_to(px(from.x), px(from.y));
                pb.line_to(px(to.x), px(to.y));
                if let Some(path) = pb.finish() {
                    pm.stroke_path(&path, &paint(color), &Stroke { width, ..Stroke::default() }, base, None);
                }
                if *arrow {
                    if let Some(head) = crate::encode::arrow_head(*from, *to, style.stroke_width) {
                        let mut pb = PathBuilder::new();
                        pb.move_to(px(head[0].x), px(head[0].y));
                        pb.line_to(px(head[1].x), px(head[1].y));
                        pb.line_to(px(head[2].x), px(head[2].y));
                        pb.close();
                        if let Some(path) = pb.finish() { pm.fill_path(&path, &paint(color), FillRule::Winding, base, None); }
                    }
                }
            }
            Primitive::Image { node_id, bounds, asset, fit, image_crop_pixel_rect, style } => {
                // P1: fit, opacity, corner radius (as clip) and the box's outer shadows; without
                // them the v1.1 drawing (stretched to bounds) is unchanged.
                let look = fit.is_some() || image_crop_pixel_rect.is_some() || style.is_some();
                let op = style.as_ref().map(effects::opacity).unwrap_or(1.0);
                let radius = style.as_ref().map(|s| effects::corner_radius(s, *bounds, false)).unwrap_or(0);
                if let Some(s) = style { shadows_v12(&mut pm, s, k, op, base, &|dx, dy, out| box_path(ShapeKind::Rect, *bounds, radius, k, dx, dy, out), node_id, diags); }
                let clip = if radius > 0 || *fit == Some(ImageFit::Cover) {
                    box_path(ShapeKind::Rect, *bounds, radius, k, 0.0, 0.0, 0.0).and_then(|p| { let mut m = Mask::new(pm.width(), pm.height())?; m.fill_path(&p, FillRule::Winding, true, base); Some(m) })
                } else { None };
                match assets.read(asset).and_then(|bytes| {
                    if image_crop_pixel_rect.is_some() && sniff(&bytes) == ImageKind::Jpeg {
                        validate_jpeg_crop_orientation(&bytes)?;
                    }
                    let decoded = decode(&bytes)?;
                    match image_crop_pixel_rect {
                        Some(crop) => {
                            effects::image_crop_placement(*bounds, (decoded.width, decoded.height), *crop, *fit)?;
                            crop_decoded_image(decoded, *crop)
                        }
                        None => Ok(decoded),
                    }
                }) {
                    Ok(img) if img.width > 0 && img.height > 0 => {
                        for w in &img.warnings { diags.add("image_checksum_mismatch", format!("Image {node_id} is damaged and was decoded in degraded mode: {w}"), Some(node_id), true); }
                        let (width, height) = (img.width, img.height);
                        let mut data = img.rgba;
                        for p in data.chunks_exact_mut(4) {
                            let a = u16::from(p[3]);
                            for c in &mut p[..3] { *c = ((u16::from(*c) * a + 127) / 255) as u8; }
                        }
                        let Some(src) = Pixmap::from_vec(data, tiny_skia::IntSize::from_wh(width, height).ok_or_else(|| DltError::invalid("Bad image size"))?) else { continue };
                        let dest = fit.map(|f| effects::image_fit_rect(*bounds, (width, height), f)).unwrap_or(*bounds);
                        let sx = px(dest.width) / width as f32;
                        let sy = px(dest.height) / height as f32;
                        let t = moved(Transform::from_row(sx, 0.0, 0.0, sy, px(dest.x), px(dest.y)));
                        let quality = PixmapPaint { quality: tiny_skia::FilterQuality::Bicubic, opacity: op, ..PixmapPaint::default() };
                        pm.draw_pixmap(0, 0, src.as_ref(), &quality, t, clip.as_ref());
                    }
                    other => {
                        let error = match other { Err(e) => Some(e), Ok(_) => None };
                        let reason = error.as_ref().map(|error| error.message.as_str()).unwrap_or("empty image");
                        let code = error.as_ref().and_then(crop_diagnostic_code).unwrap_or("image_not_rendered");
                        diags.add(code, format!("Image {node_id} is shown as a placeholder: {reason}"), Some(node_id), true);
                        let grey = Rgba { r: 0xE5, g: 0xE7, b: 0xEB, a: 255 };
                        let path = if look { box_path(ShapeKind::Rect, *bounds, radius, k, 0.0, 0.0, 0.0) } else { shape_path(ShapeKind::Rect, px(bounds.x), px(bounds.y), px(bounds.width), px(bounds.height)) };
                        if let Some(path) = path { pm.fill_path(&path, &paint(faded(grey, op)), FillRule::Winding, base, None); }
                    }
                }
            }
            Primitive::Text { node_id, origin, style, shaped, .. } => {
                let Some(color) = diags.color(&style.color, node_id) else { continue };
                let p = paint(faded(color, effects::opacity(style)));
                for g in &shaped.glyphs {
                    let face = glyphs.face(shaped, &g.font_key)?;
                    let mut ob = Outline(PathBuilder::new());
                    if face.ttf.outline_glyph(ttf_parser::GlyphId(g.glyph_id as u16), &mut ob).is_none() { continue; }
                    let Some(path) = ob.0.finish() else { continue };
                    if face.color_glyphs { diags.add("color_glyphs_monochrome", "Color glyphs are drawn as single-color outlines", None, false); }
                    let s = style.font_size as f32 / f32::from(face.units_per_em) * k;
                    let skew = if style.italic && !face.italic { s * OBLIQUE } else { 0.0 };
                    let t = moved(Transform::from_row(s, 0.0, skew, -s, px(origin.x + g.position.x), px(origin.y + g.position.y)));
                    pm.fill_path(&path, &p, FillRule::Winding, t, None);
                    if crate::synthetic_bold(style, face.weight) {
                        let stroke = Stroke { width: f32::from(face.units_per_em) * 0.03, ..Stroke::default() };
                        pm.stroke_path(&path, &p, &stroke, t, None);
                    }
                }
                for (from, to, width) in crate::shape::text_decorations(glyphs, *origin, style, shaped)? {
                    let mut pb = PathBuilder::new();
                    pb.move_to(px(from.x), px(from.y));
                    pb.line_to(px(to.x), px(to.y));
                    if let Some(path) = pb.finish() {
                        pm.stroke_path(&path, &p, &Stroke { width: px(width).max(0.1), ..Stroke::default() }, base, None);
                    }
                }
            }
        }
    }
    write_png(&pm, profile.dpi)
}

pub(crate) fn shape_path(kind: ShapeKind, x: f32, y: f32, w: f32, h: f32) -> Option<tiny_skia::Path> {
    let w = w.max(0.01);
    let h = h.max(0.01);
    match kind {
        ShapeKind::Ellipse => PathBuilder::from_oval(tiny_skia::Rect::from_xywh(x, y, w, h)?),
        ShapeKind::RoundRect => {
            let r = (w.min(h) * 0.1).max(0.0);
            let mut pb = PathBuilder::new();
            let c = 0.552_284_8 * r;
            pb.move_to(x + r, y);
            pb.line_to(x + w - r, y);
            pb.cubic_to(x + w - r + c, y, x + w, y + r - c, x + w, y + r);
            pb.line_to(x + w, y + h - r);
            pb.cubic_to(x + w, y + h - r + c, x + w - r + c, y + h, x + w - r, y + h);
            pb.line_to(x + r, y + h);
            pb.cubic_to(x + r - c, y + h, x, y + h - r + c, x, y + h - r);
            pb.line_to(x, y + r);
            pb.cubic_to(x, y + r - c, x + r - c, y, x + r, y);
            pb.close();
            pb.finish()
        }
        ShapeKind::Rect | ShapeKind::Unknown => Some(PathBuilder::from_rect(tiny_skia::Rect::from_xywh(x, y, w, h)?)),
    }
}

/// PNG with straight alpha and a pHYs chunk recording the DPI.
fn write_png(pm: &Pixmap, dpi: u32) -> DltResult<Vec<u8>> {
    let mut data = Vec::with_capacity(pm.data().len());
    for p in pm.pixels() {
        let c = p.demultiply();
        data.extend_from_slice(&[c.red(), c.green(), c.blue(), c.alpha()]);
    }
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, pm.width(), pm.height());
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let ppm = (f64::from(dpi) / 0.0254).round() as u32;
        enc.set_pixel_dims(Some(png::PixelDimensions { xppu: ppm, yppu: ppm, unit: png::Unit::Meter }));
        let mut writer = enc.write_header().map_err(|e| DltError::new(ErrorCode::Internal, format!("PNG header: {e}")))?;
        writer.write_image_data(&data).map_err(|e| DltError::new(ErrorCode::Internal, format!("PNG data: {e}")))?;
    }
    Ok(out)
}

// ---------------------------------------------------------------- contract v1.2 drawing (P1)
//
// Geometry is built in page pixels; `base` moves it onto the raster (identity for whole pages, an
// integer translation for regions on whole pixels).

/// Solid paint of `value`, or the style's gradient laid out on `bounds`; both faded by `opacity`.
fn fill_paint(style: &Style, value: &str, bounds: Rect, k: f32, opacity: f32, node: &str, diags: &mut Diags) -> Option<Paint<'static>> {
    if let Some(g) = &style.gradient {
        let (a, b) = effects::gradient_line(bounds, g.angle_millideg);
        let stops: Vec<tiny_skia::GradientStop> = g.stops.iter().filter_map(|s| parse_color(&s.color).ok().flatten().map(|c| {
            let c = faded(c, opacity);
            tiny_skia::GradientStop::new(s.pos_milli.min(1000) as f32 / 1000.0, tiny_skia::Color::from_rgba8(c.r, c.g, c.b, c.a))
        })).collect();
        let shader = tiny_skia::LinearGradient::new(tiny_skia::Point::from_xy(a.x as f32 * k, a.y as f32 * k), tiny_skia::Point::from_xy(b.x as f32 * k, b.y as f32 * k), stops, tiny_skia::SpreadMode::Pad, Transform::identity())?;
        let mut p = Paint::default();
        p.shader = shader;
        p.anti_alias = true;
        return Some(p);
    }
    diags.color(value, node).map(|c| paint(faded(c, opacity)))
}

#[allow(clippy::too_many_arguments)]
fn stroke_v12(pm: &mut Pixmap, path: &tiny_skia::Path, style: &Style, width: i64, k: f32, opacity: f32, base: Transform, node: &str, diags: &mut Diags) {
    if width <= 0 { return; }
    let Some(c) = diags.color(&style.stroke, node) else { return };
    let s = effects::sk_stroke(style, (width as f32 * k).max(0.1), k);
    effects::stroke_as_fill(pm, path, &s, &paint(faded(c, opacity)), base, None);
}

/// CSS outer shadows of a box (`shape_at(dx, dy, outset)` builds the box path in page pixels).
#[allow(clippy::too_many_arguments)]
fn shadows_v12(pm: &mut Pixmap, style: &Style, k: f32, opacity: f32, base: Transform, shape_at: &dyn Fn(f32, f32, f32) -> Option<tiny_skia::Path>, node: &str, diags: &mut Diags) {
    let (ox, oy) = (-base.tx, -base.ty);
    let visible = (ox, oy, ox + pm.width() as f32, oy + pm.height() as f32);
    for s in &style.shadows {
        let Some(c) = diags.color(&s.color, node) else { continue };
        let c = faded(c, opacity);
        if let Some((x0, y0, w, h, a)) = effects::shadow_alpha(s, k, visible, shape_at, c.a) {
            effects::composite(pm, (x0 as f32 - ox).round() as i32, (y0 as f32 - oy).round() as i32, w, h, &a, c);
        }
    }
}

/// Box path of a shape primitive with an offset and an outset (shadow spread), page pixels.
pub(crate) fn box_path(kind: ShapeKind, bounds: Rect, radius: i64, k: f32, dx: f32, dy: f32, out: f32) -> Option<tiny_skia::Path> {
    let (x, y) = (bounds.x as f32 * k + dx - out, bounds.y as f32 * k + dy - out);
    let (w, h) = (bounds.width as f32 * k + 2.0 * out, bounds.height as f32 * k + 2.0 * out);
    if w <= 0.0 || h <= 0.0 { return None; }
    match kind {
        ShapeKind::Ellipse => PathBuilder::from_oval(tiny_skia::Rect::from_xywh(x, y, w, h)?),
        _ => effects::rounded_rect(x, y, w, h, if radius > 0 { (radius as f32 * k + out).max(0.0) } else { 0.0 }),
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_shape_v12(pm: &mut Pixmap, node: &str, bounds: Rect, kind: ShapeKind, style: &Style, k: f32, base: Transform, diags: &mut Diags) {
    let op = effects::opacity(style);
    let radius = if kind == ShapeKind::Ellipse { 0 } else { effects::corner_radius(style, bounds, kind == ShapeKind::RoundRect) };
    shadows_v12(pm, style, k, op, base, &|dx, dy, out| box_path(kind, bounds, radius, k, dx, dy, out), node, diags);
    let Some(path) = box_path(kind, bounds, radius, k, 0.0, 0.0, 0.0) else { return };
    if let Some(p) = fill_paint(style, &style.fill, bounds, k, op, node, diags) { pm.fill_path(&path, &p, FillRule::Winding, base, None); }
    stroke_v12(pm, &path, style, style.stroke_width, k, op, base, node, diags);
}

#[allow(clippy::too_many_arguments)]
fn draw_line_v12(pm: &mut Pixmap, node: &str, from: Point, to: Point, style: &Style, arrow: bool, k: f32, base: Transform, diags: &mut Diags) {
    let op = effects::opacity(style);
    let mut pb = PathBuilder::new();
    pb.move_to(from.x as f32 * k, from.y as f32 * k);
    pb.line_to(to.x as f32 * k, to.y as f32 * k);
    if let Some(path) = pb.finish() { stroke_v12(pm, &path, style, style.stroke_width.max(1), k, op, base, node, diags); }
    let Some(c) = diags.color(&style.stroke, node) else { return };
    for marker in crate::encode::line_markers(from, to, style, arrow).into_iter().flatten() {
        let [tip, left, right] = marker.points;
        match marker.kind {
            LineEnd::Filled => {
                let mut pb = PathBuilder::new();
                pb.move_to(tip.x as f32 * k, tip.y as f32 * k);
                pb.line_to(left.x as f32 * k, left.y as f32 * k);
                pb.line_to(right.x as f32 * k, right.y as f32 * k);
                pb.close();
                if let Some(path) = pb.finish() { pm.fill_path(&path, &paint(faded(c, op)), FillRule::Winding, base, None); }
            }
            LineEnd::Open => {
                let mut pb = PathBuilder::new();
                pb.move_to(tip.x as f32 * k, tip.y as f32 * k);
                pb.line_to(left.x as f32 * k, left.y as f32 * k);
                pb.move_to(tip.x as f32 * k, tip.y as f32 * k);
                pb.line_to(right.x as f32 * k, right.y as f32 * k);
                if let Some(path) = pb.finish() { stroke_v12(pm, &path, style, style.stroke_width.max(1), k, op, base, node, diags); }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_path(pm: &mut Pixmap, node: &str, bounds: Rect, path: &PathSpec, style: &Style, k: f32, base: Transform, diags: &mut Diags) {
    let Some(p) = effects::sk_path(path, bounds, k) else { return };
    let op = effects::opacity(style);
    if let Some(fill) = fill_paint(style, &style.fill, bounds, k, op, node, diags) { pm.fill_path(&p, &fill, effects::sk_fill_rule(path.fill_rule), base, None); }
    stroke_v12(pm, &p, style, style.stroke_width, k, op, base, node, diags);
}
