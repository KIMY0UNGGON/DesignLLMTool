//! Board geometry (GUI_DESIGN §4, §5.6): where pages sit on the board, how zoom maps board units to
//! screen points, and the render resolution that makes a page sharp at the size it is shown. Pure
//! functions, so the placement rules are tested without a window.

use designllm_contracts::{ArtifactKind, Size};
use eframe::egui::{Pos2, Rect, Vec2};

/// One board unit is one CSS pixel (9525 EMU) for canvases, slides and web pages.
pub const EMU_PER_UNIT: f32 = designllm_contracts::EMU_PER_CSS_PX as f32;
/// Canvas and slide gaps in board units (§4).
pub const CANVAS_GAP: f32 = 96.0;
pub const SLIDE_GAP_X: f32 = 64.0;
pub const SLIDE_GAP_Y: f32 = 96.0;
/// Document pages stack with a gap in screen points.
pub const FLOW_GAP_SCREEN: f32 = 24.0;
/// The smallest screen gaps between neighbours: 16 across, 40 down (the title row needs 20).
pub const MIN_GAP_X_SCREEN: f32 = 16.0;
pub const MIN_GAP_Y_SCREEN: f32 = 40.0;
pub const SLIDE_COLUMNS: usize = 4;
pub const ZOOM_MIN: f32 = 0.02;
pub const ZOOM_MAX: f32 = 8.0;
/// Render resolution bounds in dots per inch.
pub const DPI_MIN: u32 = 24;
pub const DPI_MAX: u32 = 600;
/// Margin around the content when fitting.
pub const FIT_MARGIN: f32 = 48.0;

/// Where one page sits on the board, in board units.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PageSlot {
    pub page_index: u32,
    pub world: Rect,
}

/// The board arrangement for a document kind (§4). `zoom` only changes the gaps, which never fall
/// below the screen minimum.
pub fn arrange(kind: ArtifactKind, sizes: &[Size], zoom: f32) -> Vec<PageSlot> {
    let zoom = zoom.max(ZOOM_MIN);
    let units = |size: &Size| Vec2::new(size.width as f32 / EMU_PER_UNIT, size.height as f32 / EMU_PER_UNIT);
    match kind {
        ArtifactKind::Slides => {
            // The 16 px minimum is the gap between the slides as seen, outside their 1 px shadow rings (lead r12).
            let gap_x = SLIDE_GAP_X.max((MIN_GAP_X_SCREEN + 2.0) / zoom);
            let gap_y = SLIDE_GAP_Y.max(MIN_GAP_Y_SCREEN / zoom);
            let mut slots = Vec::with_capacity(sizes.len());
            let mut row_top = 0.0_f32;
            for row in sizes.chunks(SLIDE_COLUMNS).enumerate().map(|(index, chunk)| (index, chunk)) {
                let (row_index, chunk) = row;
                let mut x = 0.0_f32;
                let mut row_height = 0.0_f32;
                for (column, size) in chunk.iter().enumerate() {
                    let dims = units(size);
                    let index = row_index * SLIDE_COLUMNS + column;
                    slots.push(PageSlot {
                        page_index: index as u32,
                        world: Rect::from_min_size(Pos2::new(x, row_top), dims),
                    });
                    x += dims.x + gap_x;
                    row_height = row_height.max(dims.y);
                }
                row_top += row_height + gap_y;
            }
            slots
        }
        ArtifactKind::Docx | ArtifactKind::Hwpx | ArtifactKind::Hwp => {
            // Document pages stack in a column, centred on the widest page.
            let gap = FLOW_GAP_SCREEN / zoom;
            let widest = sizes.iter().map(|size| units(size).x).fold(0.0_f32, f32::max);
            let mut top = 0.0_f32;
            sizes
                .iter()
                .enumerate()
                .map(|(index, size)| {
                    let dims = units(size);
                    let slot = PageSlot {
                        page_index: index as u32,
                        world: Rect::from_min_size(Pos2::new((widest - dims.x) / 2.0, top), dims),
                    };
                    top += dims.y + gap;
                    slot
                })
                .collect()
        }
        ArtifactKind::Figure => sizes
            .iter()
            .take(1)
            .enumerate()
            .map(|(index, size)| {
                let dims = units(size);
                PageSlot { page_index: index as u32, world: Rect::from_min_size(Pos2::new(-dims.x / 2.0, -dims.y / 2.0), dims) }
            })
            .collect(),
        ArtifactKind::Canvas | ArtifactKind::WebCode => {
            let gap = CANVAS_GAP.max(MIN_GAP_X_SCREEN / zoom);
            let mut x = 0.0_f32;
            sizes
                .iter()
                .enumerate()
                .map(|(index, size)| {
                    let dims = units(size);
                    let slot = PageSlot { page_index: index as u32, world: Rect::from_min_size(Pos2::new(x, 0.0), dims) };
                    x += dims.x + gap;
                    slot
                })
                .collect()
        }
    }
}

/// The bounding box of the slots, in board units.
pub fn bounds(slots: &[PageSlot]) -> Option<Rect> {
    slots.iter().map(|slot| slot.world).reduce(|a, b| a.union(b))
}

/// A world rectangle on the screen: `origin` is the screen position of world (0, 0).
pub fn to_screen(world: Rect, origin: Pos2, zoom: f32) -> Rect {
    Rect::from_min_size(origin + world.min.to_vec2() * zoom, world.size() * zoom)
}

/// The screen point as a world point (inverse of `to_screen`).
#[cfg(test)]
pub fn to_world(point: Pos2, origin: Pos2, zoom: f32) -> Pos2 {
    Pos2::ZERO + (point - origin) / zoom
}

/// Zoom that fits `content` (board units) into `viewport`, with the margin, capped at `max_zoom`.
pub fn fit_zoom(content: Rect, viewport: Rect, max_zoom: f32) -> f32 {
    let width = (viewport.width() - FIT_MARGIN * 2.0).max(1.0);
    let height = (viewport.height() - FIT_MARGIN * 2.0).max(1.0);
    let zoom = (width / content.width().max(1.0)).min(height / content.height().max(1.0));
    zoom.clamp(ZOOM_MIN, max_zoom)
}

/// The screen origin that centres `content` in `viewport` at `zoom`.
pub fn centred_origin(content: Rect, viewport: Rect, zoom: f32) -> Pos2 {
    let centre = content.center().to_vec2() * zoom;
    viewport.center() - centre
}

/// The render resolution for a page shown at `zoom` on a screen with `pixels_per_point`. One board
/// unit is 1/96 inch, so a page drawn `zoom * ppp` physical pixels per unit needs 96 * zoom * ppp
/// dots per inch, rounded to a whole dpi (the zoom snaps to it once the view settles, `snapped_zoom`).
pub fn dpi_for(zoom: f32, pixels_per_point: f32) -> u32 {
    let raw = (96.0 * zoom * pixels_per_point).round() as i64;
    raw.clamp(i64::from(DPI_MIN), i64::from(DPI_MAX)) as u32
}

/// The zoom at which a page rendered at a whole dpi maps one texture pixel to one physical pixel: the zoom nearest to
/// `zoom` where 96 × zoom × ppp is a whole number (under half a dpi away: a 390 px page changes by at most 1 px). The
/// page is then `round(width × zoom)` pixels wide on screen, the size the renderer gives its texture (half-up
/// `pixels()`). A zoom whose resolution lies outside `DPI_MIN..=DPI_MAX` stays as it is (stretched).
pub fn snapped_zoom(zoom: f32, pixels_per_point: f32) -> f32 {
    let raw = 96.0 * zoom * pixels_per_point;
    if raw < DPI_MIN as f32 || raw > DPI_MAX as f32 {
        return zoom;
    }
    let dpi = raw.round();
    if (raw - dpi).abs() < 1e-3 { zoom } else { dpi / (96.0 * pixels_per_point) }
}

/// Whether a texture rendered at `dpi` maps 1:1 to physical pixels at `zoom`.
pub fn pixel_exact(dpi: u32, zoom: f32, pixels_per_point: f32) -> bool {
    (96.0 * zoom * pixels_per_point - dpi as f32).abs() < 1e-2
}

/// The screen rectangle that shows a `px` (physical pixels) texture 1:1: its corner on the physical pixel grid
/// nearest to `min`, and exactly `px / ppp` points large.
pub fn pixel_rect(min: Pos2, px: Vec2, pixels_per_point: f32) -> Rect {
    let snap = |value: f32| (value * pixels_per_point).round() / pixels_per_point;
    Rect::from_min_size(Pos2::new(snap(min.x), snap(min.y)), px / pixels_per_point)
}

/// The most pixels one page texture may have (16 million, 64 MB of RGBA). A page shown very large is
/// rendered at the highest resolution that fits; region rendering (`RenderRequest.region`) would
/// render only what is on screen and replace this cap.
pub const MAX_PAGE_PIXELS: f64 = 16_000_000.0;

/// `dpi`, lowered in steps of 8 until a page of `size` (EMU) fits in `MAX_PAGE_PIXELS`.
#[cfg(test)]
pub fn capped_dpi(dpi: u32, size: Size) -> u32 {
    capped_dpi_within(dpi, size, usize::MAX)
}

/// `capped_dpi`, and also no side longer than `max_side` pixels (the GPU's largest texture).
pub fn capped_dpi_within(dpi: u32, size: Size, max_side: usize) -> u32 {
    let inches_w = size.width as f64 / 914_400.0;
    let inches_h = size.height as f64 / 914_400.0;
    let side = max_side.min(u32::MAX as usize) as f64;
    let fits = |dpi: u32| {
        let (w, h) = (inches_w * f64::from(dpi), inches_h * f64::from(dpi));
        w * h <= MAX_PAGE_PIXELS && w.ceil() <= side && h.ceil() <= side
    };
    let mut dpi = dpi.max(DPI_MIN);
    while dpi > DPI_MIN && !fits(dpi) {
        dpi = dpi.saturating_sub(8).max(DPI_MIN);
    }
    dpi
}

/// The part of a page drawn at screen rectangle `page` that lies inside `board`, in page EMU (whole EMU,
/// inside the page). `None` when nothing of the page is on the board.
pub fn visible_region(page: Rect, board: Rect, size: Size) -> Option<designllm_contracts::Rect> {
    let shown = page.intersect(board);
    if shown.width() <= 0.0 || shown.height() <= 0.0 || page.width() <= 0.0 || page.height() <= 0.0 {
        return None;
    }
    let scale_x = size.width as f64 / f64::from(page.width());
    let scale_y = size.height as f64 / f64::from(page.height());
    let left = (f64::from(shown.left() - page.left()) * scale_x).floor().max(0.0) as i64;
    let top = (f64::from(shown.top() - page.top()) * scale_y).floor().max(0.0) as i64;
    let right = ((f64::from(shown.right() - page.left()) * scale_x).ceil() as i64).min(size.width);
    let bottom = ((f64::from(shown.bottom() - page.top()) * scale_y).ceil() as i64).min(size.height);
    (right > left && bottom > top).then(|| designllm_contracts::Rect { x: left, y: top, width: right - left, height: bottom - top })
}

/// Whether `outer` holds all of `inner`.
pub fn covers(outer: designllm_contracts::Rect, inner: designllm_contracts::Rect) -> bool {
    outer.x <= inner.x && outer.y <= inner.y && outer.x + outer.width >= inner.x + inner.width && outer.y + outer.height >= inner.y + inner.height
}

/// `region` grown by a quarter of its size on each side (small pans need no new render), kept inside the page.
pub fn padded_region(region: designllm_contracts::Rect, size: Size) -> designllm_contracts::Rect {
    let pad_x = region.width / 4;
    let pad_y = region.height / 4;
    let left = (region.x - pad_x).max(0);
    let top = (region.y - pad_y).max(0);
    let right = (region.x + region.width + pad_x).min(size.width);
    let bottom = (region.y + region.height + pad_y).min(size.height);
    designllm_contracts::Rect { x: left, y: top, width: right - left, height: bottom - top }
}

/// Whether a page at `rect` is visible within `board` (touching counts, so edges stay drawn).
pub fn visible(rect: Rect, board: Rect) -> bool {
    rect.intersects(board)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn phone() -> Size {
        Size { width: 390 * designllm_contracts::EMU_PER_CSS_PX, height: 844 * designllm_contracts::EMU_PER_CSS_PX }
    }

    fn slide() -> Size {
        Size { width: 12_192_000, height: 6_858_000 }
    }

    #[test]
    fn canvas_pages_form_one_top_aligned_row_with_the_design_gap() {
        let slots = arrange(ArtifactKind::Canvas, &[phone(), phone(), phone()], 1.0);
        assert_eq!(slots.len(), 3);
        assert!(slots.iter().all(|slot| slot.world.top() == 0.0));
        let gap = slots[1].world.left() - slots[0].world.right();
        assert!((gap - CANVAS_GAP).abs() < 1e-3, "gap {gap}");
    }

    #[test]
    fn slides_form_a_four_column_grid_with_their_gaps() {
        let slides: Vec<Size> = (0..8).map(|_| slide()).collect();
        let slots = arrange(ArtifactKind::Slides, &slides, 1.0);
        let columns: std::collections::BTreeSet<i64> = slots.iter().map(|slot| slot.world.left() as i64).collect();
        assert_eq!(columns.len(), 4);
        let rows: std::collections::BTreeSet<i64> = slots.iter().map(|slot| slot.world.top() as i64).collect();
        assert_eq!(rows.len(), 2);
        let horizontal = slots[1].world.left() - slots[0].world.right();
        assert!((horizontal - SLIDE_GAP_X).abs() < 1e-3);
    }

    #[test]
    fn at_a_small_zoom_the_gaps_grow_to_the_screen_minimum() {
        // Eight slides at 10 %: a board gap of 64 would be 6.4 px. The rule keeps 16 px.
        let slides: Vec<Size> = (0..8).map(|_| slide()).collect();
        let zoom = 0.1;
        let slots = arrange(ArtifactKind::Slides, &slides, zoom);
        let horizontal_screen = (slots[1].world.left() - slots[0].world.right()) * zoom;
        let vertical_screen = (slots[4].world.top() - slots[0].world.bottom()) * zoom;
        assert!(horizontal_screen >= MIN_GAP_X_SCREEN - 1e-3, "horizontal {horizontal_screen}");
        assert!(vertical_screen >= MIN_GAP_Y_SCREEN - 1e-3, "vertical {vertical_screen}");
    }

    #[test]
    fn document_pages_stack_and_are_centred_on_the_widest() {
        let a4 = Size { width: 210 * 36_000, height: 297 * 36_000 };
        let narrow = Size { width: 100 * 36_000, height: 100 * 36_000 };
        let slots = arrange(ArtifactKind::Docx, &[a4, narrow], 1.0);
        assert!(slots[1].world.top() > slots[0].world.bottom());
        assert!(slots[1].world.left() > slots[0].world.left(), "the narrow page is centred");
    }

    #[test]
    fn a_figure_is_one_page_centred_on_the_board_origin() {
        let figure = Size { width: 170 * 36_000, height: 120 * 36_000 };
        let slots = arrange(ArtifactKind::Figure, &[figure], 1.0);
        assert_eq!(slots.len(), 1);
        assert!((slots[0].world.center().x).abs() < 1e-3);
        assert!((slots[0].world.center().y).abs() < 1e-3);
    }

    #[test]
    fn world_and_screen_map_back_to_each_other() {
        let origin = Pos2::new(300.0, 120.0);
        let world = Rect::from_min_size(Pos2::new(40.0, 10.0), Vec2::new(390.0, 844.0));
        let screen = to_screen(world, origin, 0.58);
        assert!((screen.width() - 390.0 * 0.58).abs() < 1e-3);
        let back = to_world(screen.min, origin, 0.58);
        assert!((back.x - 40.0).abs() < 1e-3 && (back.y - 10.0).abs() < 1e-3);
    }

    #[test]
    fn a_page_is_rendered_at_the_physical_size_it_is_shown_at() {
        // 96 dpi at 100 % on a 1x screen; 144 dpi at 100 % on 1.5x; half size on 1x gives 48.
        assert_eq!(dpi_for(1.0, 1.0), 96);
        assert_eq!(dpi_for(1.0, 1.5), 144);
        assert_eq!(dpi_for(0.5, 1.0), 48);
        assert_eq!(dpi_for(0.01, 1.0), DPI_MIN, "clamped below");
        assert_eq!(dpi_for(20.0, 2.0), DPI_MAX, "clamped above");
        assert_eq!(dpi_for(0.58, 1.0), 56, "a whole dpi, no steps of 4 (55.68 → 56)");
    }

    #[test]
    fn a_settled_zoom_maps_one_texture_pixel_to_one_screen_pixel() {
        for (zoom, ppp) in [(0.58_f32, 1.0_f32), (0.6213, 1.25), (1.0, 1.5), (2.37, 2.0)] {
            let snapped = snapped_zoom(zoom, ppp);
            let dpi = dpi_for(snapped, ppp);
            assert!(pixel_exact(dpi, snapped, ppp), "{zoom} at {ppp}x → {snapped}, {dpi} dpi");
            assert!((snapped - zoom).abs() <= 0.5 / (96.0 * ppp) + 1e-6, "the zoom moves by at most half a dpi");
            // On screen the page is round(width × zoom) wide, the renderer's half-up texture width at that dpi.
            let width_px = 390.0 * snapped * ppp;
            assert!((width_px - (390.0 * dpi as f32 / 96.0)).abs() < 1e-3);
            assert_eq!(snapped_zoom(snapped, ppp), snapped, "snapping twice changes nothing");
        }
        // The texture's corner lands on the physical grid and its size is its pixel size.
        let rect = pixel_rect(Pos2::new(300.3, 120.6), Vec2::new(453.0, 980.0), 1.25);
        assert_eq!((rect.min.x * 1.25, rect.min.y * 1.25), (375.0, 151.0));
        assert!((rect.width() * 1.25 - 453.0).abs() < 1e-3 && (rect.height() * 1.25 - 980.0).abs() < 1e-3);
    }

    #[test]
    fn a_very_large_page_is_rendered_at_the_highest_resolution_that_fits() {
        let a3 = Size { width: 297 * 36_000, height: 420 * 36_000 };
        let capped = capped_dpi(600, a3);
        let pixels = (297.0 / 25.4 * f64::from(capped)) * (420.0 / 25.4 * f64::from(capped));
        assert!(pixels <= MAX_PAGE_PIXELS, "{capped} dpi gives {pixels} px");
        assert!(capped > DPI_MIN, "the cap lowers the resolution, it does not go to the minimum");
        assert_eq!(capped_dpi(96, Size::a4()), 96, "small pages keep the requested resolution");
    }

    #[test]
    fn the_visible_part_of_a_zoomed_page_is_its_region_in_page_emu() {
        // An A3 page shown 4000 px wide from (-1000, -2000): the board (0, 44)–(848, 900) sees a corner of it.
        let a3 = Size { width: 297 * 36_000, height: 420 * 36_000 };
        let page = Rect::from_min_size(Pos2::new(-1000.0, -2000.0), Vec2::new(4000.0, 4000.0 * 420.0 / 297.0));
        let board = Rect::from_min_max(Pos2::new(0.0, 44.0), Pos2::new(848.0, 900.0));
        let region = visible_region(page, board, a3).expect("part of the page is on the board");
        let per_px = a3.width as f64 / 4000.0;
        assert!((region.x as f64 - 1000.0 * per_px).abs() <= 1.0);
        assert!((region.width as f64 - 848.0 * per_px).abs() <= 2.0);
        assert!(region.x + region.width <= a3.width && region.y + region.height <= a3.height);
        let padded = padded_region(region, a3);
        assert!(covers(padded, region));
        assert!(padded.x >= 0 && padded.x + padded.width <= a3.width, "the padding stays inside the page");
        let off_board = Rect::from_min_size(Pos2::new(2000.0, 2000.0), Vec2::new(100.0, 100.0));
        assert!(visible_region(off_board, board, a3).is_none());
    }

    #[test]
    fn fitting_a_page_keeps_it_inside_the_viewport_and_centres_it() {
        let viewport = Rect::from_min_size(Pos2::new(0.0, 44.0), Vec2::new(900.0, 700.0));
        let content = Rect::from_min_size(Pos2::ZERO, Vec2::new(390.0, 844.0));
        let zoom = fit_zoom(content, viewport, 1.0);
        let screen = to_screen(content, centred_origin(content, viewport, zoom), zoom);
        assert!(viewport.contains_rect(screen), "{screen:?} outside {viewport:?}");
        assert!((screen.center().x - viewport.center().x).abs() < 1e-3);
        assert!(zoom <= 1.0);
    }
}
