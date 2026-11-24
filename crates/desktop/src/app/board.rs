//! The board: pages on a zoomable surface, each drawn at the physical size it is shown at (U7). A
//! page is rendered by the core at `board::dpi_for(zoom, ppp)`; while the view is zooming the old
//! texture is stretched, and the new resolution is requested once the zoom has settled (§5.6).

use std::time::Duration;

use designllm_contracts::{
    new_id, ArtifactKind, RenderPage, RenderProfile, RenderRequest, Size, ViewTransform,
};
use eframe::egui::{
    self, Color32, Context, CornerRadius, Id, Pos2, Rect, Response, Sense,
    TextureHandle, TextureOptions, Ui, Vec2,
};
use serde_json::Value;

use super::{to_micropx, DesktopApp, PendingAction};
use crate::{board, theme};

/// The first view of a canvas or slides: room left and right, above (view group + title row) and below (tool bar).
const FIT_SIDE: f32 = 28.0;
const FIT_TOP: f32 = 76.0;
const FIT_BOTTOM: f32 = 74.0;
const FLOW_TOP: f32 = 56.0;

/// How long a zoom must be still before the board asks for a sharper page (§5.6).
pub(super) const SETTLE: Duration = Duration::from_millis(120);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum LeftTab {
    Layers,
    /// The insert tiles of a canvas or slides (68g toolbox, `toolbox.rs`).
    Tools,
    Ai,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Tool {
    Select,
    Hand,
    Memo,
}

/// The view of one document: zoom, pan and panels. It is kept per document while the app runs, so
/// returning to a document restores where the user was (§2).
#[derive(Clone, Debug)]
pub(super) struct EditorView {
    pub zoom: f32,
    /// Screen offset of the board origin from the board's top-left corner.
    pub pan: Vec2,
    pub fitted: bool,
    pub zoom_changed: std::time::Instant,
    pub left_tab: LeftTab,
    pub left_open: bool,
    pub right_open: bool,
    pub left_width: f32,
    pub right_width: f32,
    pub tool: Tool,
    pub page_index: u32,
    /// The panels were set for the window once, when the document was first shown.
    pub panels_placed: bool,
    /// A figure's data sheet is hidden (`figure.toggle_sheet`); it shows by default when a chart panel is selected.
    pub sheet_hidden: bool,
    /// Canvas page guides are drawn over the artboards (`view.toggle_guides`). Drawing is never exported.
    pub guides_visible: bool,
    /// Moves, resizes and creations snap to the page guides (`view.toggle_snap`).
    pub snap_guides: bool,
    pub figure_export_dpi: u32,
    pub figure_output_profile: Option<String>,
}

impl Default for EditorView {
    fn default() -> Self {
        Self {
            zoom: 1.0,
            pan: Vec2::ZERO,
            fitted: false,
            zoom_changed: std::time::Instant::now(),
            left_tab: LeftTab::Layers,
            left_open: true,
            right_open: true,
            left_width: theme::SIDEBAR_WIDTH,
            right_width: theme::PANEL_WIDTH,
            tool: Tool::Select,
            page_index: 0,
            panels_placed: false,
            sheet_hidden: false,
            guides_visible: true,
            snap_guides: true,
            figure_export_dpi: 300,
            figure_output_profile: None,
        }
    }
}

/// One rendered page on the board: the texture, the resolution and the snapshot it came from.
pub(super) struct PageTexture {
    pub artifact_id: String,
    pub snapshot_id: String,
    pub revision: u64,
    pub dpi: u32,
    pub texture: TextureHandle,
    /// Physical pixel size of the texture.
    pub px: Vec2,
    /// Shared, so the board reads it each frame without copying its source map.
    pub page: std::sync::Arc<RenderPage>,
    /// Where the engine drew each node of the page (page EMU): the engine's own frames (`scene.metadata["node_frames"]`,
    /// asked with `include_node_frames`), else the frames read from the scene (`multi::laid_out_frames`).
    pub frames: std::collections::BTreeMap<String, designllm_contracts::Rect>,
}

/// What a drag moves and the screen step it is drawn at (`DesktopApp::drag_shift`), kept in the frame memory under `DRAG_LANDING`
/// from the drag until the page drawn after the drop arrives. `at` is the input time of the drag's last frame.
#[derive(Clone)]
struct DragLanding {
    node_ids: Vec<String>,
    page_index: u32,
    delta: Vec2,
    base_revision: u64,
    at: f64,
}

/// What lay under the pointer when the current press began (`hovered`, egui's top widgets then) and which of those are the
/// board's own (`board`: its surface, node regions, memos, selection handles), so a pan starts only from the board and not
/// from a button, a floating bar or a popup over it. Kept in the frame memory under `BOARD_PRESS`.
#[derive(Clone, Default)]
struct BoardPress {
    hovered: Vec<Id>,
    board: Vec<Id>,
    on_board_layer: bool,
}

const BOARD_PRESS: &str = "board-press";

/// Notes that a board widget lies under a press that begins this frame (`BoardPress`).
pub(super) fn note_board_press(ctx: &Context, response: &Response) {
    if response.hovered() && ctx.input(|input| input.pointer.any_pressed()) {
        ctx.data_mut(|data| data.get_temp_mut_or_default::<BoardPress>(Id::new(BOARD_PRESS)).board.push(response.id));
    }
}

const DRAG_LANDING: &str = "drag-landing";
/// How long (seconds) a drop keeps its moved pixels at most while the page of its change is on its way.
const DRAG_LANDING_HOLD: f64 = 10.0;
/// The artboard veil over a dragged node's own place (the share of the artboard colour).
const DRAG_VEIL: f32 = 0.5;

/// The engine's frames of a snapshot over the ones read from `page`'s regions and primitives.
pub(super) fn snapshot_frames(snapshot: &designllm_contracts::RenderSnapshot, page: &RenderPage) -> std::collections::BTreeMap<String, designllm_contracts::Rect> {
    let mut frames = super::multi::laid_out_frames(page);
    if let Some(engine) = snapshot
        .scene
        .metadata
        .get("node_frames")
        .and_then(|value| serde_json::from_value::<std::collections::BTreeMap<String, designllm_contracts::Rect>>(value.clone()).ok())
    {
        frames.extend(engine);
    }
    frames
}

/// The part of a page rendered again at the resolution it is shown at, and the page rectangle it covers.
pub(super) struct RegionTexture {
    pub region: designllm_contracts::Rect,
    pub dpi: u32,
    pub revision: u64,
    pub texture: TextureHandle,
    /// Physical pixel size of the texture.
    pub px: Vec2,
}

/// Whether a click on the board is a double click. egui counts any two clicks within 0.3 s as one, wherever they land
/// (`InputOptions::max_double_click_delay`), and a double click opens a text: a quick click on another object would open
/// that object's text instead of selecting it, and the clicks after it would only end the edit (68f item 2). It counts
/// only where the click before it landed (egui's own click distance, 6 points). Every board click passes here.
pub(super) fn board_double_click(ctx: &Context, position: Pos2, double: bool) -> bool {
    let id = Id::new("board-last-click");
    let last = ctx.data(|data| data.get_temp::<Pos2>(id));
    ctx.data_mut(|data| data.insert_temp(id, position));
    double && last.is_some_and(|last| last.distance(position) <= 6.0)
}

/// The default size of a page before the core has reported one.
pub(super) fn default_page_size(kind: ArtifactKind) -> Size {
    match kind {
        ArtifactKind::Figure => Size { width: 170 * designllm_contracts::EMU_PER_MM, height: 120 * designllm_contracts::EMU_PER_MM },
        ArtifactKind::WebCode => Size { width: 1440 * designllm_contracts::EMU_PER_CSS_PX, height: 900 * designllm_contracts::EMU_PER_CSS_PX },
        _ => Size::a4(),
    }
}

pub(super) fn node_page_index(artifact: &designllm_contracts::Artifact, node_id: &str) -> Option<u32> {
    let pages = match &artifact.body {
        designllm_contracts::ArtifactBody::Canvas(document) => &document.pages,
        designllm_contracts::ArtifactBody::Slides(document) => &document.pages,
        _ => return None,
    };
    pages.iter().position(|page| page.nodes.iter().any(|node| super::doc_tree::find_node_in_node(node, node_id).is_some()))
        .and_then(|index| u32::try_from(index).ok())
}

/// The zoom that fits content whose size depends on the zoom, and the content it fits. The gaps between pages are in screen points,
/// so the arrangement changes with the zoom: each round measures the content at the zoom it has, then fits that content. `None`
/// when there is no content at the first zoom.
pub(super) fn settle_zoom(content_at: impl Fn(f32) -> Option<Rect>, fit: impl Fn(Rect) -> f32, rounds: usize) -> Option<(f32, Rect)> {
    let mut zoom = 1.0_f32;
    let mut content = content_at(zoom)?;
    for _ in 0..rounds {
        content = content_at(zoom).unwrap_or(content);
        zoom = fit(content);
    }
    Some((zoom, content))
}

impl DesktopApp {
    /// The view of the open document.
    pub(super) fn view(&mut self) -> &mut EditorView {
        let key = self.active.as_ref().map(|item| item.id.clone()).unwrap_or_default();
        self.editor_views.entry(key).or_default()
    }

    pub(super) fn view_ref(&self) -> EditorView {
        let key = self.active.as_ref().map(|item| item.id.as_str()).unwrap_or("");
        self.editor_views.get(key).cloned().unwrap_or_default()
    }

    /// The sizes of the document's pages in order. Canvases and slides know them from the artifact;
    /// the others use the size the core reported for each page, or the kind's default.
    pub(super) fn page_sizes(&self) -> Vec<Size> {
        let Some(active) = self.active.as_ref() else {
            return Vec::new();
        };
        if let Some(artifact) = self.artifact.as_ref() {
            match &artifact.body {
                designllm_contracts::ArtifactBody::Canvas(document) => {
                    return document.pages.iter().map(|page| page.size).collect();
                }
                designllm_contracts::ArtifactBody::Slides(document) => {
                    return document.pages.iter().map(|page| page.size).collect();
                }
                _ => {}
            }
        }
        let count = self.total_pages.max(1);
        (0..count)
            .map(|index| {
                self.known_sizes
                    .get(&index)
                    .copied()
                    .unwrap_or_else(|| default_page_size(active.kind))
            })
            .collect()
    }

    /// Fits the board to the document: the whole content for canvases, slides and figures (at most
    /// 100 %), and the page width for documents, with the top of the first page near the top.
    pub(super) fn fit_view(&mut self, board_rect: Rect) {
        let Some(kind) = self.active.as_ref().map(|item| item.kind) else {
            return;
        };
        let sizes = self.page_sizes();
        let slots = board::arrange(kind, &sizes, 1.0);
        let Some(content) = board::bounds(&slots) else {
            return;
        };
        let flow = matches!(kind, ArtifactKind::Docx | ArtifactKind::Hwpx | ArtifactKind::Hwp);
        let (zoom, origin) = if flow {
            let widest = content.width().max(1.0);
            let zoom = ((board_rect.width() - 96.0) / widest).clamp(board::ZOOM_MIN, 1.0);
            let origin = Pos2::new(
                board_rect.center().x - content.center().x * zoom,
                // docx-page: the first page starts 56 below the board top (under the 12 + 36 view group, 8 apart).
                board_rect.top() + FLOW_TOP - content.top() * zoom,
            );
            (zoom, origin)
        } else if kind == ArtifactKind::Figure {
            let zoom = board::fit_zoom(content, board_rect, 1.0);
            (zoom, board::centred_origin(content, board_rect, zoom))
        } else {
            // canvas-board, slides-board: the pages sit under the view group (title rows at board top + 52, pages
            // at + 76), centred across with 28 on each side, clear of the tool bar (16 + 42 + 16 at the bottom).
            let room = Rect::from_min_max(
                Pos2::new(board_rect.left() + FIT_SIDE, board_rect.top() + FIT_TOP),
                Pos2::new(board_rect.right() - FIT_SIDE, board_rect.bottom() - FIT_BOTTOM),
            );
            // The screen gaps depend on the zoom (§4), so the fit is settled in a few rounds.
            let Some((zoom, content)) = settle_zoom(
                |zoom| board::bounds(&board::arrange(kind, &sizes, zoom)),
                |content| (room.width() / content.width().max(1.0)).min(room.height().max(1.0) / content.height().max(1.0)).clamp(board::ZOOM_MIN, 1.0),
                6,
            ) else {
                return;
            };
            let origin = Pos2::new(board_rect.center().x - content.center().x * zoom, room.top() - content.top() * zoom);
            (zoom, origin)
        };
        let view = self.view();
        view.zoom = zoom;
        view.pan = origin - board_rect.min;
        view.fitted = true;
        view.zoom_changed = std::time::Instant::now();
    }

    /// Moves or zooms the board from pointer and wheel input. Zoom keeps the point under the
    /// pointer where it is.
    /// Returns true when a pan moved the board this frame (a drag that moved, or the wheel).
    ///
    /// The pan reads the pointer itself, not the board's response: egui gives a press to the topmost widget that senses a
    /// drag, whatever the button, so over an object, a memo or a handle the board would not see the drag. A pan is a
    /// middle-button drag, or a primary drag with Space or the hand tool, that began on the board; the regions, memos
    /// and handles take no press meanwhile (`board_pan_mode`, `interact_with_region_passing`).
    fn board_input(&mut self, ctx: &Context, response: &Response, board_rect: Rect) -> bool {
        let (scroll, pinch, command, hover) = ctx.input(|input| {
            (input.smooth_scroll_delta, input.zoom_delta(), input.modifiers.command, input.pointer.hover_pos())
        });
        let pan_mode = self.board_pan_mode(ctx);
        let (middle, primary, pressed_now, origin, step) = ctx.input(|input| {
            let pointer = &input.pointer;
            (pointer.middle_down(), pointer.primary_down(), pointer.any_pressed(), pointer.press_origin(), pointer.delta())
        });
        if pressed_now {
            // What the press began on, filled in by the board's widgets this frame (`note_board_press`).
            // Only the widgets that take presses: every `Ui` is a hover-only widget too, and those hold no press.
            let hovered: Vec<Id> = ctx.interaction_snapshot(|interaction| interaction.hovered.iter().copied().collect::<Vec<Id>>());
            let hovered = hovered
                .into_iter()
                .filter(|id| ctx.read_response(*id).is_some_and(|widget| widget.sense.senses_click() || widget.sense.senses_drag()))
                .collect();
            let on_board_layer = origin.is_some_and(|origin| board_rect.contains(origin) && ctx.layer_id_at(origin) == Some(response.layer_id));
            ctx.data_mut(|data| data.insert_temp(Id::new(BOARD_PRESS), BoardPress { hovered, board: Vec::new(), on_board_layer }));
            note_board_press(ctx, response);
        }
        // A pan begins on the board itself: not on a popup, a floating bar, a page title button or another widget over it.
        let began_here = ctx
            .data(|data| data.get_temp::<BoardPress>(Id::new(BOARD_PRESS)))
            .is_some_and(|press| press.on_board_layer && press.hovered.iter().all(|id| press.board.contains(id)));
        // A primary drag already moving a node or drawing a marquee stays what it is when Space comes down.
        let primary_pan = pan_mode && primary && self.drag.is_none() && self.marquee.is_none() && self.board_memo_gesture.is_none();
        // The press frame's step is the move before the press, not a drag.
        let panning = began_here && !pressed_now && (middle || primary_pan);
        let mut moved = false;
        if panning && step != Vec2::ZERO {
            self.view().pan += step;
            moved = true;
        }
        // egui hovers only the topmost interactive widget under the pointer, so a node region over the board takes the
        // hover from the board's own response. The wheel (zoom, and the plain wheel's pan, as design tools do over objects) is the
        // board's when the pointer is on the board's layer, not over a panel or a popup, and no other widget is being dragged.
        let other_drag = ctx.dragged_id().is_some_and(|id| id != response.id);
        let over_board = !other_drag && hover.is_some_and(|pos| board_rect.contains(pos) && ctx.layer_id_at(pos) == Some(response.layer_id));
        if !over_board {
            return moved;
        }
        let zoom_factor = if (pinch - 1.0).abs() > 1e-4 {
            pinch
        } else if command && scroll.y.abs() > 0.0 {
            (scroll.y / 240.0).exp()
        } else {
            1.0
        };
        if (zoom_factor - 1.0).abs() > 1e-4 {
            let anchor = hover.unwrap_or(board_rect.center());
            self.zoom_about(board_rect, anchor, zoom_factor);
        } else if !command && scroll != Vec2::ZERO {
            self.view().pan += scroll;
            moved = true;
        }
        moved
    }

    /// Makes `page_index` the current page, as `nav.page` does (the sidebar follows; new objects go there).
    pub(super) fn focus_page(&mut self, page_index: u32) {
        if self.view_ref().page_index == page_index {
            return;
        }
        self.view().page_index = page_index;
        self.page_index = page_index;
        self.remember_pending_page_reorder_selection(page_index);
    }

    /// Multiplies the zoom by `factor`, keeping the board point under `anchor` fixed.
    pub(super) fn zoom_about(&mut self, board_rect: Rect, anchor: Pos2, factor: f32) {
        let view = self.view();
        let old_origin = board_rect.min + view.pan;
        let new_zoom = (view.zoom * factor).clamp(board::ZOOM_MIN, board::ZOOM_MAX);
        if (new_zoom - view.zoom).abs() < 1e-6 {
            return;
        }
        let ratio = new_zoom / view.zoom;
        let new_origin = anchor - (anchor - old_origin) * ratio;
        view.pan = new_origin - board_rect.min;
        view.zoom = new_zoom;
        view.fitted = true;
        view.zoom_changed = std::time::Instant::now();
    }

    /// Sets the zoom to `zoom` around the board centre (menu commands).
    pub(super) fn set_zoom(&mut self, board_rect: Rect, zoom: f32) {
        let current = self.view_ref().zoom.max(board::ZOOM_MIN);
        self.zoom_about(board_rect, board_rect.center(), zoom / current);
    }

    /// The board origin in screen points for the current pan.
    /// Asks the core for `page_index` at `dpi`, once per (dpi, revision).
    pub(super) fn request_page(&mut self, page_index: u32, wanted_dpi: u32) {
        let Some(active) = self.active.as_ref() else {
            return;
        };
        let revision = active.revision;
        let size = self
            .page_sizes()
            .get(page_index as usize)
            .copied()
            .unwrap_or_else(|| default_page_size(active.kind));
        let dpi = board::capped_dpi_within(wanted_dpi, size, self.max_texture_side);
        if self.requested_pages.get(&page_index) == Some(&(dpi, revision)) {
            return;
        }
        self.requested_pages.insert(page_index, (dpi, revision));
        let request = RenderRequest {
            artifact_id: active.id.clone(),
            expected_revision: revision,
            page_index,
            profile: RenderProfile { dpi, transparent: false },
            region: None,
            // The selection, marquee, drag and lint fixes read where the engine drew each node.
            include_node_frames: true,
        };
        self.call(
            "render.get",
            serde_json::to_value(request).unwrap_or(Value::Null),
            PendingAction::Render { page_index },
        );
    }

    /// Draws the board in the central area and handles its input.
    pub(super) fn draw_board(&mut self, ctx: &Context, ui: &mut Ui) {
        let palette = theme::palette(ctx);
        let board_rect = ui.max_rect();
        self.board_rect = Some(board_rect);
        self.max_texture_side = ctx.input(|input| input.max_texture_side);
        ui.painter().rect_filled(board_rect, CornerRadius::ZERO, palette.board);
        let response = ui.interact(board_rect, ui.id().with("board-surface"), Sense::click_and_drag());
        if self.active.as_ref().is_some_and(|active| active.kind == ArtifactKind::WebCode) {
            self.draw_web_placeholder(ui, board_rect);
            return;
        }
        if self.artifact.is_none() {
            return;
        }
        self.tick_inline_cells(ctx);
        if self.inline_text_edit.is_none()
            && !self.host_lost()
            && !self.key_dialog_open()
            && self.view_ref().tool != Tool::Memo
            && !ctx.wants_keyboard_input()
            && ctx.input(|input| input.key_pressed(egui::Key::Enter)
                && !input.modifiers.ctrl && !input.modifiers.alt && !input.modifiers.mac_cmd)
            && self.selected_nodes.len() == 1
        {
            if let (Some(node_id), Some(artifact)) = (self.selected_nodes.first().cloned(), self.artifact.as_ref()) {
                if let Some(page_index) = node_page_index(artifact, &node_id) {
                    // A table opens its cell edited last (else its first cell), 68 item 11.
                    if self.begin_text_edit_of_selection(ctx).unwrap_or_else(|| self.begin_inline_text_edit(ctx, &node_id, page_index, None)) {
                        ctx.input_mut(|input| { input.consume_key(input.modifiers, egui::Key::Enter); });
                    }
                }
            }
        }
        let fitting = !self.view_ref().fitted;
        if fitting {
            self.fit_view(board_rect);
        }
        self.reveal_pending_canvas_find();
        let moving = self.inline_text_edit.is_none() && self.board_input(ctx, &response, board_rect);
        // A fresh fit, and any zoom once it has settled, moves (by at most half a dpi) to the zoom whose render
        // resolution is a whole dpi, so the page texture maps 1:1 to physical pixels and the first render is already
        // the final one. The board origin stays where it is.
        let ppp = ctx.pixels_per_point();
        // A memo drag keeps the zoom it started with: the settle (the zoom snap, the sharp render and the region) waits for the
        // drag's end, since the snap moves the page under the drag. A drag whose release will not come is ended first, so the
        // settle is not held back for good.
        self.drop_lost_board_memo_gesture(ctx);
        let settle_due = self.view_ref().zoom_changed.elapsed() >= SETTLE && self.board_memo_gesture.is_none();
        if fitting || settle_due {
            let view = self.view();
            view.zoom = board::snapped_zoom(view.zoom, ppp);
        }
        let view = self.view_ref();
        let origin = board_rect.min + view.pan;
        let kind = self.active.as_ref().map_or(ArtifactKind::Canvas, |item| item.kind);
        let slots = board::arrange(kind, &self.page_sizes(), view.zoom);
        let wanted_dpi = board::dpi_for(view.zoom, ppp);
        self.board_dpi = wanted_dpi;
        let settled = settle_due;
        // While the board is being panned, no new region render is asked for (each quarter-page step of the pan would
        // ask for one) and the page transforms are not sent to the core (a hit test sends its own, `hit_test_page`).
        // A moving frame asks for the next one, so the first still frame after the pan asks for the sharp region once
        // and sends the current page's transform (`sync_page_transform`).
        let render_settled = settled && !moving;
        if moving {
            ctx.request_repaint();
        }
        for slot in &slots {
            let rect = board::to_screen(slot.world, origin, view.zoom);
            if !board::visible(rect, board_rect) {
                continue;
            }
            self.draw_page(ctx, ui, &palette, slot.page_index, rect, wanted_dpi, render_settled, !moving);
        }
        self.track_drag_landing(ctx);
        self.draw_drag_previews(ctx, ui, &slots, origin, view.zoom);
        if !settled {
            ctx.request_repaint_after(SETTLE);
        }
        if self.inline_text_edit.is_none()
            && view.tool != Tool::Memo
            && !self.board_pan_mode(ctx)
            && (response.clicked() || response.double_clicked())
            && !self.region_clicked
        {
            // A click on the board selects what its frame holds (the core's hit test covers shapes and
            // text alike); on empty board it clears the selection.
            // A click off the memos leaves the memo selection (a memo's own click marks `region_clicked`).
            self.selected_board_memo = None;
            self.perf_count("click_board");
            match response.interact_pointer_pos().and_then(|pos| page_at(&slots, origin, view.zoom, pos).map(|page| (page, pos))) {
                Some((page, pos)) => {
                    self.focus_page(page);
                    let double = board_double_click(ctx, pos, response.double_clicked());
                    self.hit_test_page(page, pos, ctx.pixels_per_point(), super::multi::additive_modifier(ctx), double);
                }
                None => self.select_clear(),
            }
        } else if self.inline_text_edit.is_some() && !self.board_pan_mode(ctx) && response.clicked() {
            let at = response.interact_pointer_pos().and_then(|pos| page_at(&slots, origin, view.zoom, pos).map(|page| (page, pos)));
            let page_rect = at.and_then(|(page, _)| slots.iter().find(|slot| slot.page_index == page)).map(|slot| board::to_screen(slot.world, origin, view.zoom));
            self.click_ends_inline_cell_edit(ctx, at.zip(page_rect).map(|((page, pos), rect)| (page, pos, rect)), ctx.pixels_per_point());
        }
        // A right click on a Canvas table opens its 셀 병합 / 병합 해제 menu (68 item 14). The click is read from the input (the
        // node regions over the page take it from the board's own response); the widget it landed on says what was pressed.
        let right_click = ctx.input(|input| input.pointer.secondary_clicked().then(|| input.pointer.interact_pos()).flatten())
            .filter(|_| ui.rect_contains_pointer(board_rect))
            .and_then(|pos| page_at(&slots, origin, view.zoom, pos).map(|page| (page, pos)))
            .and_then(|(page, pos)| slots.iter().find(|slot| slot.page_index == page).map(|slot| (page, pos, board::to_screen(slot.world, origin, view.zoom))));
        self.table_cell_context_menu(ctx, &response, right_click);
        self.region_clicked = false;
        if self.inline_text_edit.is_some() {
            self.draw_inline_text_conflict(ctx, ui);
            return;
        }
        self.marquee_input(ctx, ui, &response, &palette, origin, view.zoom);
        self.draw_multi_selection(ctx, ui, &palette, origin, view.zoom);
        self.draw_selection(ctx, ui, &palette, origin, view.zoom);
        self.draw_reservations(ui, &palette, origin, view.zoom);
        self.draw_lint_markers(ctx, ui, &palette, origin, view.zoom);
        self.draw_target_refusal(ctx, ui, &palette, origin, view.zoom);
    }

    /// One page: the artboard, its title, the texture, the node regions. Requests a render when
    /// the page has none, its revision moved on, or the zoom has settled at a new resolution.
    #[allow(clippy::too_many_arguments)]
    fn draw_page(
        &mut self,
        ctx: &Context,
        ui: &mut Ui,
        palette: &theme::Palette,
        page_index: u32,
        rect: Rect,
        wanted_dpi: u32,
        settled: bool,
        send_transform: bool,
    ) {
        let revision = self.active.as_ref().map_or(0, |item| item.revision);
        let kind = self.active.as_ref().map_or(ArtifactKind::Canvas, |item| item.kind);
        let flow = matches!(kind, ArtifactKind::Docx | ArtifactKind::Hwpx | ArtifactKind::Hwp);
        let selected_page = self.view_ref().page_index == page_index;
        let painter = ui.painter().clone();
        // tokens `component.artboard`: white, the artboard shadow (a 1 px ring and a soft drop), no radius.
        theme::paint_shadow(&painter, rect, CornerRadius::ZERO, theme::Elevation::Artboard);
        painter.rect_filled(rect, CornerRadius::ZERO, palette.artboard);
        // The title row: 20 high, 4 above the artboard, 12 Regular; the current page's in `ink` (others `ink-2`).
        // A figure is one sheet without a title row (figure-board).
        if !flow && kind != ArtifactKind::Figure {
            let title = self.page_title(page_index);
            let title_ink = if selected_page { palette.ink } else { palette.ink_2 };
            let row = Rect::from_min_max(
                Pos2::new(rect.left(), rect.top() - theme::ARTBOARD_TITLE_GAP - theme::ARTBOARD_TITLE_HEIGHT),
                Pos2::new(rect.right(), rect.top() - theme::ARTBOARD_TITLE_GAP),
            );
            let title_action = selected_page && kind == ArtifactKind::Canvas && self.board_titles_write_supported();
            let action_count = 2 + if title_action { 1 } else { 0 };
            let actions = if selected_page { action_count as f32 * theme::MINI_BUTTON + (action_count - 1) as f32 * 2.0 + 6.0 } else { 0.0 };
            let shown = super::chrome::truncate_font(ui, &title, (row.width() - actions).max(0.0), &theme::font(theme::Type::BoardTitle));
            ui.painter().text(Pos2::new(row.left(), row.center().y), egui::Align2::LEFT_CENTER, shown, theme::font(theme::Type::BoardTitle), title_ink);
            if selected_page && row.width() > actions + 40.0 {
                self.artboard_actions(ui, &palette, page_index, row);
            }
        }
        // The texture on screen is kept until its replacement arrives. A new revision is asked for
        // at once and keeps the resolution; a new zoom asks for the sharper page only when settled.
        let current = self.page_textures.get(&page_index).map(|page| (page.dpi, page.revision, page.texture.clone(), page.px));
        let active_id = self.active.as_ref().map(|active| active.id.as_str());
        let revision_moved = current.as_ref().is_none_or(|(_, seen, _, _)| *seen != revision)
            || self.page_textures.get(&page_index).is_none_or(|page| Some(page.artifact_id.as_str()) != active_id);
        let dpi_moved = current.as_ref().is_some_and(|(dpi, _, _, _)| *dpi != wanted_dpi) && settled;
        let table_cell_waiting_for_scene = self.inline_text_edit.as_ref()
            .filter(|edit| edit.page_index == page_index && edit.reservation_node_ids.len() > 1)
            .is_some_and(|edit| matches!(
                self.inline_table_cell_readiness(edit),
                super::InlineCellReadiness::WaitingForRender | super::InlineCellReadiness::RevisionDrift
            ));
        if self.inline_text_edit.as_ref().is_none_or(|edit| edit.page_index != page_index) || table_cell_waiting_for_scene {
            match (&current, revision_moved, dpi_moved) {
                (None, _, _) => self.request_page(page_index, wanted_dpi),
                (Some((dpi, _, _, _)), true, _) => self.request_page(page_index, *dpi),
                (Some(_), false, true) => self.request_page(page_index, wanted_dpi),
                _ => {}
            }
        }
        let Some((dpi, _, texture, px)) = current else {
            return;
        };
        // At a settled zoom the texture has the page's physical pixel size: it is drawn 1:1 from a corner on the
        // pixel grid, so no glyph or line is resampled. While zooming (or above the texture cap) it is stretched.
        let ppp = ctx.pixels_per_point();
        let shown = if board::pixel_exact(dpi, self.view_ref().zoom, ppp) { board::pixel_rect(rect.min, px, ppp) } else { rect };
        painter.image(texture.id(), shown, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), Color32::WHITE);
        self.draw_page_guides(&painter, palette, page_index, rect);
        self.draw_page_region(&painter, page_index, rect, wanted_dpi, settled, revision);
        self.draw_drag_veils(ctx, &painter, palette, page_index, shown);
        // slides-board: the current slide has the selection outline (1.5 px accent, inside).
        if kind == ArtifactKind::Slides && selected_page {
            painter.rect_stroke(rect, CornerRadius::ZERO, egui::Stroke::new(1.5, palette.accent), egui::StrokeKind::Inside);
        }
        self.sync_page_transform(page_index, rect, px, ctx.pixels_per_point(), send_transform);
        if let Some(page) = self.page_textures.get(&page_index).map(|page| page.page.clone()) {
            if kind == ArtifactKind::Figure {
                self.draw_figure_overlays(ui, page.size, rect);
            } else {
                let memo_tool = kind == ArtifactKind::Canvas && self.view_ref().tool == Tool::Memo;
                if !memo_tool {
                    self.draw_page_regions(ctx, ui, page_index, &page, rect);
                }
                if kind == ArtifactKind::Canvas {
                    let memo_page_size = self.artifact.as_ref().and_then(|artifact| match &artifact.body {
                        designllm_contracts::ArtifactBody::Canvas(document) => document.pages.get(page_index as usize).map(|page| page.size),
                        _ => None,
                    }).unwrap_or(page.size);
                    self.draw_board_memos(ctx, ui, page_index, memo_page_size, rect);
                }
                self.draw_inline_text_editor(ctx, ui, page_index, page.size, rect);
            }
        }
    }

    /// The name shown above a page: the page's own name or its number. A slide is its number and its
    /// name or first text (§8: '번호 + 첫 제목 글').
    pub(super) fn page_title(&self, page_index: u32) -> String {
        match self.artifact.as_ref().map(|artifact| &artifact.body) {
            Some(designllm_contracts::ArtifactBody::Canvas(document)) => document
                .pages
                .get(page_index as usize)
                .and_then(|page| page.name.clone())
                .unwrap_or_else(|| format!("페이지 {}", page_index + 1)),
            Some(designllm_contracts::ArtifactBody::Slides(document)) => {
                let title = document.pages.get(page_index as usize).and_then(|page| {
                    page.name.clone().or_else(|| first_text(&page.nodes).map(|text| text.lines().next().unwrap_or_default().trim().to_owned()))
                });
                match title.filter(|title| !title.is_empty()) {
                    Some(title) => format!("{}  {title}", page_index + 1),
                    None => format!("{}", page_index + 1),
                }
            }
            _ => format!("페이지 {}", page_index + 1),
        }
    }

    /// The two mini buttons at the right end of the current page's title row: show only this page, export it.
    fn artboard_actions(&mut self, ui: &mut Ui, palette: &theme::Palette, page_index: u32, row: Rect) {
        let show_title = self.active.as_ref().is_some_and(|active| active.kind == ArtifactKind::Canvas)
            && self.board_titles_write_supported();
        let button_count = 2 + if show_title { 1 } else { 0 };
        let width = button_count as f32 * theme::MINI_BUTTON + (button_count - 1) as f32 * 2.0;
        let size = Vec2::new(width, theme::MINI_BUTTON);
        let area = Rect::from_min_size(Pos2::new(row.right() - size.x, row.top() - 2.0), size);
        let mut child = ui.new_child(egui::UiBuilder::new().max_rect(area).layout(egui::Layout::left_to_right(egui::Align::Center)));
        child.spacing_mut().item_spacing = Vec2::new(2.0, 0.0);
        let page_label = if self.active.as_ref().is_some_and(|item| item.kind == ArtifactKind::Slides) { "이 슬라이드" } else { "이 페이지" };
        if show_title && crate::widgets::mini_button(&mut child, palette, "tool-text", "보드 제목 편집").clicked() {
            self.region_clicked = true;
            self.open_board_title_editor(page_index);
        }
        if crate::widgets::mini_button(&mut child, palette, "focus", &format!("{page_label}만 보기")).clicked() {
            self.region_clicked = true;
            self.invoke_ui("view.zoom_page", serde_json::json!({ "page_index": page_index }));
        }
        if crate::widgets::mini_button(&mut child, palette, "export", &format!("{page_label} 내보내기")).clicked() {
            self.region_clicked = true;
            self.export_scope = String::from("page");
            self.smoke_open = Some(String::from("export"));
        }
    }

    /// Fits the board to one page (the title row's "이 페이지만 보기").
    pub(super) fn fit_page(&mut self, board_rect: Rect, page_index: u32) {
        let Some(kind) = self.active.as_ref().map(|item| item.kind) else {
            return;
        };
        let slots = board::arrange(kind, &self.page_sizes(), 1.0);
        let Some(slot) = slots.iter().find(|slot| slot.page_index == page_index) else {
            return;
        };
        let zoom = board::fit_zoom(slot.world, board_rect, 1.0);
        // Arrange again at that zoom: the screen gaps depend on it (§4), so the page's place does too.
        let slots = board::arrange(kind, &self.page_sizes(), zoom);
        let Some(slot) = slots.iter().find(|slot| slot.page_index == page_index) else {
            return;
        };
        let origin = board::centred_origin(slot.world, board_rect, zoom);
        let view = self.view();
        view.zoom = zoom;
        view.pan = origin - board_rect.min;
        view.fitted = true;
        view.page_index = page_index;
        view.zoom_changed = std::time::Instant::now();
        self.page_index = page_index;
    }

    /// Node regions of one page (click to select, drag to move), from the core's source map.
    fn draw_page_regions(&mut self, ctx: &Context, ui: &mut Ui, page_index: u32, page: &RenderPage, rect: Rect) {
        if self.inline_text_edit.is_some() {
            return;
        }
        // One walk of the page for the frame, not one lookup per region.
        let passing = self.pass_through_ids(page_index);
        for (index, source) in page.source_map.iter().enumerate() {
            let region = super::page_to_screen(source.bounds, page.size, rect);
            if region.width() < 1.0 || region.height() < 1.0 {
                continue;
            }
            let key = egui::Id::new(("scene-region", page_index, source.node_id.as_str(), index));
            let pass_through = passing.contains(&source.node_id);
            if let Some(click) = self.interact_with_region_passing(ui, key, page_index, rect, &source.node_id, source.bounds, page.size, region, pass_through) {
                self.region_clicked = true;
                self.focus_page(page_index);
                self.perf_count("click_region");
                self.hit_test_page(page_index, click.position, ctx.pixels_per_point(), super::multi::additive_modifier(ctx), click.double_clicked);
            }
        }
        // A table's border band over its edge cells: the table itself (68 item 11).
        self.draw_table_border_bands(ctx, ui, page_index, page, rect);
    }

    /// Keeps what a drag moves and the step it is drawn at (`DragLanding`, read by `drag_shift` and the previews): while
    /// the drag is under way, and after the drop until the page drawn at the change's revision arrives, so the drop does
    /// not show the nodes back at their old place meanwhile. A drop that changed nothing (no change on its way, the
    /// revision as before) lets it go at once. Run after the pages, whose regions moved the drag this frame.
    fn track_drag_landing(&self, ctx: &Context) {
        let id = Id::new(DRAG_LANDING);
        let now = ctx.input(|input| input.time);
        if let Some(drag) = self.drag.as_ref() {
            let mut node_ids = Vec::with_capacity(drag.others.len() + 1);
            node_ids.push(drag.node_id.clone());
            node_ids.extend(drag.others.iter().map(|(id, _)| id.clone()));
            let landing = DragLanding {
                node_ids,
                page_index: drag.source_page_index,
                delta: self.drag_preview_delta(drag),
                base_revision: drag.base_revision,
                at: now,
            };
            ctx.data_mut(|data| data.insert_temp(id, landing));
            return;
        }
        let Some(landing) = ctx.data(|data| data.get_temp::<DragLanding>(id)) else {
            return;
        };
        let old_page_shown = self.page_textures.get(&landing.page_index).is_some_and(|page| page.revision == landing.base_revision);
        let change_on_its_way = self.pending.values().any(|pending| pending.apply.is_some())
            || self.active.as_ref().is_some_and(|active| active.revision != landing.base_revision);
        if !(old_page_shown && change_on_its_way && now - landing.at < DRAG_LANDING_HOLD) {
            ctx.data_mut(|data| data.remove::<DragLanding>(id));
        }
    }

    /// The page and the screen step at which `node_id` is shown moved: by the drag under way, or by a drop the page does
    /// not show yet (`track_drag_landing`).
    pub(super) fn drag_shift(&self, ctx: &Context, node_id: &str) -> Option<(u32, Vec2)> {
        ctx.data(|data| data.get_temp::<DragLanding>(Id::new(DRAG_LANDING)))
            .filter(|landing| landing.node_ids.iter().any(|id| id == node_id))
            .map(|landing| (landing.page_index, landing.delta))
    }

    /// The ids a drag moves on `page_index` (the drag under way, else a drop not drawn yet).
    fn dragged_on(&self, ctx: &Context, page_index: u32) -> Vec<String> {
        match self.drag.as_ref() {
            Some(drag) if drag.source_page_index == page_index => {
                std::iter::once(drag.node_id.clone()).chain(drag.others.iter().map(|(id, _)| id.clone())).collect()
            }
            Some(_) => Vec::new(),
            None => ctx
                .data(|data| data.get_temp::<DragLanding>(Id::new(DRAG_LANDING)))
                .filter(|landing| landing.page_index == page_index && landing.delta != Vec2::ZERO)
                .map(|landing| landing.node_ids)
                .unwrap_or_default(),
        }
    }

    /// The screen rectangle of `node_id`'s drawn frame on a page picture shown at `shown`, inside the page.
    fn dragged_frame(page: &PageTexture, node_id: &str, shown: Rect) -> Option<Rect> {
        let frame = page.frames.get(node_id)?;
        let from = super::page_to_screen(*frame, page.page.size, shown).intersect(shown);
        (from.width() > 0.0 && from.height() > 0.0).then_some(from)
    }

    /// The dragged nodes' own places on a page are veiled half in the artboard colour, so what stays there reads as the
    /// node's old place, not as a second copy (the page picture holds no background without the node to show instead).
    fn draw_drag_veils(&self, ctx: &Context, painter: &egui::Painter, palette: &theme::Palette, page_index: u32, shown: Rect) {
        let ids = self.dragged_on(ctx, page_index);
        let Some(page) = self.page_textures.get(&page_index).filter(|_| !ids.is_empty()) else {
            return;
        };
        for id in &ids {
            if let Some(from) = Self::dragged_frame(page, id, shown) {
                painter.rect_filled(from, CornerRadius::ZERO, palette.artboard.gamma_multiply(DRAG_VEIL));
            }
        }
    }

    /// The dragged nodes' pixels follow the pointer without asking the core for anything: the part of the page picture under
    /// each node's drawn frame is drawn again at the drop's step (snapped as the drop will be, `drag_preview_delta`). Drawn
    /// after every page, so a node dragged over another page stays on top. The drop is one change, drawn by the core once
    /// it is made.
    fn draw_drag_previews(&self, ctx: &Context, ui: &Ui, slots: &[board::PageSlot], origin: Pos2, zoom: f32) {
        let Some(landing) = ctx.data(|data| data.get_temp::<DragLanding>(Id::new(DRAG_LANDING))) else {
            return;
        };
        if landing.delta == Vec2::ZERO {
            return;
        }
        let (Some(page), Some(slot)) = (self.page_textures.get(&landing.page_index), slots.iter().find(|slot| slot.page_index == landing.page_index)) else {
            return;
        };
        let rect = board::to_screen(slot.world, origin, zoom);
        let ppp = ctx.pixels_per_point();
        let shown = if board::pixel_exact(page.dpi, zoom, ppp) { board::pixel_rect(rect.min, page.px, ppp) } else { rect };
        let painter = ui.painter();
        for id in &landing.node_ids {
            let Some(from) = Self::dragged_frame(page, id, shown) else {
                continue;
            };
            let uv = Rect::from_min_max(
                Pos2::new((from.left() - shown.left()) / shown.width(), (from.top() - shown.top()) / shown.height()),
                Pos2::new((from.right() - shown.left()) / shown.width(), (from.bottom() - shown.top()) / shown.height()),
            );
            painter.image(page.texture.id(), from.translate(landing.delta), uv, Color32::WHITE);
        }
    }

    fn draw_inline_text_editor(&mut self, ctx: &Context, ui: &mut Ui, page_index: u32, page_size: Size, page_rect: Rect) {
        let Some(edit) = self.inline_text_edit.as_ref().filter(|edit| edit.page_index == page_index).cloned() else { return };
        let current_targets = self.artifact.as_ref()
            .and_then(|artifact| super::doc_tree::inline_text_target_node_ids(artifact, edit.kind, edit.page_index, &edit.node_id));
        if edit.reservation_node_ids.len() > 1 {
            let readiness = self.inline_table_cell_readiness(&edit);
            let waiting_on_shown_frame = readiness == super::InlineCellReadiness::WaitingForRender
                && self.page_textures.get(&page_index).is_some_and(|page| page.frames.contains_key(&edit.node_id));
            if !self.apply_inline_cell_readiness(edit.id, readiness) && !waiting_on_shown_frame { return; }
        } else if current_targets.as_ref() != Some(&edit.reservation_node_ids) {
            if let Some(session) = self.inline_text_edit.as_mut().filter(|session| session.id == edit.id) {
                session.conflict = Some("편집 대상이 바뀌었습니다. 입력은 유지했습니다".into());
                session.retry_allowed = false;
            }
            return;
        }
        let Some((node_kind, node_bounds, font_size, color, bold, align)) = self.artifact.as_ref()
            .and_then(|artifact| super::doc_tree::find_node(artifact, &edit.node_id))
            .map(|node| (
                node.kind,
                node.bounds,
                // font_size is EMU: EMU × screen points per EMU (page_to_screen's scale). The floor only guards a degenerate size.
                (node.style.font_size as f32 * page_rect.width() / page_size.width.max(1) as f32).clamp(1.0, 200.0),
                crate::widgets::parse_hex(&node.style.color).unwrap_or(theme::palette(ctx).ink),
                node.style.bold,
                match node.style.align.as_str() {
                    "center" => egui::Align::Center,
                    "right" => egui::Align::Max,
                    _ => egui::Align::Min,
                },
            ))
        else {
            if let Some(session) = self.inline_text_edit.as_mut() {
                session.conflict = Some("편집하던 텍스트가 삭제되었습니다. 입력은 유지했습니다".into());
                session.retry_allowed = false;
            }
            return;
        };
        let bounds = if edit.kind == designllm_contracts::ArtifactKind::Canvas && node_kind == designllm_contracts::NodeKind::Rectangle {
            let Some(bounds) = self.page_textures.get(&page_index).and_then(|page| page.frames.get(&edit.node_id)).copied() else {
                if let Some(session) = self.inline_text_edit.as_mut() {
                    session.conflict = Some("표 셀의 현재 배치를 찾지 못했습니다. 입력은 유지했습니다".into());
                    session.retry_allowed = false;
                }
                return;
            };
            bounds
        } else {
            node_bounds
        };
        let rect = super::page_to_screen(bounds, page_size, page_rect).intersect(page_rect);
        if rect.width() < 2.0 || rect.height() < 2.0 { return; }
        let palette = theme::palette(ctx);
        // shortcut: edit overlays use the artboard color; use a clean background render when nested fills or images must remain visible.
        ui.painter().rect_filled(rect, CornerRadius::ZERO, palette.artboard);
        let mut child = ui.new_child(egui::UiBuilder::new().max_rect(rect).id_salt(("inline-text", &edit.node_id)));
        child.set_min_size(rect.size());
        let output = crate::widgets::inline_edit(
            &mut child,
            &mut self.text_draft,
            self.text_editor_id,
            theme::display_font(bold, font_size),
            color,
            align,
            rect.size(),
            edit.conflict.is_none() || edit.composing,
        );
        self.text_adapter.record_layout(&output);
        if self.cell_wait_shown(ctx.input(|input| input.time)) { self.paint_cell_wait_hint(ui, rect); }
        if edit.conflict.is_none() || edit.composing {
            self.text_editor_focused = true;
            output.response.request_focus();
            if ctx.input(|input| input.key_pressed(egui::Key::Escape))
                && !self.text_adapter.is_ime_active()
                && !edit.ime_cancelled_frame
                && !self.key_dialog_open()
            {
                self.finish_inline_text_edit();
            }
        }
    }

    fn draw_inline_text_conflict(&mut self, ctx: &Context, _ui: &mut Ui) {
        let Some(edit) = self.inline_text_edit.as_ref()
            .filter(|edit| edit.conflict.is_some()).cloned() else { return };
        use crate::widgets::{DialogButton, DialogChoice};
        let palette = theme::palette(ctx);
        // A notice on the board, under the other notices: it takes no keys, so Esc and typing stay the inline editor's.
        let board = self.board_rect.unwrap_or_else(|| ctx.content_rect());
        let shown = crate::widgets::dialog_with_choices(ctx, &palette, Id::new("inline-text-conflict"), "", crate::widgets::DialogPlace::BoardTop(board), |ui| {
                ui.label(edit.conflict.as_deref().unwrap_or_default());
                if edit.retry_same_request {
                    ui.label("마지막 입력의 확정 결과가 불명확합니다. 재시도는 같은 request_id로 확인하며, 초안을 버려도 이미 확정된 입력은 남을 수 있습니다.");
                }
                ui.label("적용되지 않은 초안:");
                let preview: String = self.text_draft.chars().take(240).collect();
                ui.label(egui::RichText::new(preview).monospace());
                let can_copy = !edit.composing && !self.text_adapter.is_ime_active();
                let can_retry = edit.retry_allowed && !edit.queue.is_empty() && !edit.composing && !self.text_adapter.is_ime_active()
                    && edit.in_flight.is_none() && !edit.reservation_pending && !edit.layout_pending;
                let can_discard = !edit.composing && !self.text_adapter.is_ime_active()
                    && edit.in_flight.is_none() && !edit.reservation_pending;
                let mut choices = vec![
                    DialogChoice::new("초안 복사", DialogButton::Plain).enabled(can_copy),
                    DialogChoice::new("다시 시도", DialogButton::Primary).enabled(can_retry),
                    DialogChoice::new("초안 버리기", DialogButton::Danger).enabled(can_discard),
                ];
                if edit.reservation_release_failed {
                    choices.insert(0, DialogChoice::new("예약 해제 다시 시도", DialogButton::Plain));
                }
                ((), choices)
            });
        let pressed = shown.inner.1;
        let last = pressed.len() - 1;
        let (copy_draft, retry, discard) = (pressed[last - 2], pressed[last - 1], pressed[last]);
        let retry_release = edit.reservation_release_failed && pressed[0];
        if copy_draft { ctx.copy_text(self.text_draft.clone()); }
        if retry { self.retry_inline_text_edits(); }
        if retry_release { self.release_inline_text_target(edit.id); }
        if discard { self.discard_inline_text_draft(); }
    }

    /// Records the page's transform so that hit tests map screen points to the page, and sends it to the core when the
    /// placement changed and `send` holds (not while the board pans: `hit_test_page` sends the one it needs). The first
    /// transform after a document opens always goes; and once the board is still, the current page's goes when the core
    /// holds another (after a pan, or another page's last change), so the core's view matches the screen.
    fn sync_page_transform(&mut self, page_index: u32, rect: Rect, px: Vec2, ppp: f32, send: bool) {
        if px.x <= 0.0 {
            return;
        }
        let zoom = rect.width() * ppp / px.x;
        let dpi = self.page_textures.get(&page_index).map_or(board::DPI_MIN, |page| page.dpi);
        let unchanged = self.page_transforms.get(&page_index).is_some_and(|sent| {
            sent.dpi == dpi
                && sent.zoom_milli == (zoom * 1000.0).round().clamp(1.0, u32::MAX as f32) as u32
                && sent.offset_x_micropx == to_micropx(rect.left() * ppp)
                && sent.offset_y_micropx == to_micropx(rect.top() * ppp)
        });
        self.page_rect = Some(rect);
        if unchanged {
            let current = self.page_transforms.get(&page_index).filter(|transform| {
                send && page_index == self.view_ref().page_index && self.core_transform_id.as_ref() != Some(&transform.id)
            });
            if let Some(transform) = current.cloned() {
                self.send_page_transform(page_index, &transform);
            }
            return;
        }
        let transform = ViewTransform {
            id: new_id(),
            dpi,
            zoom_milli: (zoom * 1000.0).round().clamp(1.0, u32::MAX as f32) as u32,
            offset_x_micropx: to_micropx(rect.left() * ppp),
            offset_y_micropx: to_micropx(rect.top() * ppp),
            crop: None,
        };
        if send || self.core_transform_id.is_none() {
            self.send_page_transform(page_index, &transform);
        }
        self.page_transforms.insert(page_index, transform);
    }

    /// After a refused transform or hit test the board no longer knows which transform the core holds: no transform has an
    /// empty id, so the next hit test sends its own first. A refused transform (the last one sent) is not sent again, so a
    /// core that keeps refusing it gets it once and the window shows the refusal once; the next placement makes a new one.
    pub(super) fn forget_core_transform_on_failure(&mut self, action: &PendingAction) {
        match action {
            PendingAction::Transform => {
                self.failed_transform_id = self.core_transform_id.take().filter(|id| !id.is_empty());
                self.core_transform_id = Some(String::new());
            }
            PendingAction::HitTest { .. } => self.core_transform_id = Some(String::new()),
            _ => {}
        }
    }

    /// Sends a page's transform to the core. The core keeps one transform per view, for one page (`ViewState.transform`),
    /// so the board remembers which one it holds. One the core refused is not sent again.
    fn send_page_transform(&mut self, page_index: u32, transform: &ViewTransform) {
        if self.failed_transform_id.as_ref() == Some(&transform.id) {
            return;
        }
        self.core_transform_id = Some(transform.id.clone());
        let view_id = self.view_id.clone();
        self.call(
            "view.transform.set",
            serde_json::json!({
                "view_id": view_id,
                "page_index": page_index,
                "transform": transform,
            }),
            PendingAction::Transform,
        );
    }

    /// Asks the core which node is at `position` on `page_index`. When the core holds another page's transform (every
    /// page on screen sends its own, and the last one sent stays), this page's goes first, or the hit test is refused.
    pub(super) fn hit_test_page(&mut self, page_index: u32, position: Pos2, pixels_per_point: f32, additive: bool, edit_text: bool) {
        self.cell_nav.note_hit(page_index, position, pixels_per_point, additive, edit_text);
        let (Some(page), Some(transform)) = (self.page_textures.get(&page_index), self.page_transforms.get(&page_index)) else {
            if edit_text { self.set_error("페이지를 그리는 중입니다. 잠시 뒤 다시 누르세요".into()); }
            self.perf_count("hit_quiet_no_page");
            return;
        };
        let snapshot_id = page.snapshot_id.clone();
        let transform_id = transform.id.clone();
        let Some(context) = self.context.as_ref() else {
            self.perf_count("hit_quiet_no_context");
            return;
        };
        let resend = self.core_transform_id.as_ref().is_some_and(|held| *held != transform_id).then(|| transform.clone());
        let query = designllm_contracts::RenderQuery {
            artifact_id: context.artifact_id.clone(),
            snapshot_id: snapshot_id.clone(),
            view_id: Some(self.view_id.clone()),
            transform_id: transform_id.clone(),
            page_index,
            query: designllm_contracts::RenderQueryKind::HitTest {
                point: designllm_contracts::ScreenPoint {
                    x_micropx: to_micropx(position.x * pixels_per_point),
                    y_micropx: to_micropx(position.y * pixels_per_point),
                },
            },
        };
        match serde_json::to_value(query) {
            Ok(params) => {
                if let Some(transform) = resend {
                    self.send_page_transform(page_index, &transform);
                }
                self.call(
                    "render.hit_test",
                    params,
                    PendingAction::HitTest { snapshot_id, page_index, transform_id, additive, edit_text },
                );
            }
            Err(_) => self.set_error(String::from("선택을 처리하지 못했습니다")),
        }
    }

    /// Stand-in for the web page: its preview is a separate window (WebView2), opened from here.
    fn draw_web_placeholder(&mut self, ui: &mut Ui, board_rect: Rect) {
        self.draw_web_mapping_entry(ui, board_rect);
        let palette = theme::palette(ui.ctx());
        let center = board_rect.center();
        // GUI_DESIGN §8: without a preview engine the board shows its reason in one line; the code editor stays.
        if let Some(reason) = self.web_runtime_unavailable.as_deref() {
            ui.painter().text(center, egui::Align2::CENTER_CENTER, reason, theme::font(theme::Type::Body), palette.ink_2);
            return;
        }
        let rect = Rect::from_center_size(center, Vec2::new(200.0, theme::CONTROL_HEIGHT));
        let mut child = ui.new_child(egui::UiBuilder::new().max_rect(rect));
        if crate::widgets::labeled_button(&mut child, &palette, Some("play"), "웹 미리보기 열기", true).clicked() {
            self.start_materialized_web_preview();
        }
    }

    /// A page shown larger than one texture may be (`board::MAX_PAGE_PIXELS`) is drawn from its whole-page texture
    /// at a lower resolution, with the part on screen rendered again at the physical resolution
    /// (`RenderRequest.region`, U7, §5.6). The region is asked for once the zoom has settled; until it arrives the
    /// whole-page texture shows.
    fn draw_page_region(&mut self, painter: &egui::Painter, page_index: u32, rect: Rect, wanted_dpi: u32, settled: bool, revision: u64) {
        let Some(page_size) = self.page_textures.get(&page_index).map(|page| page.page.size) else {
            return;
        };
        let Some(board_rect) = self.board_rect else {
            return;
        };
        if board::capped_dpi_within(wanted_dpi, page_size, self.max_texture_side) >= wanted_dpi {
            self.region_textures.remove(&page_index);
            return;
        }
        let Some(wanted) = board::visible_region(rect, board_rect, page_size) else {
            return;
        };
        // The region on screen now, if it covers what is visible at this resolution and revision.
        if let Some(region) = self.region_textures.get(&page_index) {
            let fresh = region.dpi == wanted_dpi && region.revision == revision;
            // While zooming or panning a region of the shown revision stays at its old resolution; one of an older
            // revision would show content the page no longer has.
            if fresh || (!settled && region.revision == revision) {
                let mut screen = super::page_to_screen(region.region, page_size, rect);
                let ppp = painter.ctx().pixels_per_point();
                if board::pixel_exact(region.dpi, self.view_ref().zoom, ppp) {
                    screen = board::pixel_rect(screen.min, region.px, ppp);
                }
                painter.image(region.texture.id(), screen, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), Color32::WHITE);
            }
            if fresh && board::covers(region.region, wanted) {
                return;
            }
        }
        if !settled {
            return;
        }
        let mut request = board::padded_region(wanted, page_size);
        // The region must fit one texture too: past the GPU's side the padding goes first, then the region itself.
        let side_emu = (self.max_texture_side as f64 * 914_400.0 / f64::from(wanted_dpi)) as i64;
        if request.width > side_emu || request.height > side_emu {
            request = wanted;
        }
        if request.width > side_emu || request.height > side_emu {
            return;
        }
        let key = (request, wanted_dpi, revision);
        if self.requested_regions.get(&page_index) == Some(&key) {
            return;
        }
        self.requested_regions.insert(page_index, key);
        let Some(active) = self.active.as_ref() else {
            return;
        };
        let render = RenderRequest {
            artifact_id: active.id.clone(),
            expected_revision: revision,
            page_index,
            profile: RenderProfile { dpi: wanted_dpi, transparent: false },
            region: Some(request),
            include_node_frames: false,
        };
        self.call("render.get", serde_json::to_value(render).unwrap_or(Value::Null), PendingAction::RenderRegion { page_index });
    }

    /// Keeps a region texture (the snapshot's `preview_transform.crop` names its page rectangle).
    pub(super) fn store_region_image(&mut self, ctx: &Context, page_index: u32, snapshot: &designllm_contracts::RenderSnapshot, region: designllm_contracts::Rect, image: egui::ColorImage) {
        let (width, height) = (image.width() as u32, image.height() as u32);
        let started = std::time::Instant::now();
        let texture = ctx.load_texture(format!("page-{page_index}-region"), image, TextureOptions::LINEAR);
        self.perf_texture(width, height, started.elapsed());
        let px = Vec2::new(width as f32, height as f32);
        self.region_textures.insert(page_index, RegionTexture { region, dpi: snapshot.profile.dpi, revision: snapshot.revision, texture, px });
    }

    /// Records the page sizes the core reports, so the board can place pages it has not drawn yet.
    pub(super) fn remember_page_size(&mut self, page: &designllm_contracts::RenderPage) {
        self.known_sizes.insert(page.page_index, page.size);
    }

    /// Clears textures and transforms when another document opens (keeps the per-document view).
    pub(super) fn reset_board(&mut self) {
        self.page_textures.clear();
        self.region_textures.clear();
        self.requested_regions.clear();
        self.page_transforms.clear();
        self.core_transform_id = None;
        self.failed_transform_id = None;
        self.requested_pages.clear();
        self.known_sizes.clear();
        self.page_rect = None;
    }

    /// Texture for a page, decoded from the core's PNG.
    #[cfg(test)]
    pub(super) fn store_page_texture(&mut self, ctx: &Context, page_index: u32, snapshot: &designllm_contracts::RenderSnapshot, width: u32, height: u32, rgba: &[u8]) {
        let image = egui::ColorImage::from_rgba_unmultiplied([width as usize, height as usize], rgba);
        self.store_page_image(ctx, page_index, snapshot, image);
    }

    /// `store_page_texture` for a picture already in texture pixels (`RpcPayload::png_image`).
    pub(super) fn store_page_image(&mut self, ctx: &Context, page_index: u32, snapshot: &designllm_contracts::RenderSnapshot, image: egui::ColorImage) {
        let Some(render_page) = snapshot.scene.pages.iter().find(|page| page.page_index == page_index).cloned() else {
            return;
        };
        let (width, height) = (image.width() as u32, image.height() as u32);
        let started = std::time::Instant::now();
        let texture = ctx.load_texture(format!("page-{page_index}"), image, TextureOptions::LINEAR);
        self.perf_texture(width, height, started.elapsed());
        self.page_textures.insert(
            page_index,
            PageTexture {
                artifact_id: snapshot.artifact_id.clone(),
                snapshot_id: snapshot.id.clone(),
                revision: snapshot.revision,
                dpi: snapshot.profile.dpi,
                texture,
                px: Vec2::new(width as f32, height as f32),
                frames: snapshot_frames(snapshot, &render_page),
                page: std::sync::Arc::new(render_page),
            },
        );
    }
}

/// The text of the first text node, depth first.
fn first_text(nodes: &[designllm_contracts::Node]) -> Option<String> {
    nodes.iter().find_map(|node| {
        if node.kind == designllm_contracts::NodeKind::Text && !node.text.trim().is_empty() {
            Some(node.text.clone())
        } else {
            first_text(&node.children)
        }
    })
}

/// The page whose screen rectangle holds `pos`.
fn page_at(slots: &[board::PageSlot], origin: Pos2, zoom: f32, pos: Pos2) -> Option<u32> {
    slots
        .iter()
        .find(|slot| board::to_screen(slot.world, origin, zoom).contains(pos))
        .map(|slot| slot.page_index)
}


#[cfg(test)]
mod input_tests {
    //! Board input on a real host (`test_support::Window`): the pan of the middle button, the page a click makes current,
    //! the marquee from empty space, and the object and corner-handle drags that stay on the primary button.

    use super::super::test_support::{Host, Window};
    use crate::board::{arrange, to_screen};
    use designllm_contracts::{ArtifactBody, ArtifactKind, CanvasBoardMemo, CanvasBoardMemoKind, Intent, Node, Operation, Page, Rect as EmuRect, Size};
    use eframe::egui::{epaint::Shape, Event, Key, Modifiers, PointerButton, Pos2, Rect, TextureId, Vec2};
    use serde_json::json;
    use std::collections::BTreeSet;

    fn node(id: &str, x: i64, y: i64, width: i64, height: i64) -> Node {
        serde_json::from_value(json!({
            "id": id, "kind": "rectangle",
            "bounds": { "x": x, "y": y, "width": width, "height": height },
        }))
        .expect("a rectangle parses")
    }

    /// A canvas whose page `i` holds `pages[i]`.
    fn canvas(host: &mut Host, pages: &[Vec<Node>]) -> String {
        let id = host.create("canvas", "입력 시험");
        let first = match &host.artifact(&id).body {
            ArtifactBody::Canvas(document) => document.pages[0].id.clone(),
            _ => panic!("a canvas"),
        };
        let intents: BTreeSet<Intent> = [Intent::Content, Intent::Design].into_iter().collect();
        let mut operations = vec![
            Operation::InsertPage { index: 0, page: Page::new("page-1", Size::slide()) },
            Operation::RemovePage { page_id: first },
        ];
        for index in 1..pages.len() {
            operations.push(Operation::InsertPage { index: index as u32, page: Page::new(format!("page-{}", index + 1), Size::slide()) });
        }
        host.apply(&id, operations, intents.clone()).expect("the pages are made");
        for (index, nodes) in pages.iter().enumerate() {
            let page_id = format!("page-{}", index + 1);
            let operations = nodes.iter().cloned().map(|node| Operation::AddNode { page_id: page_id.clone(), parent_id: None, node }).collect();
            host.apply(&id, operations, intents.clone()).expect("the nodes are added");
        }
        id
    }

    /// The screen rectangle of page `index` as the window draws it now.
    fn page_rect(window: &Window, index: u32) -> Rect {
        let board = window.app.board_rect.expect("the board is drawn");
        let view = window.app.view_ref();
        let slots = arrange(ArtifactKind::Canvas, &window.app.page_sizes(), view.zoom);
        let slot = slots.iter().find(|slot| slot.page_index == index).expect("the page has a slot");
        to_screen(slot.world, board.min + view.pan, view.zoom)
    }

    /// The screen point of page EMU (`x`, `y`) on page `index`.
    fn at(window: &Window, index: u32, x: i64, y: i64) -> Pos2 {
        let rect = page_rect(window, index);
        let size = window.app.page_sizes()[index as usize];
        Pos2::new(rect.left() + x as f32 / size.width as f32 * rect.width(), rect.top() + y as f32 / size.height as f32 * rect.height())
    }

    /// The screen rectangle of node `id` as the window draws it now.
    fn frame_of(window: &Window, id: &str) -> Rect {
        let board = window.app.board_rect.expect("the board is drawn");
        let view = window.app.view_ref();
        window
            .app
            .node_frames(board.min + view.pan, view.zoom)
            .into_iter()
            .find_map(|(node_id, rect)| (node_id == id).then_some(rect))
            .expect("the node is drawn")
    }

    fn bounds_of(host: &mut Host, id: &str, node_id: &str) -> EmuRect {
        let artifact = host.artifact(id);
        let ArtifactBody::Canvas(document) = &artifact.body else { panic!("a canvas") };
        document.pages.iter().flat_map(|page| page.nodes.iter()).find(|node| node.id == node_id).expect("the node").bounds
    }

    fn button(position: Pos2, button: PointerButton, pressed: bool) -> Event {
        Event::PointerButton { pos: position, button, pressed, modifiers: Modifiers::NONE }
    }

    /// Presses `kind` at `from`, moves in steps to `to` and releases there, one frame per step.
    fn drag(window: &mut Window, host: &mut Host, kind: PointerButton, from: Pos2, to: Pos2) {
        window.frame(host, 1.0, vec![Event::PointerMoved(from), button(from, kind, true)]);
        window.frame(host, 1.0, vec![Event::PointerMoved(from + (to - from) * 0.5)]);
        window.frame(host, 1.0, vec![Event::PointerMoved(to)]);
        window.frame(host, 1.0, vec![Event::PointerMoved(to), button(to, kind, false)]);
    }

    #[test]
    fn middle_button_pans_the_board_and_neither_selects_nor_moves() {
        let mut host = Host::start();
        let id = canvas(&mut host, &[vec![node("n1", 1_000_000, 1_000_000, 3_000_000, 2_000_000)]]);
        let mut window = Window::open_with_previews(&mut host, &id);
        window.draw(&mut host, 1.0);
        let revision = host.artifact(&id).revision;
        let pan = window.app.view_ref().pan;
        let empty = page_rect(&window, 0).right_bottom() - Vec2::splat(6.0);
        drag(&mut window, &mut host, PointerButton::Middle, empty, empty + Vec2::new(60.0, 30.0));
        window.draw(&mut host, 1.0);
        assert_ne!(window.app.view_ref().pan, pan, "the middle drag pans the board");
        assert!(window.app.selected_nodes.is_empty(), "the middle drag starts no marquee");
        let on_node = at(&window, 0, 2_000_000, 2_000_000);
        let before = bounds_of(&mut host, &id, "n1");
        let pan = window.app.view_ref().pan;
        drag(&mut window, &mut host, PointerButton::Middle, on_node, on_node + Vec2::new(60.0, 30.0));
        window.draw(&mut host, 1.0);
        assert!((window.app.view_ref().pan - (pan + Vec2::new(60.0, 30.0))).length() < 0.01, "a middle drag that starts on an object pans the board");
        assert_eq!(bounds_of(&mut host, &id, "n1"), before, "the middle drag moves no object");
        assert_eq!(host.artifact(&id).revision, revision, "no change, so no undo step");
    }

    #[test]
    fn a_click_on_a_page_makes_it_current_and_an_object_selects_it() {
        let mut host = Host::start();
        let id = canvas(
            &mut host,
            &[vec![node("n1", 1_000_000, 1_000_000, 3_000_000, 2_000_000)], vec![node("n2", 1_000_000, 1_000_000, 3_000_000, 2_000_000)]],
        );
        let mut window = Window::open_with_previews(&mut host, &id);
        window.draw(&mut host, 1.0);
        let revision = host.artifact(&id).revision;
        assert_eq!(window.app.view_ref().page_index, 0);
        let empty = page_rect(&window, 1).right_bottom() - Vec2::splat(6.0);
        window.click(&mut host, 1.0, empty, false);
        window.draw(&mut host, 1.0);
        assert_eq!(window.app.view_ref().page_index, 1, "an empty click on page 2 makes it the current page");
        let object = frame_of(&window, "n1").center();
        window.click(&mut host, 1.0, object, false);
        window.draw(&mut host, 1.0);
        assert_eq!(window.app.view_ref().page_index, 0, "a click on an object of page 1 makes page 1 current");
        assert_eq!(window.app.selected_nodes, vec!["n1".to_string()]);
        assert_eq!(host.artifact(&id).revision, revision, "a page change makes no undo step");
    }

    #[test]
    fn a_drag_from_empty_page_space_selects_the_objects_it_covers() {
        let mut host = Host::start();
        let id = canvas(
            &mut host,
            &[vec![node("n1", 1_000_000, 1_000_000, 3_000_000, 2_000_000), node("n2", 5_000_000, 1_000_000, 3_000_000, 2_000_000)]],
        );
        let mut window = Window::open_with_previews(&mut host, &id);
        window.draw(&mut host, 1.0);
        let to = at(&window, 0, 9_000_000, 4_000_000);
        let from = at(&window, 0, 200_000, 200_000);
        drag(&mut window, &mut host, PointerButton::Primary, from, to);
        window.draw(&mut host, 1.0);
        let mut selected = window.app.selected_nodes.clone();
        selected.sort();
        assert_eq!(selected, vec!["n1".to_string(), "n2".to_string()]);
        window.app.select_clear();
        window.draw(&mut host, 1.0);
        // Left of the page at the height of `from`, so the box spans the same objects (from the page's middle height
        // it would pass below them: they end at y 3 000 000 of 6 858 000).
        let outside = Pos2::new(page_rect(&window, 0).left() - 20.0, from.y);
        drag(&mut window, &mut host, PointerButton::Primary, outside, to);
        window.draw(&mut host, 1.0);
        let mut selected = window.app.selected_nodes.clone();
        selected.sort();
        assert_eq!(selected, vec!["n1".to_string(), "n2".to_string()], "a drag from off the page selects the same objects");
    }

    #[test]
    fn a_corner_handle_resizes_from_the_opposite_corner_in_one_change() {
        let mut host = Host::start();
        let id = canvas(&mut host, &[vec![node("n1", 1_000_000, 1_000_000, 3_000_000, 2_000_000)]]);
        let mut window = Window::open_with_previews(&mut host, &id);
        window.draw(&mut host, 1.0);
        window.click(&mut host, 1.0, frame_of(&window, "n1").center(), false);
        window.draw(&mut host, 1.0);
        let revision = host.artifact(&id).revision;
        let before = bounds_of(&mut host, &id, "n1");
        let corner = frame_of(&window, "n1").right_bottom();
        drag(&mut window, &mut host, PointerButton::Primary, corner, corner + Vec2::new(40.0, 20.0));
        window.draw(&mut host, 1.0);
        let after = bounds_of(&mut host, &id, "n1");
        assert_eq!((after.x, after.y), (before.x, before.y), "the opposite corner stays");
        assert!(after.width > before.width && after.height > before.height, "the dragged corner moves: {before:?} -> {after:?}");
        assert_eq!(host.artifact(&id).revision, revision + 1, "one change, one undo step");
    }

    #[test]
    fn a_primary_drag_on_the_body_moves_the_object_and_keeps_its_size() {
        let mut host = Host::start();
        let id = canvas(&mut host, &[vec![node("n1", 1_000_000, 1_000_000, 3_000_000, 2_000_000)]]);
        let mut window = Window::open_with_previews(&mut host, &id);
        window.draw(&mut host, 1.0);
        window.click(&mut host, 1.0, frame_of(&window, "n1").center(), false);
        window.draw(&mut host, 1.0);
        let before = bounds_of(&mut host, &id, "n1");
        let centre = frame_of(&window, "n1").center();
        drag(&mut window, &mut host, PointerButton::Primary, centre, centre + Vec2::new(40.0, 20.0));
        window.draw(&mut host, 1.0);
        let after = bounds_of(&mut host, &id, "n1");
        assert_ne!((after.x, after.y), (before.x, before.y), "the body drag moves the object");
        assert_eq!((after.width, after.height), (before.width, before.height), "a move keeps the size");
    }

    /// The number of `method` requests the window sent so far.
    fn sent(window: &Window, method: &str) -> usize {
        window.calls.iter().filter(|(sent, _, _)| sent == method).count()
    }

    fn holds(host: &mut Host, id: &str, node_id: &str) -> bool {
        let artifact = host.artifact(id);
        let ArtifactBody::Canvas(document) = &artifact.body else { panic!("a canvas") };
        document.pages.iter().flat_map(|page| page.nodes.iter()).any(|node| node.id == node_id)
    }

    /// A `Delete` key press, handled as `App::update` does: the board memo shortcut first, then the registry's.
    fn press_delete(window: &mut Window, host: &mut Host) {
        let mut input = super::super::test_support::screen(1.0, 1280.0, 800.0);
        input.events.push(Event::Key { key: Key::Delete, physical_key: Some(Key::Delete), pressed: true, repeat: false, modifiers: Modifiers::NONE });
        let app = &mut window.app;
        let _ = window.ctx.run(input, |ctx| {
            app.handle_board_memo_shortcut(ctx);
            app.handle_shortcuts(ctx);
        });
        window.settle(host);
        window.draw(host, 1.0);
    }

    /// The textured rectangles the last frame drew.
    fn images(window: &Window) -> Vec<Rect> {
        window
            .shapes
            .iter()
            .filter_map(|clipped| match &clipped.shape {
                Shape::Mesh(mesh) if mesh.texture_id != TextureId::default() => Some(mesh.calc_bounds()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_middle_pan_sends_no_transform_and_a_click_after_it_still_selects() {
        let mut host = Host::start();
        let id = canvas(&mut host, &[vec![node("n1", 1_000_000, 1_000_000, 3_000_000, 2_000_000)], vec![node("n2", 1_000_000, 1_000_000, 3_000_000, 2_000_000)]]);
        let mut window = Window::open_with_previews(&mut host, &id);
        window.draw(&mut host, 1.0);
        let (transforms, renders) = (sent(&window, "view.transform.set"), sent(&window, "render.get"));
        let empty = page_rect(&window, 0).right_bottom() - Vec2::splat(6.0);
        let to = empty + Vec2::new(60.0, 30.0);
        window.frame(&mut host, 1.0, vec![Event::PointerMoved(empty), button(empty, PointerButton::Middle, true)]);
        window.frame(&mut host, 1.0, vec![Event::PointerMoved(empty + (to - empty) * 0.5)]);
        window.frame(&mut host, 1.0, vec![Event::PointerMoved(to)]);
        assert_eq!(sent(&window, "view.transform.set"), transforms, "the moving frames send no page transform");
        assert_eq!(sent(&window, "render.get"), renders, "the pan asks for no render");
        window.frame(&mut host, 1.0, vec![Event::PointerMoved(to), button(to, PointerButton::Middle, false)]);
        window.draw(&mut host, 1.0);
        assert_eq!(sent(&window, "view.transform.set"), transforms + 1, "the first still frame sends the current page's transform once");
        let held = window.calls.iter().rev().find(|(method, _, _)| method == "view.transform.set").map(|(_, params, _)| params["transform"]["id"].clone());
        assert_eq!(held, Some(json!(window.app.page_transforms[&0].id)), "the core holds the current page as shown");
        window.click(&mut host, 1.0, frame_of(&window, "n1").center(), false);
        window.draw(&mut host, 1.0);
        assert_eq!(window.app.selected_nodes, vec!["n1".to_string()], "the click sends its page's transform and selects");
        window.click(&mut host, 1.0, frame_of(&window, "n2").center(), false);
        window.draw(&mut host, 1.0);
        assert_eq!(window.app.selected_nodes, vec!["n2".to_string()], "the other page's object too");
        assert_eq!(window.app.view_ref().page_index, 1);
    }

    #[test]
    fn a_dragged_object_shows_its_pixels_at_the_pointer_and_drops_in_one_change() {
        let mut host = Host::start();
        let id = canvas(&mut host, &[vec![node("n1", 1_000_000, 1_000_000, 3_000_000, 2_000_000)]]);
        let mut window = Window::open_with_previews(&mut host, &id);
        window.draw(&mut host, 1.0);
        let revision = host.artifact(&id).revision;
        let frame = frame_of(&window, "n1");
        let (from, to) = (frame.center(), frame.center() + Vec2::new(40.0, 20.0));
        let (transforms, renders, applies) = (sent(&window, "view.transform.set"), sent(&window, "render.get"), sent(&window, "changes.apply"));
        window.frame(&mut host, 1.0, vec![Event::PointerMoved(from), button(from, PointerButton::Primary, true)]);
        window.frame(&mut host, 1.0, vec![Event::PointerMoved(from + Vec2::new(20.0, 10.0))]);
        window.frame(&mut host, 1.0, vec![Event::PointerMoved(to)]);
        let moved = frame.translate(to - from);
        assert!(
            images(&window).iter().any(|rect| (rect.min - moved.min).length() < 1.5 && (rect.max - moved.max).length() < 1.5),
            "the node's pixels are drawn at the pointer: {moved:?} in {:?}",
            images(&window),
        );
        assert_eq!(
            (sent(&window, "view.transform.set"), sent(&window, "render.get"), sent(&window, "changes.apply")),
            (transforms, renders, applies),
            "the drag asks the core for no render, transform or change",
        );
        window.frame(&mut host, 1.0, vec![Event::PointerMoved(to), button(to, PointerButton::Primary, false)]);
        window.draw(&mut host, 1.0);
        assert_eq!(host.artifact(&id).revision, revision + 1, "the drop is one change, one undo step");
        assert!(window.app.drag_shift(&window.ctx, "n1").is_none(), "the page of the new revision ends the preview");
        // The drop may snap to a guide within 8 px (`snap_free_move`).
        let drawn = frame_of(&window, "n1");
        assert!((drawn.min - moved.min).length() < 12.0, "the node is drawn where it was dropped: {drawn:?} vs {moved:?}");
    }

    #[test]
    fn delete_removes_the_object_clicked_last_on_either_page() {
        let mut host = Host::start();
        let id = canvas(
            &mut host,
            &[
                vec![node("n1", 1_000_000, 1_000_000, 3_000_000, 2_000_000), node("n4", 6_000_000, 1_000_000, 3_000_000, 2_000_000)],
                vec![node("n2", 1_000_000, 1_000_000, 3_000_000, 2_000_000)],
            ],
        );
        let mut window = Window::open_with_previews(&mut host, &id);
        window.draw(&mut host, 1.0);
        window.click(&mut host, 1.0, frame_of(&window, "n1").center(), false);
        window.click(&mut host, 1.0, frame_of(&window, "n4").center(), false);
        window.draw(&mut host, 1.0);
        assert_eq!(window.app.selected_nodes, vec!["n4".to_string()], "the second object of the page is selected");
        let revision = host.artifact(&id).revision;
        press_delete(&mut window, &mut host);
        assert!(!holds(&mut host, &id, "n4") && holds(&mut host, &id, "n1"), "Delete removes the selected object only");
        assert_eq!(host.artifact(&id).revision, revision + 1, "one change, one undo step");
        window.click(&mut host, 1.0, frame_of(&window, "n2").center(), false);
        window.draw(&mut host, 1.0);
        assert_eq!(window.app.selected_nodes, vec!["n2".to_string()], "an object of the other page is selected");
        press_delete(&mut window, &mut host);
        assert!(!holds(&mut host, &id, "n2"), "Delete removes it");
        assert_eq!(host.artifact(&id).revision, revision + 2);
    }

    #[test]
    fn delete_on_a_selected_memo_removes_the_memo_and_with_nothing_selected_asks_for_a_target() {
        let mut host = Host::start();
        let id = canvas(&mut host, &[vec![node("n1", 1_000_000, 1_000_000, 3_000_000, 2_000_000)]]);
        let page_id = match &host.artifact(&id).body {
            ArtifactBody::Canvas(document) => document.pages[0].id.clone(),
            _ => panic!("a canvas"),
        };
        let memo = CanvasBoardMemo {
            kind: CanvasBoardMemoKind::Memo,
            id: "memo-1".into(),
            page_id: page_id.clone(),
            bounds: EmuRect { x: 6_000_000, y: 3_500_000, width: 3_000_000, height: 2_000_000 },
            text: "메모".into(),
            color: "#FFF2A8".into(),
        };
        host.apply(&id, vec![Operation::SetCanvasBoardMemos { page_id, expected: Vec::new(), memos: vec![memo] }], [Intent::Content].into_iter().collect())
            .expect("the memo is added");
        let mut window = Window::open_with_previews(&mut host, &id);
        window.draw(&mut host, 1.0);
        window.click(&mut host, 1.0, frame_of(&window, "n1").center(), false);
        window.draw(&mut host, 1.0);
        assert_eq!(window.app.selected_nodes, vec!["n1".to_string()]);
        // Far enough in time from the click on n1 that egui does not count a double click (which opens the memo for writing).
        window.draw(&mut host, 1.0);
        window.click(&mut host, 1.0, at(&window, 0, 7_500_000, 4_500_000), false);
        window.draw(&mut host, 1.0);
        assert_eq!(window.app.selected_board_memo.as_deref(), Some("memo-1"), "the memo is selected");
        assert!(window.app.selected_nodes.is_empty(), "in place of the object");
        assert!(window.app.board_memo_editor.is_none(), "a single click opens no memo for writing");
        let revision = host.artifact(&id).revision;
        press_delete(&mut window, &mut host);
        let artifact = host.artifact(&id);
        let ArtifactBody::Canvas(document) = &artifact.body else { panic!("a canvas") };
        assert!(document.board_memos.is_empty(), "Delete removes the memo: {:?}", window.message());
        assert!(holds(&mut host, &id, "n1"), "and leaves the object");
        assert_eq!(host.artifact(&id).revision, revision + 1, "one memo change");
        press_delete(&mut window, &mut host);
        assert_eq!(host.artifact(&id).revision, revision + 1, "nothing selected, nothing removed");
        assert!(window.message().is_some_and(|message| message.contains("지울 대상을 고르세요")), "{:?}", window.message());
    }

    fn parse(value: serde_json::Value) -> Node {
        serde_json::from_value(value).expect("a node parses")
    }

    fn sorted(ids: &[String]) -> Vec<String> {
        let mut ids = ids.to_vec();
        ids.sort();
        ids
    }

    #[test]
    fn the_hand_tool_and_space_pan_over_an_object_without_selecting_moving_or_resizing_it() {
        let mut host = Host::start();
        let id = canvas(&mut host, &[vec![node("n1", 1_000_000, 1_000_000, 3_000_000, 2_000_000)]]);
        let mut window = Window::open_with_previews(&mut host, &id);
        window.draw(&mut host, 1.0);
        let (revision, before) = (host.artifact(&id).revision, bounds_of(&mut host, &id, "n1"));
        window.app.view().tool = super::Tool::Hand;
        let pan = window.app.view_ref().pan;
        let centre = frame_of(&window, "n1").center();
        drag(&mut window, &mut host, PointerButton::Primary, centre, centre + Vec2::new(60.0, 30.0));
        window.draw(&mut host, 1.0);
        assert!((window.app.view_ref().pan - (pan + Vec2::new(60.0, 30.0))).length() < 0.01, "the hand tool pans from on an object");
        assert!(window.app.selected_nodes.is_empty(), "and selects nothing");
        window.app.view().tool = super::Tool::Select;
        window.app.select_ids(vec!["n1".into()]);
        window.draw(&mut host, 1.0);
        let corner = frame_of(&window, "n1").right_bottom();
        let space = |pressed| Event::Key { key: Key::Space, physical_key: Some(Key::Space), pressed, repeat: false, modifiers: Modifiers::NONE };
        let pan = window.app.view_ref().pan;
        window.frame(&mut host, 1.0, vec![space(true)]);
        drag(&mut window, &mut host, PointerButton::Primary, corner, corner + Vec2::new(40.0, 20.0));
        window.frame(&mut host, 1.0, vec![space(false)]);
        window.draw(&mut host, 1.0);
        assert!((window.app.view_ref().pan - (pan + Vec2::new(40.0, 20.0))).length() < 0.01, "Space pans from on a corner handle");
        assert_eq!(bounds_of(&mut host, &id, "n1"), before, "the object neither moves nor resizes");
        assert_eq!(host.artifact(&id).revision, revision, "no change");
    }

    #[test]
    fn a_group_lets_a_marquee_start_on_its_empty_part_until_it_is_selected_then_moves_from_there() {
        let mut host = Host::start();
        let group = parse(json!({
            "id": "g", "kind": "group", "bounds": { "x": 1_000_000, "y": 1_000_000, "width": 8_000_000, "height": 4_000_000 },
            "children": [
                { "id": "c1", "kind": "rectangle", "bounds": { "x": 0, "y": 0, "width": 2_000_000, "height": 1_500_000 } },
                { "id": "c2", "kind": "rectangle", "bounds": { "x": 6_000_000, "y": 2_500_000, "width": 2_000_000, "height": 1_500_000 } },
            ],
        }));
        let id = canvas(&mut host, &[vec![group]]);
        let mut window = Window::open_with_previews(&mut host, &id);
        window.draw(&mut host, 1.0);
        let before = bounds_of(&mut host, &id, "g");
        // The empty middle of the group, to the top left: the box touches c1 only and covers no whole group.
        let (from, to) = (at(&window, 0, 5_000_000, 3_000_000), at(&window, 0, 2_500_000, 2_000_000));
        drag(&mut window, &mut host, PointerButton::Primary, from, to);
        window.draw(&mut host, 1.0);
        assert_eq!(window.app.selected_nodes, vec!["c1".to_string()], "the marquee starts in the unselected group");
        assert_eq!(bounds_of(&mut host, &id, "g"), before, "and the group stays");
        window.app.select_ids(vec!["g".into()]);
        window.draw(&mut host, 1.0);
        let revision = host.artifact(&id).revision;
        let from = at(&window, 0, 5_000_000, 3_000_000);
        drag(&mut window, &mut host, PointerButton::Primary, from, from + Vec2::new(40.0, 20.0));
        window.draw(&mut host, 1.0);
        let after = bounds_of(&mut host, &id, "g");
        assert!(after.x > before.x && after.y > before.y, "the selected group moves from its empty part: {before:?} -> {after:?}");
        assert_eq!(host.artifact(&id).revision, revision + 1, "one change");
        assert_eq!(window.app.selected_nodes, vec!["g".to_string()]);
    }

    #[test]
    fn a_marquee_starts_on_a_locked_background_and_leaves_it_out() {
        let mut host = Host::start();
        let background = parse(json!({
            "id": "bg", "kind": "rectangle", "locked": true,
            "bounds": { "x": 0, "y": 0, "width": 12_192_000, "height": 6_858_000 },
        }));
        let id = canvas(
            &mut host,
            &[vec![background, node("n1", 1_000_000, 1_000_000, 3_000_000, 2_000_000), node("n2", 5_000_000, 1_000_000, 3_000_000, 2_000_000)]],
        );
        let mut window = Window::open_with_previews(&mut host, &id);
        window.draw(&mut host, 1.0);
        let before = bounds_of(&mut host, &id, "bg");
        let (from, to) = (at(&window, 0, 200_000, 200_000), at(&window, 0, 9_000_000, 4_000_000));
        drag(&mut window, &mut host, PointerButton::Primary, from, to);
        window.draw(&mut host, 1.0);
        assert_eq!(sorted(&window.app.selected_nodes), vec!["n1".to_string(), "n2".to_string()]);
        assert_eq!(bounds_of(&mut host, &id, "bg"), before, "the locked background does not move");
    }

    #[test]
    fn a_drag_of_one_of_several_selected_objects_moves_them_all_in_one_change() {
        let mut host = Host::start();
        let id = canvas(
            &mut host,
            &[vec![node("n1", 1_000_000, 1_000_000, 3_000_000, 2_000_000), node("n2", 5_000_000, 1_000_000, 3_000_000, 2_000_000), node("n3", 9_000_000, 4_000_000, 2_000_000, 2_000_000)]],
        );
        let mut window = Window::open_with_previews(&mut host, &id);
        window.draw(&mut host, 1.0);
        window.app.select_ids(vec!["n1".into(), "n2".into()]);
        window.draw(&mut host, 1.0);
        let revision = host.artifact(&id).revision;
        let (one, two, three) = (bounds_of(&mut host, &id, "n1"), bounds_of(&mut host, &id, "n2"), bounds_of(&mut host, &id, "n3"));
        let (frame_one, frame_two) = (frame_of(&window, "n1"), frame_of(&window, "n2"));
        let (from, to) = (frame_one.center(), frame_one.center() + Vec2::new(40.0, 20.0));
        window.frame(&mut host, 1.0, vec![Event::PointerMoved(from), button(from, PointerButton::Primary, true)]);
        window.frame(&mut host, 1.0, vec![Event::PointerMoved(from + Vec2::new(20.0, 10.0))]);
        window.frame(&mut host, 1.0, vec![Event::PointerMoved(to)]);
        let shown = images(&window);
        for moved in [frame_one.translate(to - from), frame_two.translate(to - from)] {
            assert!(shown.iter().any(|rect| (rect.min - moved.min).length() < 1.5), "each selected object is drawn moved: {moved:?} in {shown:?}");
        }
        window.frame(&mut host, 1.0, vec![Event::PointerMoved(to), button(to, PointerButton::Primary, false)]);
        window.draw(&mut host, 1.0);
        let (after_one, after_two) = (bounds_of(&mut host, &id, "n1"), bounds_of(&mut host, &id, "n2"));
        assert!(after_one.x > one.x && after_one.y > one.y, "the dragged object moves");
        assert_eq!((after_two.x - two.x, after_two.y - two.y), (after_one.x - one.x, after_one.y - one.y), "the other selected one by the same step");
        assert_eq!(bounds_of(&mut host, &id, "n3"), three, "an object not selected stays");
        assert_eq!(host.artifact(&id).revision, revision + 1, "one change, one undo step");
        assert_eq!(sorted(&window.app.selected_nodes), vec!["n1".to_string(), "n2".to_string()], "the selection stays");
    }

    /// A canvas of one page with `n1` and two guide columns, and the x of the line between them (page EMU).
    fn canvas_with_guides(host: &mut Host) -> (String, Window, i64) {
        let id = canvas(host, &[vec![node("n1", 1_000_000, 1_000_000, 3_000_000, 2_000_000)]]);
        let mut window = Window::open_with_previews(host, &id);
        window.draw(host, 1.0);
        window
            .step(host, "canvas.guides.set", json!({ "axis": "columns", "count": 2, "spacing_emu": 0, "leading_margin_emu": 0, "trailing_margin_emu": 0 }))
            .expect("the columns are set");
        window.draw(host, 1.0);
        let artifact = host.artifact(&id);
        let ArtifactBody::Canvas(document) = &artifact.body else { panic!("a canvas") };
        let layout = super::super::page_guides::page_guide_layout(document, &document.pages[0]).expect("the guides fit the page");
        let line = layout.vertical_lines()[1];
        (id, window, line)
    }

    #[test]
    fn a_dragged_object_is_drawn_where_the_guide_snap_will_drop_it() {
        let mut host = Host::start();
        let (id, mut window, line) = canvas_with_guides(&mut host);
        let frame = frame_of(&window, "n1");
        // The left edge 4 px right of the line: inside the 8 physical px snap window.
        let target = at(&window, 0, line, 1_000_000).x + 4.0;
        let (from, to) = (frame.center(), frame.center() + Vec2::new(target - frame.left(), 0.0));
        window.frame(&mut host, 1.0, vec![Event::PointerMoved(from), button(from, PointerButton::Primary, true)]);
        window.frame(&mut host, 1.0, vec![Event::PointerMoved(from + (to - from) * 0.5)]);
        window.frame(&mut host, 1.0, vec![Event::PointerMoved(to)]);
        let snapped_left = at(&window, 0, line, 1_000_000).x;
        assert!(
            images(&window).iter().any(|rect| (rect.left() - snapped_left).abs() < 1.5 && (rect.top() - frame.top()).abs() < 1.5),
            "the preview sits on the line at {snapped_left}: {:?}",
            images(&window),
        );
        window.frame(&mut host, 1.0, vec![Event::PointerMoved(to), button(to, PointerButton::Primary, false)]);
        window.draw(&mut host, 1.0);
        assert_eq!(bounds_of(&mut host, &id, "n1").x, line, "the drop lands on the line");
    }

    #[test]
    fn a_corner_handle_keeps_the_opposite_corner_turns_over_and_snaps() {
        let mut host = Host::start();
        let (id, mut window, line) = canvas_with_guides(&mut host);
        window.app.select_ids(vec!["n1".into()]);
        window.draw(&mut host, 1.0);
        // The top-left corner: the bottom-right one stays.
        let before = bounds_of(&mut host, &id, "n1");
        let corner = frame_of(&window, "n1").left_top();
        drag(&mut window, &mut host, PointerButton::Primary, corner, corner - Vec2::new(30.0, 15.0));
        window.draw(&mut host, 1.0);
        let after = bounds_of(&mut host, &id, "n1");
        assert_eq!((after.x + after.width, after.y + after.height), (before.x + before.width, before.y + before.height), "the opposite corner stays");
        assert!(after.x < before.x && after.y < before.y, "the dragged corner moves: {before:?} -> {after:?}");
        // The bottom-right corner past the left edge: the box turns over and keeps a size, its right edge where the left was.
        let before = after;
        let frame = frame_of(&window, "n1");
        let corner = frame.right_bottom();
        drag(&mut window, &mut host, PointerButton::Primary, corner, corner - Vec2::new(frame.width() + 30.0, 0.0));
        window.draw(&mut host, 1.0);
        let after = bounds_of(&mut host, &id, "n1");
        assert!(after.width > 0 && after.x < before.x, "the box turns over: {before:?} -> {after:?}");
        assert!((after.x + after.width - before.x).abs() < 20_000, "its right edge is the old left edge: {before:?} -> {after:?}");
        // The right edge dragged 4 px short of the guide line snaps onto it.
        let frame = frame_of(&window, "n1");
        let corner = frame.right_bottom();
        let target = Pos2::new(at(&window, 0, line, 0).x - 4.0, corner.y + 10.0);
        drag(&mut window, &mut host, PointerButton::Primary, corner, target);
        window.draw(&mut host, 1.0);
        let snapped = bounds_of(&mut host, &id, "n1");
        assert_eq!(snapped.x + snapped.width, line, "the right edge snaps to the line: {snapped:?}");
    }

    #[test]
    fn a_middle_drag_that_starts_on_a_page_title_button_does_not_pan() {
        let mut host = Host::start();
        let id = canvas(&mut host, &[vec![node("n1", 1_000_000, 1_000_000, 3_000_000, 2_000_000)]]);
        let mut window = Window::open_with_previews(&mut host, &id);
        window.draw(&mut host, 1.0);
        let page = page_rect(&window, 0);
        // The last mini button at the right end of the current page's title row (`artboard_actions`).
        let row_top = page.top() - crate::theme::ARTBOARD_TITLE_GAP - crate::theme::ARTBOARD_TITLE_HEIGHT;
        let on_button = Pos2::new(page.right() - crate::theme::MINI_BUTTON / 2.0, row_top - 2.0 + crate::theme::MINI_BUTTON / 2.0);
        let pan = window.app.view_ref().pan;
        drag(&mut window, &mut host, PointerButton::Middle, on_button, on_button + Vec2::new(60.0, 30.0));
        window.draw(&mut host, 1.0);
        assert_eq!(window.app.view_ref().pan, pan, "a press on a button over the board is the button's");
        let empty = page.right_bottom() - Vec2::splat(6.0);
        drag(&mut window, &mut host, PointerButton::Middle, empty, empty + Vec2::new(60.0, 30.0));
        window.draw(&mut host, 1.0);
        assert!((window.app.view_ref().pan - (pan + Vec2::new(60.0, 30.0))).length() < 0.01, "a press on the page pans");
    }

    #[test]
    fn a_refused_transform_is_not_sent_again_until_the_placement_changes() {
        let mut host = Host::start();
        let id = canvas(&mut host, &[vec![node("n1", 1_000_000, 1_000_000, 3_000_000, 2_000_000)]]);
        let mut window = Window::open_with_previews(&mut host, &id);
        window.draw(&mut host, 1.0);
        let refused = window.app.page_transforms[&0].id.clone();
        assert_eq!(window.app.core_transform_id.as_deref(), Some(refused.as_str()), "the core holds the page's transform");
        // The core refused it (the reply's error path).
        window.app.forget_core_transform_on_failure(&super::super::PendingAction::Transform);
        let sends = sent(&window, "view.transform.set");
        window.draw(&mut host, 1.0);
        window.click(&mut host, 1.0, frame_of(&window, "n1").center(), false);
        window.draw(&mut host, 1.0);
        assert_eq!(sent(&window, "view.transform.set"), sends, "the refused transform is sent neither by the frames nor by a hit test");
        // A pan makes a new placement: its transform goes once the board is still.
        let empty = page_rect(&window, 0).right_bottom() - Vec2::splat(6.0);
        drag(&mut window, &mut host, PointerButton::Middle, empty, empty + Vec2::new(60.0, 30.0));
        window.draw(&mut host, 1.0);
        assert_eq!(sent(&window, "view.transform.set"), sends + 1, "the new placement's transform is sent");
        assert_ne!(window.app.page_transforms[&0].id, refused);
    }

    #[test]
    fn a_shape_with_children_moves_from_its_empty_part_like_any_object() {
        let mut host = Host::start();
        let card = parse(json!({
            "id": "card", "kind": "rectangle", "bounds": { "x": 1_000_000, "y": 1_000_000, "width": 6_000_000, "height": 4_000_000 },
            "children": [{ "id": "inner", "kind": "rectangle", "bounds": { "x": 0, "y": 0, "width": 1_500_000, "height": 1_000_000 } }],
        }));
        let id = canvas(&mut host, &[vec![card]]);
        let mut window = Window::open_with_previews(&mut host, &id);
        window.draw(&mut host, 1.0);
        let (revision, before) = (host.artifact(&id).revision, bounds_of(&mut host, &id, "card"));
        let from = at(&window, 0, 5_000_000, 4_000_000);
        drag(&mut window, &mut host, PointerButton::Primary, from, from + Vec2::new(40.0, 20.0));
        window.draw(&mut host, 1.0);
        let after = bounds_of(&mut host, &id, "card");
        assert!(after.x > before.x && after.y > before.y, "the shape moves, no marquee starts: {before:?} -> {after:?}");
        assert_eq!(host.artifact(&id).revision, revision + 1);
    }

    #[test]
    fn a_drag_or_a_resize_on_another_page_makes_it_current() {
        let mut host = Host::start();
        let id = canvas(
            &mut host,
            &[vec![node("n1", 1_000_000, 1_000_000, 3_000_000, 2_000_000)], vec![node("n2", 1_000_000, 1_000_000, 3_000_000, 2_000_000)]],
        );
        let mut window = Window::open_with_previews(&mut host, &id);
        window.draw(&mut host, 1.0);
        assert_eq!(window.app.view_ref().page_index, 0);
        let centre = frame_of(&window, "n2").center();
        window.frame(&mut host, 1.0, vec![Event::PointerMoved(centre), button(centre, PointerButton::Primary, true)]);
        window.frame(&mut host, 1.0, vec![Event::PointerMoved(centre + Vec2::new(20.0, 10.0))]);
        assert_eq!(window.app.view_ref().page_index, 1, "the drag's start makes page 2 current");
        window.frame(&mut host, 1.0, vec![Event::PointerMoved(centre + Vec2::new(20.0, 10.0)), button(centre + Vec2::new(20.0, 10.0), PointerButton::Primary, false)]);
        window.draw(&mut host, 1.0);
        window.app.focus_page(0);
        window.app.select_ids(vec!["n2".into()]);
        window.draw(&mut host, 1.0);
        assert_eq!(window.app.view_ref().page_index, 0);
        let corner = frame_of(&window, "n2").right_bottom();
        window.frame(&mut host, 1.0, vec![Event::PointerMoved(corner), button(corner, PointerButton::Primary, true)]);
        window.frame(&mut host, 1.0, vec![Event::PointerMoved(corner + Vec2::new(20.0, 10.0))]);
        assert_eq!(window.app.view_ref().page_index, 1, "the resize's start makes page 2 current");
        window.frame(&mut host, 1.0, vec![Event::PointerMoved(corner + Vec2::new(20.0, 10.0)), button(corner + Vec2::new(20.0, 10.0), PointerButton::Primary, false)]);
        window.draw(&mut host, 1.0);
    }
}
