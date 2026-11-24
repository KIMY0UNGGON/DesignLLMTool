//! Contract v1.2 drawing model shared by the PNG, SVG and PDF encoders:
//! per-paint opacity, corner radius, CSS outer box shadow, two-stop linear gradient with the CSS angle
//! convention, stroke dash/cap/join, vector paths, image fit. Every rule here is public so other
//! writers (canvas HTML, PPTX) can reproduce the same geometry.
use crate::paint::Rgba;
use designllm_contracts::*;
use tiny_skia::{FillRule, LineCap, LineJoin, Mask, Paint, PathBuilder, Pixmap, Stroke, StrokeDash as SkDash, Transform};

/// `opacity_milli` as a factor in [0, 1] (None = 1).
pub fn opacity(style: &Style) -> f32 { style.opacity_milli.map(|m| m.min(1000) as f32 / 1000.0).unwrap_or(1.0) }

/// A paint's alpha multiplied by the style opacity (rounded half up).
pub(crate) fn faded(c: Rgba, opacity: f32) -> Rgba { Rgba { a: (f32::from(c.a) * opacity + 0.5).floor().clamp(0.0, 255.0) as u8, ..c } }

/// Effective corner radius in EMU: `corner_radius` (or 10% of min(w, h) for the `round_rect` shape when
/// unset), clamped to half the shorter side as CSS does. 0 = square corners.
pub fn corner_radius(style: &Style, bounds: Rect, round_rect_default: bool) -> i64 {
    let half = bounds.width.min(bounds.height).max(0) / 2;
    let r = match style.corner_radius { Some(r) => r.max(0), None if round_rect_default => bounds.width.min(bounds.height) / 10, None => 0 };
    r.min(half)
}

/// CSS linear-gradient line for `bounds` and `angle_millideg` (0 = bottom to top, clockwise): start
/// and end point in integer EMU (half away from zero). The line passes through the box centre and is
/// long enough that the corners get the first and last stop colours, as in CSS Images 3.
pub fn gradient_line(bounds: Rect, angle_millideg: i32) -> (Point, Point) {
    let theta = f64::from(angle_millideg.rem_euclid(360_000)) / 1000.0 * std::f64::consts::PI / 180.0;
    let (w, h) = (bounds.width as f64, bounds.height as f64);
    let len = (w * theta.sin()).abs() + (h * theta.cos()).abs();
    let (dx, dy) = (theta.sin() * len / 2.0, -theta.cos() * len / 2.0);
    let (cx, cy) = (bounds.x as f64 + w / 2.0, bounds.y as f64 + h / 2.0);
    let r = |v: f64| v.round() as i64;
    (Point { x: r(cx - dx), y: r(cy - dy) }, Point { x: r(cx + dx), y: r(cy + dy) })
}

/// Dash pattern (dash, gap) in EMU for a stroke `width`: dashed = 3w + 3w, dotted = 1w + 1w, solid =
/// none. Caps apply to every dash as in SVG.
pub fn dash_pattern(dash: Option<StrokeDash>, width: i64) -> Option<(i64, i64)> {
    let w = width.max(1);
    match dash { Some(StrokeDash::Dashed) => Some((3 * w, 3 * w)), Some(StrokeDash::Dotted) => Some((w, w)), Some(StrokeDash::Solid) | None => None }
}

/// Where an image of `pixels` is drawn for `bounds` and `fit` (the result is clipped to `bounds`):
/// fill stretches, contain fits inside (centred), cover fills and overflows (centred). EMU, half up.
pub fn image_fit_rect(bounds: Rect, pixels: (u32, u32), fit: ImageFit) -> Rect {
    let (pw, ph) = (i128::from(pixels.0.max(1)), i128::from(pixels.1.max(1)));
    let (bw, bh) = (i128::from(bounds.width), i128::from(bounds.height));
    let wider = bw * ph > bh * pw; // box aspect wider than the image
    let (w, h) = match fit {
        ImageFit::Fill => return bounds,
        ImageFit::Contain if wider => ((bh * pw * 2 + ph) / (2 * ph), bh),
        ImageFit::Contain => (bw, (bw * ph * 2 + pw) / (2 * pw)),
        ImageFit::Cover if wider => (bw, (bw * ph * 2 + pw) / (2 * pw)),
        ImageFit::Cover => ((bh * pw * 2 + ph) / (2 * ph), bh),
    };
    let (w, h) = (w as i64, h as i64);
    Rect { x: bounds.x + (bounds.width - w) / 2, y: bounds.y + (bounds.height - h) / 2, width: w, height: h }
}

/// Geometry for drawing the original image bytes through a pixel crop. `image_*` places the full
/// source image so `crop` maps exactly onto `destination`; vector encoders clip to `clip` to keep
/// pixels outside the selected window out of both the frame and any contain letterboxing.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct ImageCropPlacement {
    pub destination: Rect,
    pub clip: Rect,
    pub image_x: f64,
    pub image_y: f64,
    pub image_width: f64,
    pub image_height: f64,
}

pub(crate) fn image_crop_placement(
    bounds: Rect,
    pixels: (u32, u32),
    crop: PixelRect,
    fit: Option<ImageFit>,
) -> DltResult<ImageCropPlacement> {
    let (width, height) = (pixels.0, pixels.1);
    let right = crop.x.checked_add(crop.width).filter(|right| *right <= width);
    let bottom = crop.y.checked_add(crop.height).filter(|bottom| *bottom <= height);
    if width == 0 || height == 0 || crop.width == 0 || crop.height == 0 || right.is_none() || bottom.is_none() {
        return Err(DltError::invalid("image crop is empty or outside the source pixels"));
    }
    let bounds_right = bounds.x.checked_add(bounds.width).ok_or_else(|| DltError::invalid("image crop placement bounds overflow"))?;
    let bounds_bottom = bounds.y.checked_add(bounds.height).ok_or_else(|| DltError::invalid("image crop placement bounds overflow"))?;
    if bounds.width <= 0 || bounds.height <= 0 {
        return Err(DltError::invalid("image crop placement bounds must have positive size"));
    }
    let destination = image_fit_rect(bounds, (crop.width, crop.height), fit.unwrap_or(ImageFit::Fill));
    let dest_right = destination.x.checked_add(destination.width).ok_or_else(|| DltError::invalid("image crop placement destination overflow"))?;
    let dest_bottom = destination.y.checked_add(destination.height).ok_or_else(|| DltError::invalid("image crop placement destination overflow"))?;
    let clip_left = destination.x.max(bounds.x);
    let clip_top = destination.y.max(bounds.y);
    let clip_right = dest_right.min(bounds_right);
    let clip_bottom = dest_bottom.min(bounds_bottom);
    if destination.width <= 0 || destination.height <= 0 || clip_right <= clip_left || clip_bottom <= clip_top {
        return Err(DltError::invalid("image crop placement has no visible area"));
    }
    let scale_x = destination.width as f64 / f64::from(crop.width);
    let scale_y = destination.height as f64 / f64::from(crop.height);
    Ok(ImageCropPlacement {
        destination,
        clip: Rect { x: clip_left, y: clip_top, width: clip_right - clip_left, height: clip_bottom - clip_top },
        image_x: destination.x as f64 - f64::from(crop.x) * scale_x,
        image_y: destination.y as f64 - f64::from(crop.y) * scale_y,
        image_width: f64::from(width) * scale_x,
        image_height: f64::from(height) * scale_y,
    })
}

/// Gaussian sigma of a CSS shadow blur radius (σ = blur / 2).
pub fn shadow_sigma(blur: i64) -> f64 { blur.max(0) as f64 / 2.0 }

pub(crate) fn sk_cap(cap: Option<StrokeCap>) -> LineCap { match cap { Some(StrokeCap::Round) => LineCap::Round, Some(StrokeCap::Square) => LineCap::Square, Some(StrokeCap::Butt) | None => LineCap::Butt } }
pub(crate) fn sk_join(join: Option<StrokeJoin>) -> LineJoin { match join { Some(StrokeJoin::Round) => LineJoin::Round, Some(StrokeJoin::Bevel) => LineJoin::Bevel, Some(StrokeJoin::Miter) | None => LineJoin::Miter } }

/// tiny-skia stroke for `style` with `width_px`.
pub(crate) fn sk_stroke(style: &Style, width_px: f32, px_per_emu: f32) -> Stroke {
    let dash = dash_pattern(style.stroke_dash, style.stroke_width).and_then(|(d, g)| SkDash::new(vec![d as f32 * px_per_emu, g as f32 * px_per_emu], 0.0));
    Stroke { width: width_px, line_cap: sk_cap(style.stroke_cap), line_join: sk_join(style.stroke_join), dash, ..Stroke::default() }
}

/// Rounded rectangle (or rectangle when `r` is 0) path in device units.
pub(crate) fn rounded_rect(x: f32, y: f32, w: f32, h: f32, r: f32) -> Option<tiny_skia::Path> {
    let (w, h) = (w.max(0.01), h.max(0.01));
    let r = r.clamp(0.0, w.min(h) / 2.0);
    if r <= 0.0 { return Some(PathBuilder::from_rect(tiny_skia::Rect::from_xywh(x, y, w, h)?)); }
    let c = 0.552_284_8 * r;
    let mut pb = PathBuilder::new();
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

/// Maps a PathSpec (user units × 1000) onto `bounds` (EMU): (scale x, scale y, offset x, offset y) such
/// that EMU = offset + coordinate × scale. The view box is stretched onto the bounds.
pub fn path_transform(path: &PathSpec, bounds: Rect) -> (f64, f64, f64, f64) {
    let vb = path.view_box;
    let sx = if vb.width != 0 { bounds.width as f64 / vb.width as f64 } else { 1.0 };
    let sy = if vb.height != 0 { bounds.height as f64 / vb.height as f64 } else { 1.0 };
    (sx, sy, bounds.x as f64 - vb.x as f64 * sx, bounds.y as f64 - vb.y as f64 * sy)
}

/// Device-space tiny-skia path of a PathSpec placed on `bounds` (`k` = px per EMU).
pub(crate) fn sk_path(path: &PathSpec, bounds: Rect, k: f32) -> Option<tiny_skia::Path> {
    let (sx, sy, ox, oy) = path_transform(path, bounds);
    let p = |x: i64, y: i64| (((ox + x as f64 * sx) * f64::from(k)) as f32, ((oy + y as f64 * sy) * f64::from(k)) as f32);
    let mut pb = PathBuilder::new();
    for c in &path.commands {
        match *c {
            PathCommand::MoveTo { x, y } => { let (a, b) = p(x, y); pb.move_to(a, b); }
            PathCommand::LineTo { x, y } => { let (a, b) = p(x, y); pb.line_to(a, b); }
            PathCommand::CubicTo { x1, y1, x2, y2, x, y } => { let (a, b) = p(x1, y1); let (c2, d) = p(x2, y2); let (e, f) = p(x, y); pb.cubic_to(a, b, c2, d, e, f); }
            PathCommand::Close => pb.close(),
        }
    }
    pb.finish()
}

pub(crate) fn sk_fill_rule(rule: FillRule_) -> FillRule { match rule { FillRule_::Nonzero => FillRule::Winding, FillRule_::Evenodd => FillRule::EvenOdd } }
pub(crate) use designllm_contracts::FillRule as FillRule_;

fn subpaths(path: &tiny_skia::Path) -> Vec<(tiny_skia::Path, bool)> {
    use tiny_skia::PathSegment as S;
    let mut out = Vec::new();
    let mut pb = PathBuilder::new();
    let mut closed = false;
    let flush = |pb: &mut PathBuilder, closed: &mut bool, out: &mut Vec<(tiny_skia::Path, bool)>| {
        let done = std::mem::replace(pb, PathBuilder::new());
        if let Some(p) = done.finish() { out.push((p, *closed)); }
        *closed = false;
    };
    for seg in path.segments() {
        match seg {
            S::MoveTo(p) => { flush(&mut pb, &mut closed, &mut out); pb.move_to(p.x, p.y); }
            S::LineTo(p) => pb.line_to(p.x, p.y),
            S::QuadTo(a, p) => pb.quad_to(a.x, a.y, p.x, p.y),
            S::CubicTo(a, b, p) => pb.cubic_to(a.x, a.y, b.x, b.y, p.x, p.y),
            S::Close => { pb.close(); closed = true; }
        }
    }
    flush(&mut pb, &mut closed, &mut out);
    out
}

/// Stroke outline drawn as a fill (GUI_DESIGN §17.5): joins are kept at
/// one device pixel and below (no hairline shortcut), and a closed subpath no larger than the stroke
/// width (a small circle) is filled whole instead of leaving the inner offset's hole.
/// `path` and `stroke` are in pixels; `ts` places them (a translation).
pub(crate) fn stroke_as_fill(pm: &mut Pixmap, path: &tiny_skia::Path, stroke: &Stroke, paint: &Paint<'_>, ts: Transform, mask: Option<&Mask>) {
    let (w, res) = (stroke.width, 1.0);
    // Path::stroke does not dash (Pixmap::stroke_path does it first), so dash here.
    let dashed = match &stroke.dash { Some(d) => match path.dash(d, res) { Some(p) => p, None => return }, None => path.clone() };
    // Every other subpath is stroked together and filled once (the union, no double-blended overlaps), as
    // `stroke_path` does; small closed ones fill each contour of their outline.
    let parts = subpaths(&dashed);
    let is_small = |(sub, closed): &(tiny_skia::Path, bool)| { let b = sub.bounds(); *closed && b.width().max(b.height()) <= w };
    let rest = if parts.iter().any(is_small) {
        let mut pb = PathBuilder::new();
        for part in parts.iter().filter(|p| !is_small(p)) { pb.push_path(&part.0); }
        pb.finish()
    } else { Some(dashed.clone()) };
    if let Some(outline) = rest.and_then(|p| p.stroke(stroke, res)) { pm.fill_path(&outline, paint, FillRule::Winding, ts, mask); }
    for (sub, _) in parts.iter().filter(|p| is_small(p)) {
        let Some(outline) = sub.stroke(stroke, res) else { continue };
        for (contour, _) in subpaths(&outline) { pm.fill_path(&contour, paint, FillRule::Winding, ts, mask); }
    }
}

/// Separable Gaussian blur of an 8-bit coverage buffer (`w` × `h`), σ in pixels, kernel radius 3σ.
pub(crate) fn gaussian_blur(data: &mut [u8], w: usize, h: usize, sigma: f64) {
    if sigma < 0.05 || w == 0 || h == 0 { return; }
    let radius = (sigma * 3.0).ceil() as isize;
    let kernel: Vec<f64> = (-radius..=radius).map(|i| (-(i * i) as f64 / (2.0 * sigma * sigma)).exp()).collect();
    let sum: f64 = kernel.iter().sum();
    let kernel: Vec<f64> = kernel.iter().map(|v| v / sum).collect();
    let mut tmp = vec![0f64; w * h];
    for y in 0..h {
        for x in 0..w {
            let mut acc = 0.0;
            for (k, kv) in kernel.iter().enumerate() {
                let xx = x as isize + k as isize - radius;
                if xx >= 0 && (xx as usize) < w { acc += f64::from(data[y * w + xx as usize]) * kv; }
            }
            tmp[y * w + x] = acc;
        }
    }
    for y in 0..h {
        for x in 0..w {
            let mut acc = 0.0;
            for (k, kv) in kernel.iter().enumerate() {
                let yy = y as isize + k as isize - radius;
                if yy >= 0 && (yy as usize) < h { acc += tmp[yy as usize * w + x] * kv; }
            }
            data[y * w + x] = acc.round().clamp(0.0, 255.0) as u8;
        }
    }
}

/// A CSS outer shadow as an alpha raster in page pixels: (left, top, width, height, alpha). The shape
/// (`shape` builds the box path at an offset with an extra outset) is spread, offset and blurred, then
/// cut out where the box itself is (CSS draws outer shadows outside the border box only). Only what
/// can reach `visible` (left, top, right, bottom in page pixels) is computed.
pub(crate) fn shadow_alpha(shadow: &Shadow, k: f32, visible: (f32, f32, f32, f32), shape: &dyn Fn(f32, f32, f32) -> Option<tiny_skia::Path>, alpha: u8) -> Option<(i32, i32, u32, u32, Vec<u8>)> {
    let sigma = (shadow_sigma(shadow.blur) * f64::from(k)) as f32;
    let spread = shadow.spread as f32 * k;
    let (dx, dy) = (shadow.x as f32 * k, shadow.y as f32 * k);
    let caster = shape(dx, dy, spread)?;
    let box_path = shape(0.0, 0.0, 0.0)?;
    let pad = (sigma * 3.0).ceil() + 1.0;
    let b = caster.bounds();
    // The layer reaches a blur radius past the visible area, so pixels near its edge also blur the
    // caster outside it (a region render equals the crop of the whole page). Integer layer origins
    // keep the caster on the page pixel grid.
    let (x0, y0) = ((b.left() - pad).max(visible.0 - pad).floor() as i32, (b.top() - pad).max(visible.1 - pad).floor() as i32);
    let (x1, y1) = ((b.right() + pad).min(visible.2 + pad).ceil() as i32, (b.bottom() + pad).min(visible.3 + pad).ceil() as i32);
    if x1 <= x0 || y1 <= y0 { return None; }
    let (w, h) = ((x1 - x0) as u32, (y1 - y0) as u32);
    let mut layer = Pixmap::new(w, h)?;
    let mut white = Paint::default();
    white.set_color_rgba8(0, 0, 0, 255);
    white.anti_alias = true;
    let to_layer = Transform::from_translate(-x0 as f32, -y0 as f32);
    layer.fill_path(&caster, &white, FillRule::Winding, to_layer, None);
    let mut a: Vec<u8> = layer.pixels().iter().map(|p| p.alpha()).collect();
    gaussian_blur(&mut a, w as usize, h as usize, f64::from(sigma));
    let mut hole = Pixmap::new(w, h)?;
    hole.fill_path(&box_path, &white, FillRule::Winding, to_layer, None);
    for (v, p) in a.iter_mut().zip(hole.pixels()) {
        let outside = 255 - u16::from(p.alpha());
        *v = ((u32::from(*v) * u32::from(outside) * u32::from(alpha) + 255 * 255 / 2) / (255 * 255)) as u8;
    }
    Some((x0, y0, w, h, a))
}

/// Source-over a solid colour through an alpha raster placed at (x0, y0).
pub(crate) fn composite(pm: &mut Pixmap, x0: i32, y0: i32, w: u32, h: u32, alpha: &[u8], c: Rgba) {
    let (pw, ph) = (pm.width() as i32, pm.height() as i32);
    let data = pm.data_mut();
    for yy in 0..h as i32 {
        let y = y0 + yy;
        if y < 0 || y >= ph { continue; }
        for xx in 0..w as i32 {
            let x = x0 + xx;
            if x < 0 || x >= pw { continue; }
            let a = u32::from(alpha[(yy as u32 * w + xx as u32) as usize]);
            if a == 0 { continue; }
            let i = ((y * pw + x) * 4) as usize;
            let inv = 255 - a;
            for (ch, src) in [(0usize, c.r), (1, c.g), (2, c.b)] {
                let s = (u32::from(src) * a + 127) / 255;
                data[i + ch] = (s + (u32::from(data[i + ch]) * inv + 127) / 255).min(255) as u8;
            }
            data[i + 3] = (a + (u32::from(data[i + 3]) * inv + 127) / 255).min(255) as u8;
        }
    }
}

#[cfg(test)]
mod crop_tests {
    use super::*;

    #[test]
    fn crop_geometry_fits_the_selected_pixels_and_clips_letterboxing() {
        let bounds = Rect { x: 100, y: 200, width: 1000, height: 500 };
        let crop = PixelRect { x: 1, y: 0, width: 2, height: 2 };
        let contain = image_crop_placement(bounds, (4, 2), crop, Some(ImageFit::Contain)).unwrap();
        assert_eq!(contain.destination, Rect { x: 350, y: 200, width: 500, height: 500 });
        assert_eq!(contain.clip, contain.destination, "uncropped source pixels must not bleed into contain letterboxing");
        assert_eq!((contain.image_x, contain.image_y, contain.image_width, contain.image_height), (100.0, 200.0, 1000.0, 500.0));
    }

    #[test]
    fn crop_geometry_uses_crop_aspect_for_cover_then_clips_to_frame() {
        let bounds = Rect { x: 100, y: 200, width: 1000, height: 500 };
        let placement = image_crop_placement(
            bounds,
            (4, 2),
            PixelRect { x: 1, y: 0, width: 2, height: 2 },
            Some(ImageFit::Cover),
        ).unwrap();
        assert_eq!(placement.destination, Rect { x: 100, y: -50, width: 1000, height: 1000 });
        assert_eq!(placement.clip, bounds);
        assert_eq!((placement.image_x, placement.image_y, placement.image_width, placement.image_height), (-400.0, -50.0, 2000.0, 1000.0));
    }

    #[test]
    fn malformed_crop_rectangles_are_rejected_before_geometry_is_used() {
        let bounds = Rect { x: 0, y: 0, width: 1000, height: 1000 };
        for crop in [
            PixelRect { x: 0, y: 0, width: 0, height: 1 },
            PixelRect { x: 3, y: 0, width: 2, height: 1 },
            PixelRect { x: u32::MAX, y: 0, width: 1, height: 1 },
        ] {
            assert!(image_crop_placement(bounds, (4, 2), crop, Some(ImageFit::Fill)).is_err());
        }
    }
}
