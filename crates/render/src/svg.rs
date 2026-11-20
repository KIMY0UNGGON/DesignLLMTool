//! SVG encoder. User units are EMU (exact integers); width/height carry the physical size in mm.
//!
//! Default mode keeps text as `<text>` elements (editable, Editability::Objects) placed at the
//! shaped origin with `textLength` equal to the shaped advance. Every used bundled OFL face is
//! embedded once as a Unicode-mapped subset in an `@font-face` data URI, so
//! the file renders without installed fonts. Used system faces are not embedded: reported as the
//! blocking loss `svg_font_not_embedded` and left to the export loss policy.
//! Outline mode draws shaped glyph outlines as paths: `text_outlined` (blocking) and
//! Editability::Vector.
use crate::encode::{arrow_head, Diags, GlyphSource, ShapeKind};
use crate::paint::{base64, crop_diagnostic_code, decode, hex, sniff, validate_jpeg_crop_orientation, ImageKind};
use designllm_contracts::*;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

mod font;

pub(crate) fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c if (c as u32) < 0x20 && c != '\t' && c != '\n' && c != '\r' => {}
            c => out.push(c),
        }
    }
    out
}

fn mm(emu: i64) -> String { format!("{:.4}", emu as f64 / EMU_PER_MM as f64) }

fn paint_attr(diags: &mut Diags, name: &str, value: &str, node: &str) -> String {
    match diags.color(value, node) {
        Some(c) if c.a == 255 => format!(" {name}=\"{}\"", hex(c)),
        Some(c) => format!(" {name}=\"{}\" {name}-opacity=\"{:.4}\"", hex(c), f64::from(c.a) / 255.0),
        None => format!(" {name}=\"none\""),
    }
}

struct PathOut(String);
impl ttf_parser::OutlineBuilder for PathOut {
    fn move_to(&mut self, x: f32, y: f32) { let _ = write!(self.0, "M{x} {y}"); }
    fn line_to(&mut self, x: f32, y: f32) { let _ = write!(self.0, "L{x} {y}"); }
    fn quad_to(&mut self, x1: f32, y1: f32, x: f32, y: f32) { let _ = write!(self.0, "Q{x1} {y1} {x} {y}"); }
    fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x: f32, y: f32) { let _ = write!(self.0, "C{x1} {y1} {x2} {y2} {x} {y}"); }
    fn close(&mut self) { self.0.push('Z'); }
}

pub(crate) fn encode_svg(glyphs: &GlyphSource<'_>, page: &RenderPage, assets: &dyn AssetStore, outline: bool, diags: &mut Diags) -> DltResult<String> {
    let mut defs = String::new();
    let mut body = String::new();
    let mut defined: BTreeSet<(String, u32)> = BTreeSet::new();
    let mut embedded = BTreeMap::new();
    let mut not_embedded: BTreeSet<String> = BTreeSet::new();
    let mut ids = 0usize;
    for primitive in &page.primitives {
        match primitive {
            Primitive::Path { node_id, bounds, path, style } => fx::path(&mut body, &mut defs, &mut ids, diags, node_id, *bounds, path, style),
            Primitive::Shape { node_id, bounds, shape, style } if crate::v12::has_effects(style) => {
                let kind = ShapeKind::parse(shape);
                if kind == ShapeKind::Unknown { diags.add("shape_approximated", format!("Shape '{shape}' is drawn as its bounding rectangle"), Some(node_id), false); }
                fx::shape(&mut body, &mut defs, &mut ids, diags, node_id, *bounds, kind, style);
            }
            Primitive::Line { node_id, from, to, style, arrow } if crate::v12::has_effects(style) => fx::line(&mut body, diags, node_id, *from, *to, style, *arrow),
            Primitive::Shape { node_id, bounds, shape, style } => {
                let kind = ShapeKind::parse(shape);
                if kind == ShapeKind::Unknown { diags.add("shape_approximated", format!("Shape '{shape}' is drawn as its bounding rectangle"), Some(node_id), false); }
                let fill = paint_attr(diags, "fill", &style.fill, node_id);
                let stroke = if style.stroke_width > 0 { format!("{} stroke-width=\"{}\"", paint_attr(diags, "stroke", &style.stroke, node_id), style.stroke_width) } else { " stroke=\"none\"".into() };
                let id = esc(node_id);
                match kind {
                    ShapeKind::Ellipse => { let _ = writeln!(body, "<ellipse data-node-id=\"{id}\" cx=\"{}\" cy=\"{}\" rx=\"{}\" ry=\"{}\"{fill}{stroke}/>", bounds.x + bounds.width / 2, bounds.y + bounds.height / 2, bounds.width / 2, bounds.height / 2); }
                    ShapeKind::RoundRect => { let r = bounds.width.min(bounds.height) / 10; let _ = writeln!(body, "<rect data-node-id=\"{id}\" x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\" rx=\"{r}\"{fill}{stroke}/>", bounds.x, bounds.y, bounds.width, bounds.height); }
                    _ => { let _ = writeln!(body, "<rect data-node-id=\"{id}\" x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\"{fill}{stroke}/>", bounds.x, bounds.y, bounds.width, bounds.height); }
                }
            }
            Primitive::Line { node_id, from, to, style, arrow } => {
                let stroke = paint_attr(diags, "stroke", &style.stroke, node_id);
                let id = esc(node_id);
                let _ = writeln!(body, "<line data-node-id=\"{id}\" x1=\"{}\" y1=\"{}\" x2=\"{}\" y2=\"{}\"{stroke} stroke-width=\"{}\"/>", from.x, from.y, to.x, to.y, style.stroke_width.max(1));
                if *arrow {
                    if let Some(h) = arrow_head(*from, *to, style.stroke_width) {
                        let fill = paint_attr(diags, "fill", &style.stroke, node_id);
                        let _ = writeln!(body, "<path data-node-id=\"{id}\" d=\"M{} {}L{} {}L{} {}Z\"{fill}/>", h[0].x, h[0].y, h[1].x, h[1].y, h[2].x, h[2].y);
                    }
                }
            }
            Primitive::Image { node_id, bounds, asset, fit, image_crop_pixel_rect, style } => {
                let id = esc(node_id);
                let look = fit.is_some() || image_crop_pixel_rect.is_some() || style.is_some();
                let mut degraded: Vec<String> = Vec::new();
                let mut image_pixels = None;
                let uri = assets.read(asset).and_then(|bytes| {
                    if image_crop_pixel_rect.is_some() && sniff(&bytes) == ImageKind::Jpeg {
                        validate_jpeg_crop_orientation(&bytes)?;
                    }
                    match sniff(&bytes) {
                    ImageKind::Png => {
                        // Cropped PNGs are re-encoded without source metadata so SVG image decoders
                        // cannot apply EXIF orientation after the crop geometry was calculated.
                        let img = decode(&bytes)?;
                        image_pixels = Some((img.width, img.height));
                        if img.warnings.is_empty() && image_crop_pixel_rect.is_none() {
                            Ok(format!("data:image/png;base64,{}", base64(&bytes)))
                        } else {
                            degraded = img.warnings.clone();
                            Ok(format!("data:image/png;base64,{}", base64(&crate::png_from_rgba(img.width, img.height, &img.rgba)?)))
                        }
                    }
                    ImageKind::Jpeg => {
                        let img = decode(&bytes)?;
                        image_pixels = Some((img.width, img.height));
                        Ok(format!("data:image/jpeg;base64,{}", base64(&bytes)))
                    }
                    _ => {
                        let img = decode(&bytes)?;
                        image_pixels = Some((img.width, img.height));
                        Ok(format!("data:image/png;base64,{}", base64(&crate::png_from_rgba(img.width, img.height, &img.rgba)?)))
                    }
                }});
                for w in degraded { diags.add("image_checksum_mismatch", format!("Image {node_id} is damaged and was decoded in degraded mode: {w}"), Some(node_id), true); }
                match uri {
                    Ok(uri) if image_crop_pixel_rect.is_some() => fx::image(&mut body, &mut defs, &mut ids, diags, node_id, *bounds, *fit, style.as_ref(), Some(&uri), *image_crop_pixel_rect, image_pixels),
                    Ok(uri) if look => fx::image(&mut body, &mut defs, &mut ids, diags, node_id, *bounds, *fit, style.as_ref(), Some(&uri), None, None),
                    Ok(uri) => { let _ = writeln!(body, "<image data-node-id=\"{id}\" x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\" preserveAspectRatio=\"none\" href=\"{uri}\"/>", bounds.x, bounds.y, bounds.width, bounds.height); }
                    Err(e) => {
                        let code = crop_diagnostic_code(&e).unwrap_or("image_not_rendered");
                        diags.add(code, format!("Image {node_id} is shown as a placeholder: {}", e.message), Some(node_id), true);
                        if look { fx::image(&mut body, &mut defs, &mut ids, diags, node_id, *bounds, *fit, style.as_ref(), None, None, None); } else {
                            let _ = writeln!(body, "<rect data-node-id=\"{id}\" x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\" fill=\"#E5E7EB\"/>", bounds.x, bounds.y, bounds.width, bounds.height);
                        }
                    }
                }
            }
            Primitive::Text { node_id, origin, text, style, shaped, .. } => {
                let id = esc(node_id);
                let fill = fx::paint(diags, "fill", &style.color, node_id, crate::effects::opacity(style));
                let color = diags.color(&style.color, node_id).map(hex).unwrap_or_else(|| "none".into());
                // v1.2 opacity on the synthetic bold stroke too (v1.1 output unchanged).
                let fade = match (style.opacity_milli.is_some(), diags.color(&style.color, node_id)) { (true, Some(c)) => format!(" stroke-opacity=\"{:.4}\"", f64::from(crate::effects::faded(c, crate::effects::opacity(style)).a) / 255.0), _ => String::new() };
                if outline {
                    let _ = writeln!(body, "<g data-node-id=\"{id}\" aria-label=\"{}\"{fill}>", esc(text));
                    for g in &shaped.glyphs {
                        let face = glyphs.face(shaped, &g.font_key)?;
                        if defined.insert((face.key.clone(), g.glyph_id)) {
                            let mut p = PathOut(String::new());
                            face.ttf.outline_glyph(ttf_parser::GlyphId(g.glyph_id as u16), &mut p);
                            let _ = writeln!(defs, "<path id=\"g{}-{}\" d=\"{}\"/>", face.key, g.glyph_id, p.0);
                        }
                        let s = style.font_size as f64 / f64::from(face.units_per_em);
                        let skew = if style.italic && !face.italic { s * 0.2126 } else { 0.0 };
                        let bold = if crate::synthetic_bold(style, face.weight) { format!(" stroke=\"{color}\" stroke-width=\"{}\"{fade}", f64::from(face.units_per_em) * 0.03) } else { String::new() };
                        let _ = writeln!(body, "<use href=\"#g{}-{}\" transform=\"matrix({s} 0 {skew} {} {} {})\"{bold}/>", face.key, g.glyph_id, -s, origin.x + g.position.x, origin.y + g.position.y);
                    }
                    body.push_str("</g>\n");
                } else {
                    let mut names: Vec<String> = Vec::new();
                    for f in &shaped.fonts_used {
                        if !names.contains(&f.family) { names.push(f.family.clone()); }
                        let face = glyphs.face(shaped, &f.key)?;
                        if face.bundled {
                            embedded.entry(face.key.clone()).or_insert_with(|| font::EmbeddedFont::new(face)).include(text, shaped);
                        } else {
                            not_embedded.insert(format!("{} ({})", face.family, face.postscript_name));
                        }
                    }
                    if names.is_empty() { names.push(style.font_family.clone()); }
                    let family = names.iter().map(|n| format!("'{}'", esc(&n.chars().filter(|c| !matches!(c, '\'' | '"' | '\\')).collect::<String>()))).collect::<Vec<_>>().join(", ");
                    let weight = if let Some(weight) = style.font_weight {
                        format!(" font-weight=\"{weight}\"")
                    } else if style.bold {
                        " font-weight=\"bold\"".into()
                    } else {
                        String::new()
                    };
                    let italic = if style.italic { " font-style=\"italic\"" } else { "" };
                    let width = shaped.size.width;
                    let length = if width > 0 && text.chars().count() > 1 { format!(" textLength=\"{width}\" lengthAdjust=\"spacing\"") } else { String::new() };
                    // v1.2 letter spacing (CSS: after every character), so textLength needs no adjustment.
                    let spacing = match style.letter_spacing { Some(ls) if ls != 0 => format!(" letter-spacing=\"{ls}\""), _ => String::new() };
                    let _ = writeln!(body, "<text data-node-id=\"{id}\" xml:space=\"preserve\" x=\"{}\" y=\"{}\" font-family=\"{family}, sans-serif\" font-size=\"{}\"{weight}{italic}{fill}{spacing}{length}>{}</text>", origin.x, origin.y, style.font_size, esc(text));
                }
                let stroke = fx::paint(diags, "stroke", &style.color, node_id, crate::effects::opacity(style));
                for (from, to, width) in crate::shape::text_decorations(glyphs, *origin, style, shaped)? {
                    let _ = writeln!(body, "<line data-node-id=\"{id}\" x1=\"{}\" y1=\"{}\" x2=\"{}\" y2=\"{}\"{stroke} stroke-width=\"{width}\"/>", from.x, from.y, to.x, to.y);
                }
            }
        }
    }
    let mut style_css = String::new();
    for font in embedded.values() {
        let face = &font.face;
        let subset = font.subset();
        let weight = face.weight;
        let slant = if face.italic { "italic" } else { "normal" };
        // CSS inside CDATA: no XML escaping; drop characters that could end the string or block.
        let family: String = face.family.chars().filter(|c| !matches!(c, '\'' | '"' | '\\' | '<' | '>' | ']' | '{' | '}')).collect();
        let _ = writeln!(style_css, "@font-face {{ font-family: '{family}'; font-weight: {weight}; font-style: {slant}; src: url(data:font/ttf;base64,{}) format('truetype'); }}", base64(subset.as_deref().unwrap_or(face.data)));
    }
    if !embedded.is_empty() {
        let names: Vec<String> = embedded.values().map(|f| f.face.postscript_name.clone()).collect();
        diags.add("svg_fonts_embedded", format!("Embedded font faces (SIL OFL 1.1): {}", names.join(", ")), None, false);
    }
    if !not_embedded.is_empty() {
        diags.add("svg_font_not_embedded", format!("System fonts are referenced by name only and may render differently: {}", not_embedded.into_iter().collect::<Vec<_>>().join(", ")), None, true);
    }
    let defs = if style_css.is_empty() { defs } else { format!("<style type=\"text/css\"><![CDATA[
{style_css}]]></style>
{defs}") };
    if outline && page.primitives.iter().any(|p| matches!(p, Primitive::Text { .. })) {
        diags.add("text_outlined", "Text is converted to glyph outlines; it is vector only and no longer editable as text", None, true);
    }
    Ok(format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" xmlns:xlink=\"http://www.w3.org/1999/xlink\" width=\"{}mm\" height=\"{}mm\" viewBox=\"0 0 {} {}\" data-page-index=\"{}\" data-page-id=\"{}\">\n<defs>\n{defs}</defs>\n{body}</svg>\n",
        mm(page.size.width), mm(page.size.height), page.size.width, page.size.height, page.page_index, esc(&page.id)
    ))
}

/// Contract v1.2 SVG drawing (P1): gradients, outer shadows (blur filter + outside mask), corner
/// radius, dash/cap/join, opacity on every paint, vector paths. Ids are `dlt-<kind><n>` in drawing order.
mod fx {
    use super::{esc, Diags, ShapeKind};
    use crate::effects::{self, faded};
    use crate::paint::{hex, parse_color};
    use designllm_contracts::*;
    use std::fmt::Write as _;

    /// Paint attribute with the style opacity applied (the v1.1 attribute itself at opacity 1).
    pub fn paint(diags: &mut Diags, name: &str, value: &str, node: &str, opacity: f32) -> String {
        match diags.color(value, node) {
            Some(c) => {
                let c = faded(c, opacity);
                if c.a == 255 { format!(" {name}=\"{}\"", hex(c)) } else { format!(" {name}=\"{}\" {name}-opacity=\"{:.4}\"", hex(c), f64::from(c.a) / 255.0) }
            }
            None => format!(" {name}=\"none\""),
        }
    }

    fn next(ids: &mut usize, kind: &str) -> String { let id = format!("dlt-{kind}{ids}"); *ids += 1; id }

    fn fill(defs: &mut String, ids: &mut usize, diags: &mut Diags, style: &Style, bounds: Rect, node: &str, opacity: f32) -> String {
        let Some(g) = &style.gradient else { return paint(diags, "fill", &style.fill, node, opacity) };
        let id = next(ids, "g");
        let (a, b) = effects::gradient_line(bounds, g.angle_millideg);
        let _ = write!(defs, "<linearGradient id=\"{id}\" gradientUnits=\"userSpaceOnUse\" x1=\"{}\" y1=\"{}\" x2=\"{}\" y2=\"{}\">", a.x, a.y, b.x, b.y);
        for s in &g.stops {
            if let Ok(Some(c)) = parse_color(&s.color) {
                let c = faded(c, opacity);
                let alpha = if c.a < 255 { format!(" stop-opacity=\"{:.4}\"", f64::from(c.a) / 255.0) } else { String::new() };
                let _ = write!(defs, "<stop offset=\"{:.3}\" stop-color=\"{}\"{alpha}/>", f64::from(s.pos_milli.min(1000)) / 1000.0, hex(c));
            }
        }
        defs.push_str("</linearGradient>\n");
        format!(" fill=\"url(#{id})\"")
    }

    fn stroke(diags: &mut Diags, style: &Style, width: i64, node: &str, opacity: f32) -> String {
        if width <= 0 { return " stroke=\"none\"".into(); }
        let mut s = format!("{} stroke-width=\"{width}\"", paint(diags, "stroke", &style.stroke, node, opacity));
        if let Some((d, g)) = effects::dash_pattern(style.stroke_dash, width) { let _ = write!(s, " stroke-dasharray=\"{d} {g}\""); }
        match style.stroke_cap { Some(StrokeCap::Round) => s.push_str(" stroke-linecap=\"round\""), Some(StrokeCap::Square) => s.push_str(" stroke-linecap=\"square\""), _ => {} }
        match style.stroke_join { Some(StrokeJoin::Round) => s.push_str(" stroke-linejoin=\"round\""), Some(StrokeJoin::Bevel) => s.push_str(" stroke-linejoin=\"bevel\""), _ => {} }
        s
    }

    fn markup(kind: ShapeKind, b: Rect, r: i64) -> String {
        match kind {
            ShapeKind::Ellipse => format!("ellipse cx=\"{}\" cy=\"{}\" rx=\"{}\" ry=\"{}\"", b.x + b.width / 2, b.y + b.height / 2, b.width / 2, b.height / 2),
            _ if r > 0 => format!("rect x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\" rx=\"{r}\" ry=\"{r}\"", b.x, b.y, b.width, b.height),
            _ => format!("rect x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\"", b.x, b.y, b.width, b.height),
        }
    }

    /// CSS outer shadows: the spread box offset and blurred (sigma = blur / 2), masked to outside the box.
    #[allow(clippy::too_many_arguments)]
    fn shadows(body: &mut String, defs: &mut String, ids: &mut usize, diags: &mut Diags, node: &str, kind: ShapeKind, b: Rect, r: i64, style: &Style, opacity: f32) {
        for s in &style.shadows {
            let Some(c) = diags.color(&s.color, node) else { continue };
            let c = faded(c, opacity);
            let caster = Rect { x: b.x + s.x - s.spread, y: b.y + s.y - s.spread, width: b.width + 2 * s.spread, height: b.height + 2 * s.spread };
            if caster.width <= 0 || caster.height <= 0 { continue; }
            let sigma = effects::shadow_sigma(s.blur);
            let pad = (sigma * 3.0).ceil() as i64 + 1;
            let (x0, y0) = (caster.x.min(b.x) - pad, caster.y.min(b.y) - pad);
            let (x1, y1) = ((caster.x + caster.width).max(b.x + b.width) + pad, (caster.y + caster.height).max(b.y + b.height) + pad);
            let (fid, mid) = (next(ids, "s"), next(ids, "m"));
            let area = format!("x=\"{x0}\" y=\"{y0}\" width=\"{}\" height=\"{}\"", x1 - x0, y1 - y0);
            let blur = if sigma > 0.0 {
                let _ = writeln!(defs, "<filter id=\"{fid}\" filterUnits=\"userSpaceOnUse\" {area} color-interpolation-filters=\"sRGB\"><feGaussianBlur stdDeviation=\"{sigma}\"/></filter>");
                format!(" filter=\"url(#{fid})\"")
            } else { String::new() };
            let _ = writeln!(defs, "<mask id=\"{mid}\" maskUnits=\"userSpaceOnUse\" {area}><rect {area} fill=\"#FFFFFF\"/><{} fill=\"#000000\"/></mask>", markup(kind, b, r));
            let alpha = if c.a < 255 { format!(" fill-opacity=\"{:.4}\"", f64::from(c.a) / 255.0) } else { String::new() };
            let rr = if r > 0 { (r + s.spread).max(0) } else { 0 };
            let _ = writeln!(body, "<g data-node-id=\"{}\" mask=\"url(#{mid})\"><{} fill=\"{}\"{alpha}{blur}/></g>", esc(node), markup(kind, caster, rr), hex(c));
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn shape(body: &mut String, defs: &mut String, ids: &mut usize, diags: &mut Diags, node: &str, b: Rect, kind: ShapeKind, style: &Style) {
        let op = effects::opacity(style);
        let r = if kind == ShapeKind::Ellipse { 0 } else { effects::corner_radius(style, b, kind == ShapeKind::RoundRect) };
        shadows(body, defs, ids, diags, node, kind, b, r, style, op);
        let fill = fill(defs, ids, diags, style, b, node, op);
        let stroke = stroke(diags, style, style.stroke_width, node, op);
        let _ = writeln!(body, "<{} data-node-id=\"{}\"{fill}{stroke}/>", markup(kind, b, r), esc(node));
    }

    pub fn line(body: &mut String, diags: &mut Diags, node: &str, from: Point, to: Point, style: &Style, arrow: bool) {
        let op = effects::opacity(style);
        let id = esc(node);
        let stroke = stroke(diags, style, style.stroke_width.max(1), node, op);
        let _ = writeln!(body, "<line data-node-id=\"{id}\" x1=\"{}\" y1=\"{}\" x2=\"{}\" y2=\"{}\"{stroke}/>", from.x, from.y, to.x, to.y);
        for marker in crate::encode::line_markers(from, to, style, arrow).into_iter().flatten() {
            match marker.kind {
                LineEnd::Filled => {
                    let fill = paint(diags, "fill", &style.stroke, node, op);
                    let [tip, left, right] = marker.points;
                    let _ = writeln!(body, "<path data-node-id=\"{id}\" d=\"M{} {}L{} {}L{} {}Z\"{fill}/>", tip.x, tip.y, left.x, left.y, right.x, right.y);
                }
                LineEnd::Open => {
                    let [tip, left, right] = marker.points;
                    let _ = writeln!(body, "<path data-node-id=\"{id}\" d=\"M{} {}L{} {}M{} {}L{} {}\" fill=\"none\"{stroke}/>", tip.x, tip.y, left.x, left.y, tip.x, tip.y, right.x, right.y);
                }
            }
        }
    }

    /// P1 image: fit as preserveAspectRatio (none / meet / slice, slice clipped to the bounds by the
    /// image viewport), corner radius as a clipPath, opacity on the image, outer shadows of the
    /// bounds as for shapes. `uri` None draws the placeholder in the same box.
    #[allow(clippy::too_many_arguments)]
    pub fn image(
        body: &mut String,
        defs: &mut String,
        ids: &mut usize,
        diags: &mut Diags,
        node: &str,
        b: Rect,
        fit: Option<ImageFit>,
        style: Option<&Style>,
        uri: Option<&str>,
        crop: Option<PixelRect>,
        pixels: Option<(u32, u32)>,
    ) {
        let op = style.map(effects::opacity).unwrap_or(1.0);
        let r = style.map(|s| effects::corner_radius(s, b, false)).unwrap_or(0);
        if let Some(s) = style { shadows(body, defs, ids, diags, node, ShapeKind::Rect, b, r, s, op); }
        if let (Some(uri), Some(crop)) = (uri, crop) {
            let placement = match pixels
                .ok_or_else(|| DltError::invalid("image crop source dimensions are unavailable"))
                .and_then(|pixels| crate::effects::image_crop_placement(b, pixels, crop, fit))
            {
                Ok(placement) => placement,
                Err(error) => {
                    diags.add("IMAGE_CROP_INVALID", format!("Image {node} is shown as a placeholder: {}", error.message), Some(node), true);
                    let alpha = if op < 1.0 { format!(" opacity=\"{op:.4}\"") } else { String::new() };
                    let _ = writeln!(body, "<{} data-node-id=\"{}\" fill=\"#E5E7EB\"{alpha}/>", markup(ShapeKind::Rect, b, r), esc(node));
                    return;
                }
            };
            let crop_clip_id = next(ids, "crop");
            let clip = placement.clip;
            let _ = writeln!(defs, "<clipPath id=\"{crop_clip_id}\" clipPathUnits=\"userSpaceOnUse\"><rect x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\"/></clipPath>", clip.x, clip.y, clip.width, clip.height);
            let rounded_clip = if r > 0 {
                let id = next(ids, "round");
                let _ = writeln!(defs, "<clipPath id=\"{id}\" clipPathUnits=\"userSpaceOnUse\"><{}/></clipPath>", markup(ShapeKind::Rect, b, r));
                format!(" clip-path=\"url(#{id})\"")
            } else { String::new() };
            let alpha = if op < 1.0 { format!(" opacity=\"{op:.4}\"") } else { String::new() };
            let _ = writeln!(body, "<g{rounded_clip}><image data-node-id=\"{}\" x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\" preserveAspectRatio=\"none\" clip-path=\"url(#{crop_clip_id})\"{alpha} href=\"{uri}\"/></g>", esc(node), placement.image_x, placement.image_y, placement.image_width, placement.image_height);
            return;
        }
        let clip = if r > 0 && uri.is_some() {
            let cid = next(ids, "c");
            let _ = writeln!(defs, "<clipPath id=\"{cid}\" clipPathUnits=\"userSpaceOnUse\"><{}/></clipPath>", markup(ShapeKind::Rect, b, r));
            format!(" clip-path=\"url(#{cid})\"")
        } else { String::new() };
        let alpha = if op < 1.0 { format!(" opacity=\"{op:.4}\"") } else { String::new() };
        match uri {
            Some(uri) => {
                let par = match fit { Some(ImageFit::Contain) => "xMidYMid meet", Some(ImageFit::Cover) => "xMidYMid slice", Some(ImageFit::Fill) | None => "none" };
                let _ = writeln!(body, "<image data-node-id=\"{}\" x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\" preserveAspectRatio=\"{par}\"{alpha}{clip} href=\"{uri}\"/>", esc(node), b.x, b.y, b.width, b.height);
            }
            None => { let _ = writeln!(body, "<{} data-node-id=\"{}\" fill=\"#E5E7EB\"{alpha}/>", markup(ShapeKind::Rect, b, r), esc(node)); }
        }
    }

    /// The path in page EMU (view box stretched onto the bounds, coordinates rounded to whole EMU).
    #[allow(clippy::too_many_arguments)]
    pub fn path(body: &mut String, defs: &mut String, ids: &mut usize, diags: &mut Diags, node: &str, bounds: Rect, path: &PathSpec, style: &Style) {
        let (sx, sy, ox, oy) = effects::path_transform(path, bounds);
        let p = |x: i64, y: i64| ((ox + x as f64 * sx).round() as i64, (oy + y as f64 * sy).round() as i64);
        let mut d = String::new();
        for c in &path.commands {
            let _ = match *c {
                PathCommand::MoveTo { x, y } => { let (a, b) = p(x, y); write!(d, "M{a} {b}") }
                PathCommand::LineTo { x, y } => { let (a, b) = p(x, y); write!(d, "L{a} {b}") }
                PathCommand::CubicTo { x1, y1, x2, y2, x, y } => { let ((a, b), (c2, e), (f, g)) = (p(x1, y1), p(x2, y2), p(x, y)); write!(d, "C{a} {b} {c2} {e} {f} {g}") }
                PathCommand::Close => { d.push('Z'); Ok(()) }
            };
        }
        let op = effects::opacity(style);
        let rule = if path.fill_rule == FillRule::Evenodd { " fill-rule=\"evenodd\"" } else { "" };
        let fill = fill(defs, ids, diags, style, bounds, node, op);
        let stroke = stroke(diags, style, style.stroke_width, node, op);
        let _ = writeln!(body, "<path data-node-id=\"{}\" d=\"{d}\"{rule}{fill}{stroke}/>", esc(node));
    }
}
