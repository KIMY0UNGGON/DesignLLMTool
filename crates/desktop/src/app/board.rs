//! The board: pages on a zoomable surface, each drawn at the physical size it is shown at (U7). A
//! page is rendered by the core at `board::dpi_for(zoom, ppp)`; while the view is zooming the old
//! texture is stretched, and the new resolution is requested once the zoom has settled (§5.6).

use std::time::Duration;

use designllm_contracts::{
    new_id, ArtifactKind, RenderPage, RenderProfile, RenderRequest, Size, ViewTransform,
};
use eframe::egui::{
    self, Color32, Context, CornerRadius, Id, Pos2, PointerButton, Rect, Response, Sense,
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
    pub page: RenderPage,
    /// Where the engine drew each node of the page (page EMU): the engine's own frames (`scene.metadata["node_frames"]`,
    /// asked with `include_node_frames`), else the frames read from the scene (`multi::laid_out_frames`).
    pub frames: std::collections::BTreeMap<String, designllm_contracts::Rect>,
}

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

/// Whether an `asset.read` answer is a decoded PNG a texture can be made from: a non-empty image with four bytes per pixel.
pub(super) fn is_rgba_png(mime: &str, width: u32, height: u32, rgba: &[u8]) -> bool {
    mime == "image/png" && width > 0 && height > 0 && rgba.len() == width as usize * height as usize * 4
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
    fn board_input(&mut self, ctx: &Context, response: &Response, board_rect: Rect) {
        let (scroll, pinch, command, space, hover) = ctx.input(|input| {
            (
                input.smooth_scroll_delta,
                input.zoom_delta(),
                input.modifiers.command,
                input.key_down(egui::Key::Space),
                input.pointer.hover_pos(),
            )
        });
        let hand = self.view_ref().tool == super::board::Tool::Hand;
        let panning = response.dragged_by(PointerButton::Middle)
            || ((space || hand) && response.dragged_by(PointerButton::Primary));
        if panning {
            let delta = response.drag_delta();
            self.view().pan += delta;
        }
        if !response.hovered() {
            return;
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
        }
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
        if self.inline_text_edit.is_none()
            && !self.host_lost()
            && self.view_ref().tool != Tool::Memo
            && !ctx.wants_keyboard_input()
            && ctx.input(|input| input.key_pressed(egui::Key::Enter)
                && !input.modifiers.ctrl && !input.modifiers.alt && !input.modifiers.mac_cmd)
            && self.selected_nodes.len() == 1
        {
            if let (Some(node_id), Some(artifact)) = (self.selected_nodes.first().cloned(), self.artifact.as_ref()) {
                if let Some(page_index) = node_page_index(artifact, &node_id) {
                    if self.begin_inline_text_edit(ctx, &node_id, page_index, None) {
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
        if self.inline_text_edit.is_none() {
            self.board_input(ctx, &response, board_rect);
        }
        // A fresh fit, and any zoom once it has settled, moves (by at most half a dpi) to the zoom whose render
        // resolution is a whole dpi, so the page texture maps 1:1 to physical pixels and the first render is already
        // the final one. The board origin stays where it is.
        let ppp = ctx.pixels_per_point();
        if fitting || self.view_ref().zoom_changed.elapsed() >= SETTLE {
            let view = self.view();
            view.zoom = board::snapped_zoom(view.zoom, ppp);
        }
        let view = self.view_ref();
        let origin = board_rect.min + view.pan;
        let kind = self.active.as_ref().map_or(ArtifactKind::Canvas, |item| item.kind);
        let slots = board::arrange(kind, &self.page_sizes(), view.zoom);
        let wanted_dpi = board::dpi_for(view.zoom, ppp);
        self.board_dpi = wanted_dpi;
        let settled = view.zoom_changed.elapsed() >= SETTLE;
        for slot in &slots {
            let rect = board::to_screen(slot.world, origin, view.zoom);
            if !board::visible(rect, board_rect) {
                continue;
            }
            self.draw_page(ctx, ui, &palette, slot.page_index, rect, wanted_dpi, settled);
        }
        if !settled {
            ctx.request_repaint_after(SETTLE);
        }
        if self.inline_text_edit.is_none()
            && view.tool != Tool::Memo
            && (response.clicked() || response.double_clicked())
            && !self.region_clicked
        {
            // A click on the board selects what its frame holds (the core's hit test covers shapes and
            // text alike); on empty board it clears the selection.
            match response.interact_pointer_pos().and_then(|pos| page_at(&slots, origin, view.zoom, pos).map(|page| (page, pos))) {
                Some((page, pos)) => self.hit_test_page(page, pos, ctx.pixels_per_point(), super::multi::additive_modifier(ctx), response.double_clicked()),
                None => self.select_clear(),
            }
        }
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
        // slides-board: the current slide has the selection outline (1.5 px accent, inside).
        if kind == ArtifactKind::Slides && selected_page {
            painter.rect_stroke(rect, CornerRadius::ZERO, egui::Stroke::new(1.5, palette.accent), egui::StrokeKind::Inside);
        }
        self.sync_page_transform(page_index, rect, px, ctx.pixels_per_point());
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
        for (index, source) in page.source_map.iter().enumerate() {
            let region = super::page_to_screen(source.bounds, page.size, rect);
            if region.width() < 1.0 || region.height() < 1.0 {
                continue;
            }
            let key = egui::Id::new(("scene-region", page_index, source.node_id.as_str(), index));
            if let Some(click) = self.interact_with_region(ui, key, page_index, rect, &source.node_id, source.bounds, page.size, region) {
                self.region_clicked = true;
                self.hit_test_page(page_index, click.position, ctx.pixels_per_point(), super::multi::additive_modifier(ctx), click.double_clicked);
            }
        }
    }

    fn draw_inline_text_editor(&mut self, ctx: &Context, ui: &mut Ui, page_index: u32, page_size: Size, page_rect: Rect) {
        let Some(edit) = self.inline_text_edit.as_ref().filter(|edit| edit.page_index == page_index).cloned() else { return };
        let current_targets = self.artifact.as_ref()
            .and_then(|artifact| super::doc_tree::inline_text_target_node_ids(artifact, edit.kind, edit.page_index, &edit.node_id));
        if edit.reservation_node_ids.len() > 1 {
            let readiness = self.inline_table_cell_readiness(&edit);
            if !self.apply_inline_cell_readiness(edit.id, readiness) { return; }
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
        let output = egui::TextEdit::multiline(&mut self.text_draft)
            .id(self.text_editor_id)
            .font(theme::display_font(bold, font_size))
            .text_color(color)
            .horizontal_align(align)
            .frame(false)
            .margin(egui::Margin::ZERO)
            .desired_width(rect.width())
            .desired_rows(1)
            .min_size(rect.size())
            .interactive(edit.conflict.is_none() || edit.composing)
            .show(&mut child);
        self.text_adapter.record_layout(&output);
        if edit.conflict.is_none() || edit.composing {
            self.text_editor_focused = true;
            output.response.request_focus();
            if ctx.input(|input| input.key_pressed(egui::Key::Escape))
                && !self.text_adapter.is_ime_active()
                && !edit.ime_cancelled_frame
            {
                self.finish_inline_text_edit();
            }
        }
    }

    fn draw_inline_text_conflict(&mut self, ctx: &Context, _ui: &mut Ui) {
        let Some(edit) = self.inline_text_edit.as_ref()
            .filter(|edit| edit.conflict.is_some() || edit.layout_pending).cloned() else { return };
        let position = self.board_rect.map_or(Pos2::new(16.0, 56.0), |rect| Pos2::new(rect.left() + 16.0, rect.top() + 16.0));
        let mut retry = false;
        let mut retry_release = false;
        let mut discard = false;
        let mut copy_draft = false;
        egui::Area::new(Id::new("inline-text-conflict"))
            .order(egui::Order::Foreground)
            .fixed_pos(position)
            .show(ctx, |ui| {
                let palette = theme::palette(ui.ctx());
                egui::Frame::new().fill(palette.surface).stroke(egui::Stroke::new(1.0, palette.line))
                    .corner_radius(CornerRadius::same(8)).inner_margin(egui::Margin::same(10)).show(ui, |ui| {
                        ui.set_max_width(360.0);
                        ui.label(edit.conflict.as_deref().unwrap_or("표 셀 배치를 갱신 중입니다. 새 입력은 잠시 중지했고 기존 초안과 대기 입력은 보존했습니다."));
                        if edit.retry_same_request {
                            ui.label("마지막 입력의 확정 결과가 불명확합니다. 재시도는 같은 request_id로 확인하며, 초안을 버려도 이미 확정된 입력은 남을 수 있습니다.");
                        }
                        ui.label("적용되지 않은 초안:");
                        let preview: String = self.text_draft.chars().take(240).collect();
                        ui.label(egui::RichText::new(preview).monospace());
                        ui.horizontal(|ui| {
                            if edit.reservation_release_failed && ui.button("예약 해제 다시 시도").clicked() { retry_release = true; }
                            let can_copy = !edit.composing && !self.text_adapter.is_ime_active();
                            if ui.add_enabled(can_copy, egui::Button::new("초안 복사")).clicked() { copy_draft = true; }
                            let can_retry = edit.retry_allowed && !edit.queue.is_empty() && !edit.composing && !self.text_adapter.is_ime_active()
                                && edit.in_flight.is_none() && !edit.reservation_pending && !edit.layout_pending;
                            if ui.add_enabled(can_retry, egui::Button::new("다시 시도")).clicked() { retry = true; }
                            let can_discard = !edit.composing && !self.text_adapter.is_ime_active()
                                && edit.in_flight.is_none() && !edit.reservation_pending;
                            if ui.add_enabled(can_discard, egui::Button::new("초안 버리기")).clicked() { discard = true; }
                        });
                    });
            });
        if copy_draft { ctx.copy_text(self.text_draft.clone()); }
        if retry { self.retry_inline_text_edits(); }
        if retry_release { self.release_inline_text_target(edit.id); }
        if discard { self.discard_inline_text_draft(); }
    }

    /// Records the page's transform with the core so that hit tests map screen points to the page
    /// (sent only when the placement changed).
    fn sync_page_transform(&mut self, page_index: u32, rect: Rect, px: Vec2, ppp: f32) {
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
        self.page_transforms.insert(page_index, transform.clone());
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

    /// Asks the core which node is at `position` on `page_index`.
    pub(super) fn hit_test_page(&mut self, page_index: u32, position: Pos2, pixels_per_point: f32, additive: bool, edit_text: bool) {
        let (Some(page), Some(transform)) = (self.page_textures.get(&page_index), self.page_transforms.get(&page_index)) else {
            return;
        };
        let snapshot_id = page.snapshot_id.clone();
        let transform_id = transform.id.clone();
        let Some(context) = self.context.as_ref() else {
            return;
        };
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
            if fresh || !settled {
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
    pub(super) fn store_region_texture(&mut self, ctx: &Context, page_index: u32, snapshot: &designllm_contracts::RenderSnapshot, region: designllm_contracts::Rect, width: u32, height: u32, rgba: &[u8]) {
        let image = egui::ColorImage::from_rgba_unmultiplied([width as usize, height as usize], rgba);
        let texture = ctx.load_texture(format!("page-{page_index}-region"), image, TextureOptions::LINEAR);
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
        self.requested_pages.clear();
        self.known_sizes.clear();
        self.page_rect = None;
    }

    /// Texture for a page, decoded from the core's PNG.
    pub(super) fn store_page_texture(&mut self, ctx: &Context, page_index: u32, snapshot: &designllm_contracts::RenderSnapshot, width: u32, height: u32, rgba: &[u8]) {
        let Some(render_page) = snapshot.scene.pages.iter().find(|page| page.page_index == page_index).cloned() else {
            return;
        };
        let size = [width as usize, height as usize];
        let image = egui::ColorImage::from_rgba_unmultiplied(size, rgba);
        let texture = ctx.load_texture(format!("page-{page_index}"), image, TextureOptions::LINEAR);
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
                page: render_page,
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

