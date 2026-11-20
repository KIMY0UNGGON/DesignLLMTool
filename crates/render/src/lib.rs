//! Shared renderer (designllm-render): font discovery and fallback, single-line shaping into
//! [`ShapedText`], SVG/PNG/PDF encoding from one display list, and pure page queries.
//!
//! - Bundled Pretendard Regular/Bold (SIL OFL 1.1, `assets/fonts`) is preferred; installed
//!   system faces are a fallback. Every shaped run records the faces actually used.
//! - Shape once: encoders draw `Primitive::Text.shaped` glyphs at the explicit baseline origin
//!   and never reshape.
//! - Formats: `png` (one page, Editability::None), `svg` (one page, editable `<text>`,
//!   Editability::Objects), `svg-outline` (one page, glyph paths, `text_outlined`,
//!   Editability::Vector), `pdf` (all pages, embedded subset fonts, Editability::Vector).
//! - Queries take page EMU; core validates snapshot ids and view transforms first.
mod effects;
mod encode;
mod fonts;
mod icon;
mod paint;
mod pdf;
mod query;
mod raster;
mod region;
mod shape;
mod svg;
pub mod v12;

use designllm_contracts::*;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub use encode::scene_font_fingerprint;
#[doc(hidden)]
pub use encode::{line_markers, LineMarker};
pub use fonts::{face_family_names, face_matches_family, BUNDLED_BOLD, BUNDLED_FAMILY, BUNDLED_LICENSE, BUNDLED_REGULAR, SYSTEM_FALLBACKS};
pub use paint::{base64, jpeg_exif_orientation, pixel_size as image_pixel_size, ImageKind};
pub use query::{caret_rect, hit_test, selection_rects};
pub use effects::{corner_radius, dash_pattern, gradient_line, image_fit_rect, opacity, path_transform, shadow_sigma};
pub use shape::text_block_offset;
pub use icon::{rasterize_icon, IconOptions, IconRaster};
pub use region::page_region;
pub use shape::mul_div_round;

/// Formats accepted by [`RenderProvider::encode`].
pub const FORMATS: &[&str] = &["png", "svg", "svg-outline", "pdf"];

#[derive(Debug, Clone)]
pub struct RendererOptions {
    /// Use installed system fonts as fallback (and for explicitly requested families).
    pub system_fonts: bool,
}
impl Default for RendererOptions { fn default() -> Self { Self { system_fonts: true } } }

/// Shaping inputs: text, family, size, weight, italic, letter spacing, line height.
type ShapeKey = (String, String, i64, u16, bool, Option<i64>, Option<u32>);
const SHAPE_CACHE_LIMIT: usize = 50_000;

pub(crate) fn requested_font_weight(style: &Style) -> u16 {
    style.font_weight.unwrap_or(if style.bold { 700 } else { 400 })
}

pub(crate) fn synthetic_bold(style: &Style, face_weight: u16) -> bool {
    requested_font_weight(style) >= 600 && face_weight < 600
}

pub struct Renderer {
    book: fonts::FontBook,
    font_dir: PathBuf,
    cache: Mutex<HashMap<ShapeKey, ShapedText>>,
}

impl Renderer {
    /// Bundled Pretendard from `font_dir` plus installed system fonts as fallback.
    pub fn new(font_dir: &Path) -> DltResult<Self> { Self::with_options(font_dir, RendererOptions::default()) }

    /// Bundled fonts only: deterministic output independent of installed fonts.
    pub fn bundled_only(font_dir: &Path) -> DltResult<Self> { Self::with_options(font_dir, RendererOptions { system_fonts: false }) }

    pub fn with_options(font_dir: &Path, options: RendererOptions) -> DltResult<Self> {
        Ok(Self { book: fonts::FontBook::new(font_dir, options.system_fonts)?, font_dir: font_dir.to_path_buf(), cache: Mutex::new(HashMap::new()) })
    }

    pub fn font_dir(&self) -> &Path { &self.font_dir }

    /// [`RenderProvider::encode`] of only `region` (page EMU) of a one-page scene (P1_PLAN
    /// `RenderRequest.region`): the output covers `region` at the profile DPI. PNG draws the page
    /// moved by the region origin: with the origin on a whole pixel at that DPI it is the crop of the
    /// whole-page PNG, exact except on edge pixels of paths the region boundary cuts (tiny-skia clips
    /// them; at most 1/16 coverage). Vector formats get the region as a page ([`page_region`]).
    /// The whole scene is checked first, so a region never encodes what the page would refuse.
    /// INVALID_ARGUMENT for a multi-page scene or a region outside the page. `None` is `encode`.
    pub fn encode_region(&self, scene: &RenderScene, format: &str, profile: &RenderProfile, region: Option<Rect>, assets: &dyn AssetStore) -> DltResult<EncodedOutput> {
        self.encode_part(scene, format, profile, region, assets)
    }

    fn encode_part(&self, scene: &RenderScene, format: &str, profile: &RenderProfile, region: Option<Rect>, assets: &dyn AssetStore) -> DltResult<EncodedOutput> {
        if scene.pages.is_empty() { return Err(DltError::invalid("The scene has no pages to encode")); }
        let format = format.trim().to_ascii_lowercase();
        if format == "pdf" && region.is_some() && !scene.links.is_empty() {
            return Err(DltError::unsupported("PDF region export with RenderScene.links is not supported until annotation clipping is implemented"));
        }
        v12::refuse_scene(scene)?;
        let part;
        let scene = match region {
            None => scene,
            Some(r) => {
                if scene.pages.len() != 1 { return Err(DltError::invalid(format!("a render region needs a one-page scene; the scene has {} pages", scene.pages.len()))); }
                region::check(&scene.pages[0], r)?;
                if format == "png" { scene } else {
                    part = RenderScene { pages: vec![region::page_region(&scene.pages[0], r)?], font_fingerprint: scene.font_fingerprint.clone(), diagnostics: scene.diagnostics.clone(), metadata: scene.metadata.clone(), links: vec![] };
                    &part
                }
            }
        };
        let single = |name: &str| -> DltResult<&RenderPage> {
            if scene.pages.len() != 1 {
                return Err(DltError::invalid(format!("{name} output holds exactly one page; the scene has {} pages (select one page or use pdf)", scene.pages.len())));
            }
            Ok(&scene.pages[0])
        };
        let glyphs = encode::GlyphSource::new(&self.book);
        let mut diags = encode::Diags::new(&scene.diagnostics);
        let missing: Vec<String> = scene.pages.iter().flat_map(|p| p.primitives.iter()).filter_map(|p| match p { Primitive::Text { shaped, .. } => Some(shaped.missing_glyphs.clone()), _ => None }).flatten().collect();
        if !missing.is_empty() && !diags.has_code("missing_glyphs") {
            diags.add("missing_glyphs", format!("No available font has glyphs for {missing:?}"), None, true);
        }
        let (bytes, mime, editability) = match format.as_str() {
            "png" => (raster::encode_png(&glyphs, single("PNG")?, profile, region, assets, &mut diags)?, "image/png", Editability::None),
            "svg" => (svg::encode_svg(&glyphs, single("SVG")?, assets, false, &mut diags)?.into_bytes(), "image/svg+xml", Editability::Objects),
            "svg-outline" | "svg_outline" | "svg-outlines" => (svg::encode_svg(&glyphs, single("SVG")?, assets, true, &mut diags)?.into_bytes(), "image/svg+xml", Editability::Vector),
            "pdf" => (pdf::encode_pdf(&glyphs, scene, assets, &mut diags)?, "application/pdf", Editability::Vector),
            other => return Err(DltError::unsupported(format!("Render format '{other}' is not supported (supported: {})", FORMATS.join(", ")))),
        };
        Ok(EncodedOutput { bytes, mime: mime.into(), editability, diagnostics: diags.into_vec() })
    }
}

impl RenderProvider for Renderer {
    fn shape_text(&self, text: &str, style: &Style) -> DltResult<ShapedText> {
        v12::validate_font_weight(style)?;
        let key: ShapeKey = (text.to_string(), style.font_family.clone(), style.font_size, requested_font_weight(style), style.italic, style.letter_spacing, style.line_height_milli);
        if let Some(hit) = self.cache.lock().ok().and_then(|c| c.get(&key).cloned()) { return Ok(hit); }
        let shaped = shape::shape(&self.book, text, style)?;
        if let Ok(mut cache) = self.cache.lock() {
            if cache.len() >= SHAPE_CACHE_LIMIT { cache.clear(); }
            cache.insert(key, shaped.clone());
        }
        Ok(shaped)
    }

    fn encode(&self, scene: &RenderScene, format: &str, profile: &RenderProfile, assets: &dyn AssetStore) -> DltResult<EncodedOutput> {
        self.encode_part(scene, format, profile, None, assets)
    }
    /// The inherent [`Renderer::encode_region`] (core's render.get passes `RenderRequest.region`).
    fn encode_region(&self, scene: &RenderScene, format: &str, profile: &RenderProfile, region: Option<Rect>, assets: &dyn AssetStore) -> DltResult<EncodedOutput> {
        Renderer::encode_region(self, scene, format, profile, region, assets)
    }

    fn font_catalog_fingerprint(&self) -> String { self.book.catalog_fingerprint().to_string() }

    fn available_fonts(&self) -> Vec<String> { self.book.families() }

    fn hit_test(&self, page: &RenderPage, point: Point) -> DltResult<Option<HitTestResult>> { query::hit_test(page, point) }

    fn caret_rect(&self, page: &RenderPage, position: &TextPosition) -> DltResult<Rect> { query::caret_rect(page, position) }

    fn selection_rects(&self, page: &RenderPage, range: &TextRange) -> DltResult<Vec<Rect>> { query::selection_rects(page, range) }
}

/// Text primitive for one shaped line at a baseline origin (page EMU).
pub fn text_primitive(node_id: &str, origin: Point, text: &str, style: &Style, shaped: ShapedText) -> Primitive {
    let bounds = Rect { x: origin.x, y: origin.y - shaped.ascent, width: shaped.size.width, height: shaped.ascent + shaped.descent };
    Primitive::Text { node_id: node_id.to_string(), bounds, origin, text: text.to_string(), style: style.clone(), shaped }
}

/// Source region for a shaped line: logical range `[start, start + graphemes)` of
/// `paragraph_id`, absolute grapheme rects. `line` overrides the vertical extent (line box).
pub fn text_region(node_id: &str, paragraph_id: &str, start: u32, origin: Point, shaped: &ShapedText, line: Option<(i64, i64)>) -> SourceRegion {
    let (top, height) = line.unwrap_or((origin.y - shaped.ascent, shaped.ascent + shaped.descent));
    let rects: Vec<Rect> = shaped.grapheme_rects.iter().map(|r| Rect { x: origin.x + r.x, y: top, width: r.width, height }).collect();
    SourceRegion {
        node_id: node_id.to_string(),
        bounds: Rect { x: origin.x, y: top, width: shaped.size.width, height },
        logical_range: Some(TextRange { paragraph_id: paragraph_id.to_string(), start, end: start + rects.len() as u32 }),
        grapheme_rects: rects,
    }
}

/// Decode PNG, JPEG or BMP into straight RGBA8 `(width, height, pixels)`.
pub fn decode_image(bytes: &[u8]) -> DltResult<(u32, u32, Vec<u8>)> {
    let d = paint::decode(bytes)?;
    Ok((d.width, d.height, d.rgba))
}

/// Encode straight RGBA8 pixels as PNG.
pub fn png_from_rgba(width: u32, height: u32, rgba: &[u8]) -> DltResult<Vec<u8>> {
    if rgba.len() as u64 != u64::from(width) * u64::from(height) * 4 { return Err(DltError::invalid("RGBA buffer size does not match the image size")); }
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, width, height);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut w = enc.write_header().map_err(|e| DltError::new(ErrorCode::Internal, format!("PNG header: {e}")))?;
        w.write_image_data(rgba).map_err(|e| DltError::new(ErrorCode::Internal, format!("PNG data: {e}")))?;
    }
    Ok(out)
}
