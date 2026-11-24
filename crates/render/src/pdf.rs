//! PDF encoder: multi-page, embedded subset fonts (Type0/CIDFontType2 or CFF, Identity-H) with
//! ToUnicode and ActualText, images (JPEG pass-through, PNG/BMP as Flate RGB + SMask), shapes,
//! lines. Page boxes use the exact EMU size in points (no integer truncation).
use crate::effects::{self, faded};
use crate::encode::{arrow_head, Diags, GlyphSource, ShapeKind};
use crate::fonts::Face;
use crate::paint::{crop_diagnostic_code, decode, jpeg_info, parse_color, sniff, validate_jpeg_crop_orientation, ImageKind, Rgba};
use designllm_contracts::*;
use pdf_writer::types::{ActionType, AnnotationType, CidFontType, FontFlags, FunctionShadingType, LineCapStyle, LineJoinStyle, SystemInfo, TextRenderingMode, UnicodeCmap};
use pdf_writer::{Content, Filter, Finish, Name, Pdf, Rect as PdfRect, Ref, Str, TextStr};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::sync::Arc;
use unicode_segmentation::UnicodeSegmentation;

const OBLIQUE: f32 = 0.2126;
const KAPPA: f32 = 0.552_284_8;

fn pt(emu: i64) -> f32 { emu as f32 / EMU_PER_PT as f32 }

struct PdfLink { bounds: Rect, uri: Vec<u8> }

fn pdf_uri(url: &str) -> Vec<u8> {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut uri = Vec::with_capacity(url.len());
    for byte in url.bytes() {
        if byte.is_ascii() { uri.push(byte); } else {
            uri.extend_from_slice(&[b'%', HEX[(byte >> 4) as usize], HEX[(byte & 0x0f) as usize]]);
        }
    }
    uri
}

/// Validated, page-clipped annotations in the scene's page order and input link order.
fn pdf_links(scene: &RenderScene) -> DltResult<Vec<Vec<PdfLink>>> {
    let mut links: Vec<Vec<PdfLink>> = (0..scene.pages.len()).map(|_| Vec::new()).collect();
    if scene.links.is_empty() { return Ok(links); }
    let mut page_indices = BTreeMap::new();
    for (index, page) in scene.pages.iter().enumerate() {
        if page.size.width <= 0 || page.size.height <= 0 {
            return Err(DltError::invalid(format!("render page {} has a non-positive size", page.id)));
        }
        if page_indices.insert(page.id.as_str(), index).is_some() {
            return Err(DltError::invalid(format!("render scene has duplicate page id {}", page.id)));
        }
    }
    for link in &scene.links {
        let Some(&page_index) = page_indices.get(link.page_id.as_str()) else { continue };
        link.check()?;
        let r = link.bounds;
        let (right, bottom) = (r.x + r.width, r.y + r.height);
        if r.width == 0 || r.height == 0 { continue; }
        let page = &scene.pages[page_index];
        let (left, top) = (r.x.max(0), r.y.max(0));
        let (right, bottom) = (right.min(page.size.width), bottom.min(page.size.height));
        if right <= left || bottom <= top { continue; }
        links[page_index].push(PdfLink {
            bounds: Rect { x: left, y: top, width: right - left, height: bottom - top },
            uri: pdf_uri(&link.url),
        });
    }
    Ok(links)
}

struct FontEmbed { face: Arc<Face>, name: String, type0: Ref, glyphs: BTreeMap<u16, String>, remap: subsetter::GlyphRemapper }

struct ImageEmbed { name: String, id: Ref, pixels: (u32, u32) }

fn deflate(data: &[u8]) -> DltResult<Vec<u8>> {
    let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    z.write_all(data).map_err(|e| DltError::new(ErrorCode::Internal, e.to_string()))?;
    z.finish().map_err(|e| DltError::new(ErrorCode::Internal, e.to_string()))
}

/// (text of each glyph for ToUnicode, clusters that need ActualText) for one Text primitive.
fn cluster_text(text: &str, shaped: &ShapedText) -> Vec<(usize, String, bool)> {
    let bounds: Vec<usize> = text.grapheme_indices(true).map(|(i, _)| i).chain(std::iter::once(text.len())).collect();
    let slice = |a: u32, b: u32| -> String {
        let a = bounds.get(a as usize).copied().unwrap_or(text.len());
        let b = bounds.get(b as usize).copied().unwrap_or(text.len()).max(a);
        text[a..b].to_string()
    };
    let mut out = Vec::new();
    let mut i = 0;
    while i < shaped.glyphs.len() {
        let g = &shaped.glyphs[i];
        let mut j = i + 1;
        while j < shaped.glyphs.len() && shaped.glyphs[j].grapheme_start == g.grapheme_start && shaped.glyphs[j].grapheme_end == g.grapheme_end { j += 1; }
        let t = slice(g.grapheme_start, g.grapheme_end);
        let chars: Vec<char> = t.chars().collect();
        let count = j - i;
        if count == 1 {
            out.push((i, t, false));
        } else if count == chars.len() {
            for (k, c) in chars.iter().enumerate() { out.push((i + k, c.to_string(), false)); }
        } else {
            out.push((i, t, true));
            for k in 1..count { out.push((i + k, String::new(), true)); }
        }
        i = j;
    }
    out
}

pub(crate) fn encode_pdf(glyphs: &GlyphSource<'_>, scene: &RenderScene, assets: &dyn AssetStore, diags: &mut Diags) -> DltResult<Vec<u8>> {
    let links_by_page = pdf_links(scene)?;
    let mut next = 1;
    let mut alloc = || { let r = Ref::new(next); next += 1; r };
    let catalog = alloc();
    let tree = alloc();
    let info = alloc();
    let page_refs: Vec<(Ref, Ref)> = scene.pages.iter().map(|_| (alloc(), alloc())).collect();

    // Fonts: glyph usage and Unicode text per glyph; conflicts force ActualText.
    let mut fonts: BTreeMap<String, FontEmbed> = BTreeMap::new();
    let mut actual: BTreeSet<(usize, usize, usize)> = BTreeSet::new();
    let mut outlined: BTreeSet<String> = BTreeSet::new();
    for (pi, page) in scene.pages.iter().enumerate() {
        for (qi, primitive) in page.primitives.iter().enumerate() {
            let Primitive::Text { text, shaped, .. } = primitive else { continue };
            for (gi, unicode, needs_actual) in cluster_text(text, shaped) {
                let g = &shaped.glyphs[gi];
                let face = glyphs.face(shaped, &g.font_key)?;
                if !face.embeddable {
                    outlined.insert(face.postscript_name.clone());
                    continue;
                }
                let count = fonts.len();
                let entry = fonts.entry(face.key.clone()).or_insert_with(|| FontEmbed { face: face.clone(), name: format!("F{count}"), type0: Ref::new(1), glyphs: BTreeMap::new(), remap: subsetter::GlyphRemapper::new() });
                let gid = g.glyph_id as u16;
                if needs_actual { actual.insert((pi, qi, gi)); }
                match entry.glyphs.get(&gid) {
                    None => { entry.glyphs.insert(gid, unicode); }
                    Some(existing) if existing.is_empty() && !unicode.is_empty() => { entry.glyphs.insert(gid, unicode); }
                    Some(existing) if !unicode.is_empty() && *existing != unicode => { actual.insert((pi, qi, gi)); }
                    _ => {}
                }
            }
        }
    }
    if !outlined.is_empty() {
        diags.add("font_embedding_restricted", format!("Fonts {} forbid embedding; their text is drawn as outlines and cannot be extracted", outlined.into_iter().collect::<Vec<_>>().join(", ")), None, true);
    }
    let mut pdf = Pdf::new();
    for font in fonts.values_mut() {
        font.type0 = alloc();
        for gid in font.glyphs.keys() { font.remap.remap(*gid); }
        let face = &font.face;
        let subset = subsetter::subset(face.data, face.index, &font.remap).map_err(|e| DltError::new(ErrorCode::Internal, format!("Font subset of {} failed: {e:?}", face.postscript_name)))?;
        let tag_src = sha256(format!("{}{:?}", face.key, font.glyphs.keys().collect::<Vec<_>>()).as_bytes());
        let tag: String = tag_src.bytes().take(6).map(|b| (b'A' + (b % 26)) as char).collect();
        let base = format!("{tag}+{}", face.postscript_name.replace(' ', ""));
        let (cid, desc, cmap_id, file) = (alloc(), alloc(), alloc(), alloc());
        pdf.type0_font(font.type0).base_font(Name(base.as_bytes())).encoding_predefined(Name(b"Identity-H")).descendant_font(cid).to_unicode(cmap_id);
        let upem = f32::from(face.units_per_em);
        {
            let mut c = pdf.cid_font(cid);
            c.subtype(if face.is_cff { CidFontType::Type0 } else { CidFontType::Type2 })
                .base_font(Name(base.as_bytes()))
                .system_info(SystemInfo { registry: Str(b"Adobe"), ordering: Str(b"Identity"), supplement: 0 })
                .font_descriptor(desc)
                .default_width(0.0);
            if !face.is_cff { c.cid_to_gid_map_predefined(Name(b"Identity")); }
            let mut w = c.widths();
            for old in font.glyphs.keys() {
                let new = font.remap.get(*old).unwrap_or(0);
                let adv = f32::from(face.ttf.glyph_hor_advance(ttf_parser::GlyphId(*old)).unwrap_or(0));
                w.consecutive(new, [adv * 1000.0 / upem]);
            }
            w.finish();
        }
        let s1000 = |v: i16| f32::from(v) * 1000.0 / upem;
        let mut fd = pdf.font_descriptor(desc);
        fd.name(Name(base.as_bytes()))
            .flags(FontFlags::NON_SYMBOLIC)
            .bbox(PdfRect::new(s1000(face.bbox[0]), s1000(face.bbox[1]), s1000(face.bbox[2]), s1000(face.bbox[3])))
            .italic_angle(0.0)
            .ascent(s1000(face.ascender))
            .descent(s1000(face.descender))
            .cap_height(s1000(face.cap_height))
            .stem_v(80.0);
        if face.is_cff { fd.font_file3(file); } else { fd.font_file2(file); }
        fd.finish();
        let mut cmap = UnicodeCmap::new(Name(b"Custom"), SystemInfo { registry: Str(b"Adobe"), ordering: Str(b"UCS"), supplement: 0 });
        for (old, text) in &font.glyphs {
            if !text.is_empty() { cmap.pair_with_multiple(font.remap.get(*old).unwrap_or(0), text.chars()); }
        }
        pdf.stream(cmap_id, &cmap.finish());
        let program = if face.is_cff { cff_table(&subset)? } else { subset };
        let compressed = deflate(&program)?;
        let mut stream = pdf.stream(file, &compressed);
        stream.filter(Filter::FlateDecode);
        if face.is_cff { stream.pair(Name(b"Subtype"), Name(b"CIDFontType0C")); } else { stream.pair(Name(b"Length1"), program.len() as i32); }
        stream.finish();
    }

    // Images, shared across pages by asset hash.
    let mut images: BTreeMap<String, Option<ImageEmbed>> = BTreeMap::new();
    let mut crop_blocked_nodes = BTreeSet::new();
    for page in &scene.pages {
        for primitive in &page.primitives {
            let Primitive::Image { node_id, asset, image_crop_pixel_rect, .. } = primitive else { continue };
            if image_crop_pixel_rect.is_some() {
                match assets.read(asset).and_then(|bytes| {
                    if sniff(&bytes) == ImageKind::Jpeg {
                        validate_jpeg_crop_orientation(&bytes)?;
                        if let Some((header_width, header_height, _)) = jpeg_info(&bytes) {
                            let decoded = decode(&bytes)?;
                            if (decoded.width, decoded.height) != (header_width, header_height) {
                                return Err(DltError::invalid("image crop source dimensions differ from decoded JPEG dimensions"));
                            }
                        }
                    }
                    Ok(())
                }) {
                    Ok(()) => {}
                    Err(error) if crop_diagnostic_code(&error).is_some() => {
                        let code = crop_diagnostic_code(&error).unwrap();
                        diags.add(code, format!("Image {node_id} is shown as a placeholder: {}", error.message), Some(node_id), true);
                        crop_blocked_nodes.insert(node_id.clone());
                    }
                    Err(_) => {}
                }
            }
            if images.contains_key(&asset.sha256) { continue; }
            let embed = (|| -> DltResult<(ImageEmbed, Vec<String>)> {
                let bytes = assets.read(asset)?;
                let mut warnings = Vec::new();
                let id = alloc();
                let name = format!("Im{}", images.len());
                let pixels;
                match (sniff(&bytes), jpeg_info(&bytes)) {
                    (ImageKind::Jpeg, Some((w, h, comps))) if comps == 1 || comps == 3 => {
                        decode(&bytes)?;
                        pixels = (w, h);
                        let mut x = pdf.image_xobject(id, &bytes);
                        x.width(w as i32).height(h as i32).bits_per_component(8);
                        x.filter(Filter::DctDecode);
                        if comps == 1 { x.color_space().device_gray(); } else { x.color_space().device_rgb(); }
                        x.finish();
                    }
                    _ => {
                        let img = decode(&bytes)?;
                        pixels = (img.width, img.height);
                        warnings = img.warnings.clone();
                        let rgb: Vec<u8> = img.rgba.chunks_exact(4).flat_map(|p| [p[0], p[1], p[2]]).collect();
                        let data = deflate(&rgb)?;
                        let mask = if img.has_alpha() {
                            let alpha: Vec<u8> = img.rgba.chunks_exact(4).map(|p| p[3]).collect();
                            let mid = alloc();
                            let adata = deflate(&alpha)?;
                            let mut m = pdf.image_xobject(mid, &adata);
                            m.width(img.width as i32).height(img.height as i32).bits_per_component(8);
                            m.filter(Filter::FlateDecode);
                            m.color_space().device_gray();
                            m.finish();
                            Some(mid)
                        } else { None };
                        let mut x = pdf.image_xobject(id, &data);
                        x.width(img.width as i32).height(img.height as i32).bits_per_component(8);
                        x.filter(Filter::FlateDecode);
                        x.color_space().device_rgb();
                        if let Some(m) = mask { x.s_mask(m); }
                        x.finish();
                    }
                }
                Ok((ImageEmbed { name, id, pixels }, warnings))
            })();
            match embed {
                Ok((e, warnings)) => {
                    for w in warnings { diags.add("image_checksum_mismatch", format!("Image {node_id} is damaged and was decoded in degraded mode: {w}"), Some(node_id), true); }
                    images.insert(asset.sha256.clone(), Some(e));
                }
                Err(e) => {
                    diags.add("image_not_rendered", format!("Image {node_id} is shown as a placeholder: {}", e.message), Some(node_id), true);
                    images.insert(asset.sha256.clone(), None);
                }
            }
        }
    }

    // Alpha graphics states.
    let mut alphas: BTreeMap<u8, (String, Ref)> = BTreeMap::new();
    let mut alpha_name = |a: u8, alloc: &mut dyn FnMut() -> Ref| -> String {
        let n = alphas.len();
        alphas.entry(a).or_insert_with(|| (format!("A{n}"), alloc())).0.clone()
    };

    let mut page_fonts: Vec<BTreeSet<String>> = vec![BTreeSet::new(); scene.pages.len()];
    let mut page_images: Vec<BTreeSet<String>> = vec![BTreeSet::new(); scene.pages.len()];
    let mut page_alphas: Vec<BTreeSet<u8>> = vec![BTreeSet::new(); scene.pages.len()];
    // v1.2: graphics states (fill alpha, stroke alpha), shadings and shadow images per page.
    let mut gstates: BTreeMap<(u8, u8), (String, Ref)> = BTreeMap::new();
    let mut fx_counter = 0usize;
    let mut page_gstates: Vec<BTreeSet<(u8, u8)>> = vec![BTreeSet::new(); scene.pages.len()];
    let mut page_shadings: Vec<Vec<(String, Ref)>> = vec![Vec::new(); scene.pages.len()];
    let mut page_xobjects: Vec<Vec<(String, Ref)>> = vec![Vec::new(); scene.pages.len()];
    let mut contents: Vec<Vec<u8>> = Vec::with_capacity(scene.pages.len());
    for (pi, page) in scene.pages.iter().enumerate() {
        let h = pt(page.size.height);
        let mut c = Content::new();
        for (qi, primitive) in page.primitives.iter().enumerate() {
            match primitive {
                Primitive::Path { node_id, bounds, path, style } => {
                    let mut fx = Fx { pdf: &mut pdf, alloc: &mut alloc, gstates: &mut gstates, page_gstates: &mut page_gstates[pi], shadings: &mut page_shadings[pi], xobjects: &mut page_xobjects[pi], counter: &mut fx_counter, page };
                    fx.path(&mut c, diags, node_id, *bounds, path, style);
                }
                Primitive::Shape { node_id, bounds, shape, style } if crate::v12::has_effects(style) => {
                    let kind = ShapeKind::parse(shape);
                    if kind == ShapeKind::Unknown { diags.add("shape_approximated", format!("Shape '{shape}' is drawn as its bounding rectangle"), Some(node_id), false); }
                    let mut fx = Fx { pdf: &mut pdf, alloc: &mut alloc, gstates: &mut gstates, page_gstates: &mut page_gstates[pi], shadings: &mut page_shadings[pi], xobjects: &mut page_xobjects[pi], counter: &mut fx_counter, page };
                    fx.shape(&mut c, diags, node_id, *bounds, kind, style)?;
                }
                Primitive::Line { node_id, from, to, style, arrow } if crate::v12::has_effects(style) => {
                    let mut fx = Fx { pdf: &mut pdf, alloc: &mut alloc, gstates: &mut gstates, page_gstates: &mut page_gstates[pi], shadings: &mut page_shadings[pi], xobjects: &mut page_xobjects[pi], counter: &mut fx_counter, page };
                    fx.line(&mut c, diags, node_id, *from, *to, style, *arrow);
                }
                Primitive::Shape { node_id, bounds, shape, style } => {
                    let kind = ShapeKind::parse(shape);
                    if kind == ShapeKind::Unknown { diags.add("shape_approximated", format!("Shape '{shape}' is drawn as its bounding rectangle"), Some(node_id), false); }
                    let fill = diags.color(&style.fill, node_id);
                    let stroke = if style.stroke_width > 0 { diags.color(&style.stroke, node_id) } else { None };
                    if fill.is_none() && stroke.is_none() { continue; }
                    c.save_state();
                    if let Some(f) = fill {
                        c.set_fill_rgb(f32::from(f.r) / 255.0, f32::from(f.g) / 255.0, f32::from(f.b) / 255.0);
                        if f.a < 255 { let n = alpha_name(f.a, &mut alloc); page_alphas[pi].insert(f.a); c.set_parameters(Name(n.as_bytes())); }
                    }
                    if let Some(s) = stroke {
                        c.set_stroke_rgb(f32::from(s.r) / 255.0, f32::from(s.g) / 255.0, f32::from(s.b) / 255.0);
                        c.set_line_width(pt(style.stroke_width));
                    }
                    let (x, y, w, hh) = (pt(bounds.x), h - pt(bounds.y + bounds.height), pt(bounds.width), pt(bounds.height));
                    match kind {
                        ShapeKind::Ellipse => ellipse(&mut c, x, y, w, hh),
                        ShapeKind::RoundRect => round_rect(&mut c, x, y, w, hh),
                        _ => { c.rect(x, y, w, hh); }
                    }
                    match (fill.is_some(), stroke.is_some()) { (true, true) => { c.fill_nonzero_and_stroke(); } (true, false) => { c.fill_nonzero(); } _ => { c.stroke(); } }
                    c.restore_state();
                }
                Primitive::Line { node_id, from, to, style, arrow } => {
                    let Some(s) = diags.color(&style.stroke, node_id) else { continue };
                    c.save_state();
                    c.set_stroke_rgb(f32::from(s.r) / 255.0, f32::from(s.g) / 255.0, f32::from(s.b) / 255.0);
                    c.set_line_width(pt(style.stroke_width.max(1)));
                    c.move_to(pt(from.x), h - pt(from.y));
                    c.line_to(pt(to.x), h - pt(to.y));
                    c.stroke();
                    if *arrow {
                        if let Some(head) = arrow_head(*from, *to, style.stroke_width) {
                            c.set_fill_rgb(f32::from(s.r) / 255.0, f32::from(s.g) / 255.0, f32::from(s.b) / 255.0);
                            c.move_to(pt(head[0].x), h - pt(head[0].y));
                            c.line_to(pt(head[1].x), h - pt(head[1].y));
                            c.line_to(pt(head[2].x), h - pt(head[2].y));
                            c.close_path();
                            c.fill_nonzero();
                        }
                    }
                    c.restore_state();
                }
                Primitive::Image { node_id, bounds, asset, fit, image_crop_pixel_rect, style }
                    if fit.is_some() || image_crop_pixel_rect.is_some() || style.is_some() => {
                    let crop_blocked = crop_blocked_nodes.contains(node_id);
                    let embed = (!crop_blocked).then(|| images.get(&asset.sha256).and_then(|e| e.as_ref()).map(|e| (e.name.clone(), e.pixels))).flatten();
                    if embed.is_some() { page_images[pi].insert(asset.sha256.clone()); }
                    let mut fx = Fx { pdf: &mut pdf, alloc: &mut alloc, gstates: &mut gstates, page_gstates: &mut page_gstates[pi], shadings: &mut page_shadings[pi], xobjects: &mut page_xobjects[pi], counter: &mut fx_counter, page };
                    let crop = if crop_blocked { None } else { *image_crop_pixel_rect };
                    fx.image(&mut c, diags, node_id, *bounds, *fit, style.as_ref(), embed, crop)?;
                }
                Primitive::Image { bounds, asset, .. } => {
                    let (x, y, w, hh) = (pt(bounds.x), h - pt(bounds.y + bounds.height), pt(bounds.width), pt(bounds.height));
                    c.save_state();
                    match images.get(&asset.sha256).and_then(|e| e.as_ref()) {
                        Some(e) => {
                            page_images[pi].insert(asset.sha256.clone());
                            c.transform([w, 0.0, 0.0, hh, x, y]);
                            c.x_object(Name(e.name.as_bytes()));
                        }
                        None => {
                            c.set_fill_rgb(0.898, 0.906, 0.922);
                            c.rect(x, y, w, hh);
                            c.fill_nonzero();
                        }
                    }
                    c.restore_state();
                }
                Primitive::Text { node_id, origin, text, style, shaped, .. } => {
                    let Some(color) = diags.color(&style.color, node_id) else { continue };
                    // v1.2 opacity: a graphics state around the whole text (v1.1 text keeps its exact output).
                    let fade = style.opacity_milli.is_some();
                    if fade {
                        c.save_state();
                        let a = faded(color, effects::opacity(style)).a;
                        let mut fx = Fx { pdf: &mut pdf, alloc: &mut alloc, gstates: &mut gstates, page_gstates: &mut page_gstates[pi], shadings: &mut page_shadings[pi], xobjects: &mut page_xobjects[pi], counter: &mut fx_counter, page };
                        fx.alpha(&mut c, a, a);
                    }
                    let size = pt(style.font_size);
                    let (r, g, b) = (f32::from(color.r) / 255.0, f32::from(color.g) / 255.0, f32::from(color.b) / 255.0);
                    let units: Vec<(usize, String, bool)> = cluster_text(text, shaped);
                    let starts_actual: BTreeMap<usize, String> = units.iter().filter(|(gi, t, needs)| (*needs && !t.is_empty()) || actual.contains(&(pi, qi, *gi))).map(|(gi, t, _)| {
                        let g = &shaped.glyphs[*gi];
                        let full = cluster_text_span(text, g.grapheme_start, g.grapheme_end);
                        (*gi, if t.is_empty() { full } else { t.clone() })
                    }).collect();
                    // Outline fallback for faces that cannot be embedded.
                    for gm in &shaped.glyphs {
                        let face = glyphs.face(shaped, &gm.font_key)?;
                        if face.embeddable { continue; }
                        let s = size / f32::from(face.units_per_em);
                        let skew = if style.italic && !face.italic { s * OBLIQUE } else { 0.0 };
                        c.save_state();
                        c.set_fill_rgb(r, g, b);
                        c.transform([s, 0.0, skew, s, pt(origin.x + gm.position.x), h - pt(origin.y + gm.position.y)]);
                        let mut ob = PdfOutline { c: &mut c, last: (0.0, 0.0), any: false };
                        face.ttf.outline_glyph(ttf_parser::GlyphId(gm.glyph_id as u16), &mut ob);
                        if ob.any { c.fill_nonzero(); }
                        c.restore_state();
                    }
                    let embedded: Vec<&GlyphMetric> = shaped.glyphs.iter().filter(|g| fonts.contains_key(&g.font_key)).collect();
                    if !embedded.is_empty() {
                        c.begin_text();
                        c.set_fill_rgb(r, g, b);
                        let mut open_span = false;
                        let mut current: Option<String> = None;
                        let mut bold_mode = false;
                        for (gi, gm) in shaped.glyphs.iter().enumerate() {
                            let Some(font) = fonts.get(&gm.font_key) else { continue };
                            let first_of_cluster = gi == 0 || shaped.glyphs[gi - 1].grapheme_start != gm.grapheme_start || shaped.glyphs[gi - 1].grapheme_end != gm.grapheme_end;
                            if first_of_cluster {
                                if open_span { c.end_marked_content(); open_span = false; }
                                if let Some(t) = starts_actual.get(&gi) {
                                    c.begin_marked_content_with_properties(Name(b"Span")).properties().actual_text(TextStr(t));
                                    open_span = true;
                                }
                            }
                            if current.as_deref() != Some(font.name.as_str()) {
                                c.set_font(Name(font.name.as_bytes()), size);
                                current = Some(font.name.clone());
                            }
                            let want_bold = crate::synthetic_bold(style, font.face.weight);
                            if want_bold != bold_mode {
                                if want_bold {
                                    c.set_text_rendering_mode(TextRenderingMode::FillStroke);
                                    c.set_stroke_rgb(r, g, b);
                                    c.set_line_width(size * 0.03);
                                } else {
                                    c.set_text_rendering_mode(TextRenderingMode::Fill);
                                }
                                bold_mode = want_bold;
                            }
                            let skew = if style.italic && !font.face.italic { OBLIQUE } else { 0.0 };
                            c.set_text_matrix([1.0, 0.0, skew, 1.0, pt(origin.x + gm.position.x), h - pt(origin.y + gm.position.y)]);
                            let new = font.remap.get(gm.glyph_id as u16).unwrap_or(0);
                            c.show(Str(&new.to_be_bytes()));
                            page_fonts[pi].insert(gm.font_key.clone());
                        }
                        if open_span { c.end_marked_content(); }
                        if bold_mode { c.set_text_rendering_mode(TextRenderingMode::Fill); }
                        c.end_text();
                    }
                    for (from, to, width) in crate::shape::text_decorations(glyphs, *origin, style, shaped)? {
                        c.set_stroke_rgb(r, g, b);
                        c.set_line_width(pt(width));
                        c.move_to(pt(from.x), h - pt(from.y));
                        c.line_to(pt(to.x), h - pt(to.y));
                        c.stroke();
                    }
                    if fade { c.restore_state(); }
                }
            }
        }
        contents.push(c.finish());
    }
    for (a, (_, id)) in &alphas {
        let v = f32::from(*a) / 255.0;
        pdf.ext_graphics(*id).non_stroking_alpha(v).stroking_alpha(v);
    }
    for ((fa, sa), (_, id)) in &gstates { pdf.ext_graphics(*id).non_stroking_alpha(f32::from(*fa) / 255.0).stroking_alpha(f32::from(*sa) / 255.0); }
    pdf.catalog(catalog).pages(tree);
    pdf.pages(tree).kids(page_refs.iter().map(|p| p.0)).count(scene.pages.len() as i32);
    for (pi, page) in scene.pages.iter().enumerate() {
        let (page_id, content_id) = page_refs[pi];
        let link_refs: Vec<Ref> = links_by_page[pi].iter().map(|_| alloc()).collect();
        let mut p = pdf.page(page_id);
        let media = PdfRect::new(0.0, 0.0, pt(page.size.width), pt(page.size.height));
        p.media_box(media).crop_box(media).parent(tree).contents(content_id);
        if !link_refs.is_empty() { p.annotations(link_refs.iter().copied()); }
        let mut res = p.resources();
        {
            let mut fd = res.fonts();
            for key in &page_fonts[pi] { let f = &fonts[key]; fd.pair(Name(f.name.as_bytes()), f.type0); }
            fd.finish();
        }
        {
            let mut xd = res.x_objects();
            for sha in &page_images[pi] { if let Some(Some(e)) = images.get(sha) { xd.pair(Name(e.name.as_bytes()), e.id); } }
            for (n, id) in &page_xobjects[pi] { xd.pair(Name(n.as_bytes()), *id); }
            xd.finish();
        }
        {
            let mut gd = res.ext_g_states();
            for a in &page_alphas[pi] { let (n, id) = &alphas[a]; gd.pair(Name(n.as_bytes()), *id); }
            for k in &page_gstates[pi] { let (n, id) = &gstates[k]; gd.pair(Name(n.as_bytes()), *id); }
            gd.finish();
        }
        if !page_shadings[pi].is_empty() {
            let mut sd = res.shadings();
            for (n, id) in &page_shadings[pi] { sd.pair(Name(n.as_bytes()), *id); }
            sd.finish();
        }
        res.finish();
        p.finish();
        pdf.stream(content_id, &contents[pi]);
        for (link, id) in links_by_page[pi].iter().zip(link_refs) {
            let right = link.bounds.x + link.bounds.width;
            let bottom = link.bounds.y + link.bounds.height;
            let rect = PdfRect::new(pt(link.bounds.x), pt(page.size.height - bottom), pt(right), pt(page.size.height - link.bounds.y));
            pdf.annotation(id)
                .subtype(AnnotationType::Link)
                .rect(rect)
                .border(0.0, 0.0, 0.0, None)
                .action()
                .action_type(ActionType::Uri)
                .uri(Str(&link.uri));
        }
    }
    pdf.document_info(info).producer(TextStr("DesignLLMTool designllm-render"));
    Ok(pdf.finish())
}

fn cluster_text_span(text: &str, start: u32, end: u32) -> String {
    let bounds: Vec<usize> = text.grapheme_indices(true).map(|(i, _)| i).chain(std::iter::once(text.len())).collect();
    let a = bounds.get(start as usize).copied().unwrap_or(text.len());
    let b = bounds.get(end as usize).copied().unwrap_or(text.len()).max(a);
    text[a..b].to_string()
}

struct PdfOutline<'c> { c: &'c mut Content, last: (f32, f32), any: bool }
impl ttf_parser::OutlineBuilder for PdfOutline<'_> {
    fn move_to(&mut self, x: f32, y: f32) { self.c.move_to(x, y); self.last = (x, y); self.any = true; }
    fn line_to(&mut self, x: f32, y: f32) { self.c.line_to(x, y); self.last = (x, y); }
    fn quad_to(&mut self, x1: f32, y1: f32, x: f32, y: f32) {
        // Exact degree elevation of the quadratic segment.
        let (x0, y0) = self.last;
        let c1 = (x0 + 2.0 / 3.0 * (x1 - x0), y0 + 2.0 / 3.0 * (y1 - y0));
        let c2 = (x + 2.0 / 3.0 * (x1 - x), y + 2.0 / 3.0 * (y1 - y));
        self.c.cubic_to(c1.0, c1.1, c2.0, c2.1, x, y);
        self.last = (x, y);
    }
    fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x: f32, y: f32) { self.c.cubic_to(x1, y1, x2, y2, x, y); self.last = (x, y); }
    fn close(&mut self) { self.c.close_path(); }
}

/// Standalone CFF program of a subset OpenType font (FontFile3 /CIDFontType0C).
fn cff_table(font: &[u8]) -> DltResult<Vec<u8>> {
    let raw = ttf_parser::RawFace::parse(font, 0).map_err(|e| DltError::new(ErrorCode::Internal, format!("Subset font is not readable: {e}")))?;
    raw.table(ttf_parser::Tag::from_bytes(b"CFF ")).map(|t| t.to_vec()).ok_or_else(|| DltError::new(ErrorCode::Internal, "Subset CFF font has no CFF table"))
}

fn ellipse(c: &mut Content, x: f32, y: f32, w: f32, h: f32) {
    let (rx, ry) = (w / 2.0, h / 2.0);
    let (cx, cy) = (x + rx, y + ry);
    let (ox, oy) = (rx * KAPPA, ry * KAPPA);
    c.move_to(cx + rx, cy);
    c.cubic_to(cx + rx, cy + oy, cx + ox, cy + ry, cx, cy + ry);
    c.cubic_to(cx - ox, cy + ry, cx - rx, cy + oy, cx - rx, cy);
    c.cubic_to(cx - rx, cy - oy, cx - ox, cy - ry, cx, cy - ry);
    c.cubic_to(cx + ox, cy - ry, cx + rx, cy - oy, cx + rx, cy);
    c.close_path();
}

fn round_rect(c: &mut Content, x: f32, y: f32, w: f32, h: f32) {
    let r = w.min(h) * 0.1;
    let k = KAPPA * r;
    c.move_to(x + r, y);
    c.line_to(x + w - r, y);
    c.cubic_to(x + w - r + k, y, x + w, y + r - k, x + w, y + r);
    c.line_to(x + w, y + h - r);
    c.cubic_to(x + w, y + h - r + k, x + w - r + k, y + h, x + w - r, y + h);
    c.line_to(x + r, y + h);
    c.cubic_to(x + r - k, y + h, x, y + h - r + k, x, y + h - r);
    c.line_to(x, y + r);
    c.cubic_to(x, y + r - k, x + r - k, y, x + r, y);
    c.close_path();
}

// ---------------------------------------------------------------- contract v1.2 drawing (P1)

/// Shadow raster resolution in PDF (shadows only, as an SMask image).
pub const PDF_SHADOW_DPI: u32 = 144;

/// Per-page state of the v1.2 drawing: graphics states (fill alpha, stroke alpha), shadings and
/// shadow images, and the shared object allocator.
pub(crate) struct Fx<'a> {
    pub pdf: &'a mut Pdf,
    pub alloc: &'a mut dyn FnMut() -> Ref,
    pub gstates: &'a mut BTreeMap<(u8, u8), (String, Ref)>,
    pub page_gstates: &'a mut BTreeSet<(u8, u8)>,
    pub shadings: &'a mut Vec<(String, Ref)>,
    pub xobjects: &'a mut Vec<(String, Ref)>,
    pub counter: &'a mut usize,
    pub page: &'a RenderPage,
}

fn rgb(c: Rgba) -> [f32; 3] { [f32::from(c.r) / 255.0, f32::from(c.g) / 255.0, f32::from(c.b) / 255.0] }

impl Fx<'_> {
    fn h(&self) -> f32 { pt(self.page.size.height) }
    fn name(&mut self, prefix: &str) -> String { let n = format!("{prefix}{}", *self.counter); *self.counter += 1; n }

    /// Constant alpha for fill (`ca`) and stroke (`CA`); nothing is set when both are opaque.
    fn alpha(&mut self, c: &mut Content, fill: u8, stroke: u8) {
        if fill == 255 && stroke == 255 { return; }
        let n = self.gstates.len();
        let alloc = &mut *self.alloc;
        let name = self.gstates.entry((fill, stroke)).or_insert_with(|| (format!("G{n}"), alloc())).0.clone();
        self.page_gstates.insert((fill, stroke));
        c.set_parameters(Name(name.as_bytes()));
    }

    fn stroke_style(&self, c: &mut Content, style: &Style, width: i64) {
        c.set_line_width(pt(width));
        c.set_line_cap(match style.stroke_cap { Some(StrokeCap::Round) => LineCapStyle::RoundCap, Some(StrokeCap::Square) => LineCapStyle::ProjectingSquareCap, _ => LineCapStyle::ButtCap });
        c.set_line_join(match style.stroke_join { Some(StrokeJoin::Round) => LineJoinStyle::RoundJoin, Some(StrokeJoin::Bevel) => LineJoinStyle::BevelJoin, _ => LineJoinStyle::MiterJoin });
        // Miter joins stop at 4 half widths.
        // v1.2 strokes only: v1.1 output keeps its bytes.
        if matches!(style.stroke_join, Some(StrokeJoin::Miter) | None) { c.set_miter_limit(4.0); }
        if let Some((d, g)) = effects::dash_pattern(style.stroke_dash, width) { c.set_dash_pattern([pt(d), pt(g)], 0.0); }
    }

    /// Axial shading of a two-stop gradient on `bounds` (CSS angle; the stops' positions become the
    /// shading's end points, extended on both sides).
    fn shading(&mut self, g: &Gradient, bounds: Rect) -> Option<String> {
        let colors: Vec<Rgba> = g.stops.iter().filter_map(|s| parse_color(&s.color).ok().flatten()).collect();
        if colors.len() != 2 { return None; }
        let (a, b) = effects::gradient_line(bounds, g.angle_millideg);
        let at = |t: f64| (a.x as f64 + (b.x - a.x) as f64 * t, a.y as f64 + (b.y - a.y) as f64 * t);
        let (t0, t1) = (f64::from(g.stops[0].pos_milli.min(1000)) / 1000.0, f64::from(g.stops[1].pos_milli.min(1000)) / 1000.0);
        let ((x0, y0), (x1, y1)) = (at(t0), at(t1.max(t0 + 1e-6)));
        let h = self.h();
        let (f, sh) = ((self.alloc)(), (self.alloc)());
        self.pdf.exponential_function(f).domain([0.0, 1.0]).c0(rgb(colors[0])).c1(rgb(colors[1])).n(1.0);
        let mut s = self.pdf.function_shading(sh);
        s.shading_type(FunctionShadingType::Axial);
        s.color_space().device_rgb();
        s.function(f);
        let e = EMU_PER_PT as f64;
        s.coords([(x0 / e) as f32, h - (y0 / e) as f32, (x1 / e) as f32, h - (y1 / e) as f32]);
        s.extend([true, true]);
        s.finish();
        let name = self.name("Sh");
        self.shadings.push((name.clone(), sh));
        Some(name)
    }

    /// Paints the current path: gradient (clip + shading) or solid fill, then the stroke.
    #[allow(clippy::too_many_arguments)]
    fn paint(&mut self, c: &mut Content, diags: &mut Diags, node: &str, style: &Style, bounds: Rect, build: &dyn Fn(&mut Content), even_odd: bool, stroke_width: i64) {
        let op = effects::opacity(style);
        let stroke = if stroke_width > 0 { diags.color(&style.stroke, node).map(|s| faded(s, op)) } else { None };
        if let Some(g) = &style.gradient {
            if let Some(name) = self.shading(g, bounds) {
                c.save_state();
                build(c);
                if even_odd { c.clip_even_odd(); } else { c.clip_nonzero(); }
                c.end_path();
                self.alpha(c, (op * 255.0 + 0.5).floor() as u8, 255);
                c.shading(Name(name.as_bytes()));
                c.restore_state();
            }
        } else if let Some(f) = diags.color(&style.fill, node).map(|f| faded(f, op)) {
            c.save_state();
            self.alpha(c, f.a, 255);
            let [r, g, b] = rgb(f);
            c.set_fill_rgb(r, g, b);
            build(c);
            if even_odd { c.fill_even_odd(); } else { c.fill_nonzero(); }
            c.restore_state();
        }
        if let Some(s) = stroke {
            c.save_state();
            self.alpha(c, 255, s.a);
            let [r, g, b] = rgb(s);
            c.set_stroke_rgb(r, g, b);
            self.stroke_style(c, style, stroke_width);
            build(c);
            c.stroke();
            c.restore_state();
        }
    }

    /// CSS outer shadows as images (solid colour + SMask) rasterized at PDF_SHADOW_DPI.
    fn shadows(&mut self, c: &mut Content, diags: &mut Diags, node: &str, style: &Style, kind: ShapeKind, bounds: Rect, radius: i64) -> DltResult<()> {
        let k = PDF_SHADOW_DPI as f32 / EMU_PER_INCH as f32;
        let visible = (0.0, 0.0, pixels(self.page.size.width, PDF_SHADOW_DPI)? as f32, pixels(self.page.size.height, PDF_SHADOW_DPI)? as f32);
        for s in &style.shadows {
            let Some(col) = diags.color(&s.color, node) else { continue };
            let col = faded(col, effects::opacity(style));
            let shape = |dx: f32, dy: f32, out: f32| crate::raster::box_path(kind, bounds, radius, k, dx, dy, out);
            let Some((x0, y0, w, hh, alpha)) = effects::shadow_alpha(s, k, visible, &shape, col.a) else { continue };
            let color: Vec<u8> = std::iter::repeat([col.r, col.g, col.b]).take((w * hh) as usize).flatten().collect();
            let (img, mask) = ((self.alloc)(), (self.alloc)());
            let adata = deflate(&alpha)?;
            let mut m = self.pdf.image_xobject(mask, &adata);
            m.width(w as i32).height(hh as i32).bits_per_component(8);
            m.filter(Filter::FlateDecode);
            m.color_space().device_gray();
            m.finish();
            let cdata = deflate(&color)?;
            let mut x = self.pdf.image_xobject(img, &cdata);
            x.width(w as i32).height(hh as i32).bits_per_component(8);
            x.filter(Filter::FlateDecode);
            x.color_space().device_rgb();
            x.s_mask(mask);
            x.finish();
            let name = self.name("Sd");
            self.xobjects.push((name.clone(), img));
            let scale = 72.0 / PDF_SHADOW_DPI as f32;
            c.save_state();
            c.transform([w as f32 * scale, 0.0, 0.0, hh as f32 * scale, x0 as f32 * scale, self.h() - (y0 as f32 + hh as f32) * scale]);
            c.x_object(Name(name.as_bytes()));
            c.restore_state();
            diags.add("shadow_rasterized", format!("The shadow of {node} is an image at {PDF_SHADOW_DPI} dpi in PDF"), Some(node), false);
        }
        Ok(())
    }

    /// P1 image: outer shadows of the bounds (as for shapes), then the picture placed by `fit`
    /// inside a clip of the bounds (rounded by the corner radius; cover overflows into it), with the
    /// opacity as the non-stroking alpha. `embed` (XObject name, pixel size) None is the placeholder.
    #[allow(clippy::too_many_arguments)]
    pub fn image(
        &mut self,
        c: &mut Content,
        diags: &mut Diags,
        node: &str,
        bounds: Rect,
        fit: Option<ImageFit>,
        style: Option<&Style>,
        embed: Option<(String, (u32, u32))>,
        crop: Option<PixelRect>,
    ) -> DltResult<()> {
        let radius = style.map(|s| effects::corner_radius(s, bounds, false)).unwrap_or(0);
        if let Some(s) = style { self.shadows(c, diags, node, s, ShapeKind::Rect, bounds, radius)?; }
        let h = self.h();
        let (x, y, w, hh) = (pt(bounds.x), h - pt(bounds.y + bounds.height), pt(bounds.width), pt(bounds.height));
        let r = pt(radius);
        let outline = |c: &mut Content| if r > 0.0 { round_rect_r(c, x, y, w, hh, r) } else { c.rect(x, y, w, hh); };
        c.save_state();
        if radius > 0 || fit == Some(ImageFit::Cover) { outline(c); c.clip_nonzero(); c.end_path(); }
        let a = (style.map(effects::opacity).unwrap_or(1.0) * 255.0 + 0.5).floor() as u8;
        self.alpha(c, a, a);
        match embed {
            Some((name, pixels)) => {
                if let Some(crop) = crop {
                    let placement = match effects::image_crop_placement(bounds, pixels, crop, fit) {
                        Ok(placement) => placement,
                        Err(error) => {
                            diags.add("IMAGE_CROP_INVALID", format!("Image {node} is shown as a placeholder: {}", error.message), Some(node), true);
                            c.set_fill_rgb(0.898, 0.906, 0.922);
                            outline(c);
                            c.fill_nonzero();
                            c.restore_state();
                            return Ok(());
                        }
                    };
                    let clip = placement.clip;
                    c.rect(pt(clip.x), h - pt(clip.y + clip.height), pt(clip.width), pt(clip.height));
                    c.clip_nonzero();
                    c.end_path();
                    let (image_x, image_y) = (pt_f64(placement.image_x), h - pt_f64(placement.image_y + placement.image_height));
                    c.transform([
                        pt_f64(placement.image_width), 0.0, 0.0, pt_f64(placement.image_height), image_x, image_y,
                    ]);
                    c.x_object(Name(name.as_bytes()));
                    c.restore_state();
                    return Ok(());
                }
                let d = fit.map(|f| effects::image_fit_rect(bounds, pixels, f)).unwrap_or(bounds);
                c.transform([pt(d.width), 0.0, 0.0, pt(d.height), pt(d.x), h - pt(d.y + d.height)]);
                c.x_object(Name(name.as_bytes()));
            }
            None => {
                c.set_fill_rgb(0.898, 0.906, 0.922);
                outline(c);
                c.fill_nonzero();
            }
        }
        c.restore_state();
        Ok(())
    }

    pub fn shape(&mut self, c: &mut Content, diags: &mut Diags, node: &str, bounds: Rect, kind: ShapeKind, style: &Style) -> DltResult<()> {
        let radius = if kind == ShapeKind::Ellipse { 0 } else { effects::corner_radius(style, bounds, kind == ShapeKind::RoundRect) };
        self.shadows(c, diags, node, style, kind, bounds, radius)?;
        let h = self.h();
        let (x, y, w, hh) = (pt(bounds.x), h - pt(bounds.y + bounds.height), pt(bounds.width), pt(bounds.height));
        let r = pt(radius);
        let build = move |c: &mut Content| match kind {
            ShapeKind::Ellipse => ellipse(c, x, y, w, hh),
            _ if r > 0.0 => round_rect_r(c, x, y, w, hh, r),
            _ => { c.rect(x, y, w, hh); }
        };
        self.paint(c, diags, node, style, bounds, &build, false, style.stroke_width);
        Ok(())
    }

    pub fn line(&mut self, c: &mut Content, diags: &mut Diags, node: &str, from: Point, to: Point, style: &Style, arrow: bool) {
        let Some(s) = diags.color(&style.stroke, node).map(|s| faded(s, effects::opacity(style))) else { return };
        let h = self.h();
        let [r, g, b] = rgb(s);
        c.save_state();
        self.alpha(c, s.a, s.a);
        c.set_stroke_rgb(r, g, b);
        self.stroke_style(c, style, style.stroke_width.max(1));
        c.move_to(pt(from.x), h - pt(from.y));
        c.line_to(pt(to.x), h - pt(to.y));
        c.stroke();
        for marker in crate::encode::line_markers(from, to, style, arrow).into_iter().flatten() {
            let [tip, left, right] = marker.points;
            match marker.kind {
                LineEnd::Filled => {
                    c.set_fill_rgb(r, g, b);
                    c.move_to(pt(tip.x), h - pt(tip.y));
                    c.line_to(pt(left.x), h - pt(left.y));
                    c.line_to(pt(right.x), h - pt(right.y));
                    c.close_path();
                    c.fill_nonzero();
                }
                LineEnd::Open => {
                    c.move_to(pt(tip.x), h - pt(tip.y));
                    c.line_to(pt(left.x), h - pt(left.y));
                    c.move_to(pt(tip.x), h - pt(tip.y));
                    c.line_to(pt(right.x), h - pt(right.y));
                    c.stroke();
                }
            }
        }
        c.restore_state();
    }

    pub fn path(&mut self, c: &mut Content, diags: &mut Diags, node: &str, bounds: Rect, path: &PathSpec, style: &Style) {
        let (sx, sy, ox, oy) = effects::path_transform(path, bounds);
        let h = self.h();
        let e = EMU_PER_PT as f64;
        let p = move |x: i64, y: i64| (((ox + x as f64 * sx) / e) as f32, h - ((oy + y as f64 * sy) / e) as f32);
        let commands = path.commands.clone();
        let build = move |c: &mut Content| {
            for cmd in &commands {
                match *cmd {
                    PathCommand::MoveTo { x, y } => { let (a, b) = p(x, y); c.move_to(a, b); }
                    PathCommand::LineTo { x, y } => { let (a, b) = p(x, y); c.line_to(a, b); }
                    PathCommand::CubicTo { x1, y1, x2, y2, x, y } => { let ((a, b), (c2, d), (f, g)) = (p(x1, y1), p(x2, y2), p(x, y)); c.cubic_to(a, b, c2, d, f, g); }
                    PathCommand::Close => { c.close_path(); }
                }
            }
        };
        self.paint(c, diags, node, style, bounds, &build, path.fill_rule == FillRule::Evenodd, style.stroke_width);
    }
}

fn pt_f64(emu: f64) -> f32 { (emu / EMU_PER_PT as f64) as f32 }

fn round_rect_r(c: &mut Content, x: f32, y: f32, w: f32, h: f32, r: f32) {
    let r = r.min(w / 2.0).min(h / 2.0);
    let k = KAPPA * r;
    c.move_to(x + r, y);
    c.line_to(x + w - r, y);
    c.cubic_to(x + w - r + k, y, x + w, y + r - k, x + w, y + r);
    c.line_to(x + w, y + h - r);
    c.cubic_to(x + w, y + h - r + k, x + w - r + k, y + h, x + w - r, y + h);
    c.line_to(x + r, y + h);
    c.cubic_to(x + r - k, y + h, x, y + h - r + k, x, y + h - r);
    c.line_to(x, y + r);
    c.cubic_to(x, y + r - k, x + r - k, y, x + r, y);
    c.close_path();
}
