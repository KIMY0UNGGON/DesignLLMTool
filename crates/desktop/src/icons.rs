//! Icons from `design/icons/`, rasterised by the render crate at the physical pixel size they are drawn at
//! (GUI_DESIGN §6.5). One-colour icons are rasterised as a white mask and tinted when drawn, so a
//! single texture serves every colour (`currentColor`). Multicolour icons (`tpl-*`, `app-24`) keep
//! their own colours. The cache key is (name, physical pixels), so a monitor change re-rasterises.

use std::{cell::RefCell, collections::HashMap};

use eframe::egui::{Color32, ColorImage, Context, Pos2, Rect, TextureHandle, TextureOptions, Ui};
use designllm_render::{rasterize_icon, IconOptions};

use crate::icons_table::ICONS;

thread_local! {
    /// Textures belong to one egui context; the window runs on one thread, and each test thread
    /// creates its own context, so a thread-local cache is enough.
    static CACHE: RefCell<HashMap<(String, u32), TextureHandle>> = RefCell::new(HashMap::new());
}

/// Whether `name` is a known icon.
#[cfg(test)]
pub fn exists(name: &str) -> bool {
    source(name).is_some()
}

/// The SVG of a UI icon, or of a content icon named `content:<name>` (the icon picker's previews).
fn source(name: &str) -> Option<&'static str> {
    if let Some(content) = name.strip_prefix("content:") {
        return crate::content_icons::source(content);
    }
    ICONS.iter().find(|(icon, _)| *icon == name).map(|(_, svg)| *svg)
}

fn is_multicolour(name: &str) -> bool {
    name.starts_with("tpl-") || name.starts_with("app-")
}

/// White `currentColor`: a one-colour icon is a white mask that the tint colours.
const ICON_OPTIONS: IconOptions = IconOptions { current_color: [255, 255, 255, 255], stroke_width: None };

/// Rasterises an icon at `px` x `px` as straight RGBA. The render crate's rasteriser keeps the joins
/// that resvg drops at 16 px (`docs/GUI_DESIGN.md` §6.5). Multicolour icons have no `currentColor`.
fn rasterize(_name: &str, svg: &str, px: u32) -> Option<ColorImage> {
    let raster = rasterize_icon(svg, px.max(1), px.max(1), &ICON_OPTIONS).ok()?;
    Some(ColorImage::from_rgba_unmultiplied([raster.width as usize, raster.height as usize], &raster.rgba))
}

/// The texture for `name` at `px` physical pixels, rasterised once per size.
pub fn texture(ctx: &Context, name: &str, px: u32) -> Option<TextureHandle> {
    let key = (name.to_owned(), px);
    if let Some(found) = CACHE.with(|cache| cache.borrow().get(&key).cloned()) {
        return Some(found);
    }
    let svg = source(name)?;
    let image = rasterize(name, svg, px.max(1))?;
    let handle = ctx.load_texture(format!("icon-{name}-{px}"), image, TextureOptions::LINEAR);
    CACHE.with(|cache| cache.borrow_mut().insert(key, handle.clone()));
    Some(handle)
}

/// Rounds a rectangle outwards-to-nearest onto the physical pixel grid, so icon edges fall on
/// pixel boundaries (§6.5: no stretching).
pub fn snap(rect: Rect, pixels_per_point: f32) -> Rect {
    let ppp = pixels_per_point.max(0.01);
    let snap_value = |value: f32| (value * ppp).round() / ppp;
    Rect::from_min_max(
        Pos2::new(snap_value(rect.min.x), snap_value(rect.min.y)),
        Pos2::new(snap_value(rect.max.x), snap_value(rect.max.y)),
    )
}

/// Paints the icon `name` inside `rect` (a square of logical points). `color` tints one-colour
/// icons; multicolour icons ignore it. Returns false when the icon is unknown.
pub fn paint(ui: &Ui, rect: Rect, name: &str, color: Color32) -> bool {
    let ctx = ui.ctx();
    let ppp = ctx.pixels_per_point();
    let target = snap(rect, ppp);
    let px = (target.width().min(target.height()) * ppp).round().max(1.0) as u32;
    let Some(texture) = texture(ctx, name, px) else {
        return false;
    };
    let tint = if is_multicolour(name) { Color32::WHITE } else { color };
    let uv = Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0));
    ui.painter().image(texture.id(), target, uv, tint);
    true
}

/// The app icon at `size` x `size` physical pixels as straight (not premultiplied) RGBA. The SVG
/// is the master design (`design/icons/app/app-<size>.svg`, drawn for that size).
pub fn straight_rgba(svg: &str, size: u32) -> Option<Vec<u8>> {
    rasterize_icon(svg, size, size, &ICON_OPTIONS).ok().map(|raster| raster.rgba)
}

/// The window and taskbar icon: the 48 px master, which Windows scales for the smaller uses.
pub fn window_icon() -> std::sync::Arc<eframe::egui::IconData> {
    let size = 48;
    let rgba = straight_rgba(include_str!("../../../design/icons/app/app-48.svg"), size).unwrap_or_else(|| vec![0; (size * size * 4) as usize]);
    std::sync::Arc::new(eframe::egui::IconData { rgba, width: size, height: size })
}

#[cfg(test)]
mod tests {
    use super::*;
    use eframe::egui;

    #[test]
    fn every_icon_the_index_names_is_included_and_has_a_source() {
        assert!(ICONS.len() >= 180, "the canonical set is 181 icons plus app-24");
        assert!(exists("tool-select"));
        assert!(exists("tpl-mobile"));
        assert!(!exists("alias-that-does-not-exist"));
    }

    #[test]
    fn a_one_colour_icon_rasterises_to_a_white_mask_with_alpha() {
        let image = rasterize("tool-select", source("tool-select").unwrap(), 16).expect("rasterised");
        assert_eq!(image.size, [16, 16]);
        let opaque: Vec<_> = image.pixels.iter().filter(|pixel| pixel.a() > 0).collect();
        assert!(!opaque.is_empty(), "the stroke must leave coverage");
        // Premultiplied white: every covered pixel has equal channels.
        assert!(opaque.iter().all(|pixel| pixel.r() == pixel.a() && pixel.g() == pixel.a()));
    }

    #[test]
    fn a_multicolour_icon_keeps_its_own_colours() {
        let image = rasterize("tpl-mobile", source("tpl-mobile").unwrap(), 48).expect("rasterised");
        // Some pixels are chromatic (not grey): the accent colour of the preview survives.
        let coloured = image.pixels.iter().any(|pixel| {
            let (r, g, b) = (i32::from(pixel.r()), i32::from(pixel.g()), i32::from(pixel.b()));
            pixel.a() > 200 && (r - b).abs() + (g - b).abs() > 60
        });
        assert!(coloured, "the accent colour of the template preview survives");
    }

    #[test]
    fn snapping_puts_the_edges_on_whole_physical_pixels() {
        let rect = Rect::from_min_size(Pos2::new(10.3, 4.6), egui::vec2(16.0, 16.0));
        let snapped = snap(rect, 1.5);
        for value in [snapped.min.x, snapped.min.y, snapped.max.x, snapped.max.y] {
            let physical = value * 1.5;
            assert!((physical - physical.round()).abs() < 1e-4, "{value} is not on the pixel grid");
        }
    }
}

/// Writes `assets/app.ico` (16, 24, 32, 48, 256 px; PNG entries) from the app icon masters. The
/// file is checked in and `build.rs` embeds it. Run this only when the icon design changes:
/// `cargo test -p designllm-desktop regenerate_app_icon_file -- --ignored`.
#[cfg(test)]
mod app_icon_file {
    use std::path::Path;

    use image::ImageEncoder;

    fn png_of(rgba: &[u8], size: u32) -> Vec<u8> {
        let mut bytes = Vec::new();
        image::codecs::png::PngEncoder::new(&mut bytes)
            .write_image(rgba, size, size, image::ExtendedColorType::Rgba8)
            .expect("png encodes");
        bytes
    }

    #[test]
    #[ignore = "writes assets/app.ico; run only when the app icon design changes"]
    fn regenerate_app_icon_file() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut images = Vec::new();
        for size in [16_u32, 24, 32, 48, 256] {
            let svg = std::fs::read_to_string(root.join(format!("../../design/icons/app/app-{size}.svg")))
                .expect("app icon master exists");
            let rgba = super::straight_rgba(&svg, size).expect("app icon master rasterises");
            images.push((size, png_of(&rgba, size)));
        }
        let mut file = Vec::new();
        file.extend_from_slice(&0_u16.to_le_bytes());
        file.extend_from_slice(&1_u16.to_le_bytes());
        file.extend_from_slice(&(images.len() as u16).to_le_bytes());
        let mut offset = 6 + 16 * images.len() as u32;
        for (size, png) in &images {
            // A 256 px entry stores 0 in the width and height bytes.
            let dimension = if *size >= 256 { 0 } else { *size as u8 };
            file.push(dimension);
            file.push(dimension);
            file.push(0);
            file.push(0);
            file.extend_from_slice(&1_u16.to_le_bytes());
            file.extend_from_slice(&32_u16.to_le_bytes());
            file.extend_from_slice(&(png.len() as u32).to_le_bytes());
            file.extend_from_slice(&offset.to_le_bytes());
            offset += png.len() as u32;
        }
        for (_, png) in &images {
            file.extend_from_slice(png);
        }
        std::fs::create_dir_all(root.join("assets")).expect("assets folder");
        std::fs::write(root.join("assets").join("app.ico"), file).expect("app.ico is written");
    }
}
