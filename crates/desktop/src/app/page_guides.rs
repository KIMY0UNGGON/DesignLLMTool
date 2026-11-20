//! Page-local layout guides of a canvas page: the overlay the board draws over the artboard, the snap
//! helpers that move, resize and create paths call, and the zero-extent rules of committed sizes. The
//! guides are an editing setting. Only the board draws them, so no render or export request carries them.

use designllm_contracts::{
    layout_guides::{
        calculate_page_guides, snap_create, snap_move, snap_resize, CreateSnap, EmuRect, GuideError, PageGuideLayout, Point,
        RectSnap, ResizeHandle,
    },
    ArtifactBody, CanvasDocument, CanvasPageGuides, NodeKind, Page, Size,
};
use eframe::egui::{Painter, Rect, Stroke};

use super::kind_commands::page_origin_of;
use crate::{app::DesktopApp, theme};

const NO_LINES: &[i64] = &[];

/// The track layout of a page's guides. `None` when the page size or an axis is not valid; the engine refuses
/// such a setting, so nothing is drawn or snapped for it.
pub(crate) fn page_layout(size: Size, guides: &CanvasPageGuides) -> Option<PageGuideLayout> {
    calculate_page_guides(size.width, size.height, guides.columns, guides.rows).ok()
}

/// The track layout of the guides `document` stores for `page`. `None` when the page has none, or when they do not fit it.
pub(crate) fn page_guide_layout(document: &CanvasDocument, page: &Page) -> Option<PageGuideLayout> {
    document.layout_guides.get(&page.id).and_then(|guides| page_layout(page.size, guides))
}

/// Screen x of every column line and screen y of every row line of a page drawn in `rect`. The rectangle already
/// carries the board's pan and zoom, so the page's EMU scale linearly onto it.
pub(crate) fn guide_positions(layout: &PageGuideLayout, page: Size, rect: Rect) -> (Vec<f32>, Vec<f32>) {
    let scale_x = f64::from(rect.width()) / page.width as f64;
    let scale_y = f64::from(rect.height()) / page.height as f64;
    let xs = layout.vertical_lines().iter().map(|&emu| (f64::from(rect.left()) + emu as f64 * scale_x) as f32).collect();
    let ys = layout.horizontal_lines().iter().map(|&emu| (f64::from(rect.top()) + emu as f64 * scale_y) as f32).collect();
    (xs, ys)
}

impl DesktopApp {
    /// Draws the guides of the page at `page_index` over its artboard. Nothing is drawn while the guides are hidden,
    /// when the page has none, or when the stored setting does not fit the page.
    pub(crate) fn draw_page_guides(&self, painter: &Painter, palette: &theme::Palette, page_index: u32, rect: Rect) {
        if !self.view_ref().guides_visible {
            return;
        }
        let Some(ArtifactBody::Canvas(document)) = self.artifact.as_ref().map(|artifact| &artifact.body) else {
            return;
        };
        let Some(page) = document.pages.get(page_index as usize) else {
            return;
        };
        let Some(layout) = page_guide_layout(document, page) else {
            return;
        };
        let (xs, ys) = guide_positions(&layout, page.size, rect);
        let stroke = Stroke::new(1.0, palette.accent);
        for x in xs {
            painter.vline(x, rect.y_range(), stroke);
        }
        for y in ys {
            painter.hline(rect.x_range(), y, stroke);
        }
    }

    /// Where the parent frame of the node `node_id` on `page_id` sits on the page: the node's page position less its own
    /// bounds, which are relative to that frame. `(0, 0)` for a page child. `None` when the page does not hold the node.
    pub(crate) fn parent_frame_origin(&self, page_id: &str, node_id: &str) -> Option<(i64, i64)> {
        let Some(ArtifactBody::Canvas(document)) = self.artifact.as_ref().map(|artifact| &artifact.body) else {
            return None;
        };
        let page = document.pages.iter().find(|page| page.id == page_id)?;
        let (x, y, node) = page_origin_of(&page.nodes, node_id, (0, 0))?;
        Some((x - node.bounds.x, y - node.bounds.y))
    }

    /// The frame a free move lands on, snapped to the guides of `page_id` when the view snaps to them (`view.toggle_snap`).
    /// `bounds` are relative to the parent frame, which sits at `parent` on the page (`(0, 0)` for a page child): the snap runs
    /// on the page position and the result goes back to the parent's coordinates. The snap window is 8 physical pixels at `zoom`
    /// and at the display scale the drag started with. A page without guides, a view that does not snap, and a snap the
    /// engine cannot compute keep the frame as it was dropped.
    pub(crate) fn snap_free_move(&self, page_id: &str, parent: (i64, i64), bounds: designllm_contracts::Rect, zoom: f32, pixels_per_point: f32) -> designllm_contracts::Rect {
        let view = self.view_ref();
        if !view.snap_guides {
            return bounds;
        }
        let Some(ArtifactBody::Canvas(document)) = self.artifact.as_ref().map(|artifact| &artifact.body) else {
            return bounds;
        };
        let Some(page) = document.pages.iter().find(|page| page.id == page_id) else {
            return bounds;
        };
        let Some(layout) = page_guide_layout(document, page) else {
            return bounds;
        };
        let frame = EmuRect {
            x_emu: bounds.x.saturating_add(parent.0),
            y_emu: bounds.y.saturating_add(parent.1),
            width_emu: bounds.width,
            height_emu: bounds.height,
        };
        match move_snap(Some(&layout), frame, zoom, pixels_per_point) {
            Ok(snap) => designllm_contracts::Rect {
                x: snap.rect.x_emu.saturating_sub(parent.0),
                y: snap.rect.y_emu.saturating_sub(parent.1),
                width: snap.rect.width_emu,
                height: snap.rect.height_emu,
            },
            Err(_) => bounds,
        }
    }
}

/// The guide lines a snap may land on. A page without guides has none.
fn guide_lines(layout: Option<&PageGuideLayout>) -> (&[i64], &[i64]) {
    layout.map_or((NO_LINES, NO_LINES), |layout| (layout.vertical_lines(), layout.horizontal_lines()))
}

/// The snapped rectangle of a move, in page EMU. `zoom` and `pixels_per_point` size the 8 px window.
pub(crate) fn move_snap(layout: Option<&PageGuideLayout>, rect: EmuRect, zoom: f32, pixels_per_point: f32) -> Result<RectSnap, GuideError> {
    let (vertical, horizontal) = guide_lines(layout);
    snap_move(rect, vertical, horizontal, f64::from(zoom), f64::from(pixels_per_point))
}

/// The snapped rectangle of a resize that drags `handle`. The opposite edge stays where it is.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn resize_snap(
    layout: Option<&PageGuideLayout>,
    rect: EmuRect,
    handle: ResizeHandle,
    zoom: f32,
    pixels_per_point: f32,
) -> Result<RectSnap, GuideError> {
    let (vertical, horizontal) = guide_lines(layout);
    snap_resize(rect, handle, vertical, horizontal, f64::from(zoom), f64::from(pixels_per_point))
}

/// The snapped anchor and cursor of a creation. The caller keeps the returned `anchor` for the rest of the drag.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn create_snap(
    layout: Option<&PageGuideLayout>,
    anchor: Point,
    cursor: Point,
    zoom: f32,
    pixels_per_point: f32,
) -> Result<CreateSnap, GuideError> {
    let (vertical, horizontal) = guide_lines(layout);
    snap_create(anchor, cursor, vertical, horizontal, f64::from(zoom), f64::from(pixels_per_point))
}

/// The zero-extent rules of a committed node size. No side is negative and 0 × 0 is never valid. A generic shape
/// needs both sides above 0. A `Line` may have one side at 0, which makes it a horizontal or a vertical line.
pub(crate) fn validate_node_extent(kind: NodeKind, width: i64, height: i64) -> Result<(), String> {
    if width < 0 || height < 0 {
        return Err("크기는 음수일 수 없습니다".into());
    }
    if width == 0 && height == 0 {
        return Err("크기는 0보다 커야 합니다".into());
    }
    if kind != NodeKind::Line && (width == 0 || height == 0) {
        return Err("선이 아닌 요소는 가로와 세로가 모두 0보다 커야 합니다".into());
    }
    Ok(())
}
