//! The toolbox tab of the left sidebar (68g): the insert tiles of a canvas or slides, in the groups basic, shapes and media.
//! A tile click inserts at the visible centre of the current page. A tile dragged onto the board shows the outline of the
//! object it inserts under the pointer and is inserted where it is let go, on the page under the pointer, which becomes the
//! current page. It is cancelled when let go off the pages, off the board (over the sidebar, a panel or the window's edge),
//! over anything drawn above the board (a floating bar, a popup), outside the window, or after Esc.
//! Commands (text, shapes, image, table, icon) go through the add command with the `at` argument, the same call an agent
//! sends (`commands.rs` ARG_AT). A memo, a video and an icon use their own existing flows at the same point: the memo editor,
//! the video file dialog, and the icon picker.

use designllm_contracts::{ArtifactKind, Feature, NodeKind, Rect as ModelRect, Selection, Size, EMU_PER_CSS_PX};
use eframe::egui::{self, Align2, Context, CornerRadius, Id, Key, LayerId, Margin, Order, Pos2, Rect, Sense, Stroke, StrokeKind, Ui, Vec2};
use serde_json::{json, Value};

use super::{board::LeftTab, DesktopApp};
use crate::{board, commands, icons, theme};

/// Where an insert lands: a page of the document and the object's centre in page EMU.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct AddAt {
    pub page_index: u32,
    pub center: (i64, i64),
}

/// The page, the selection and the window's copy of the context before an insert on another page, which `focus_add_page`
/// changes (68g). Kept to put back when the insert does not go ahead (`restore_add_focus`).
#[derive(Clone)]
pub(super) struct AddFocus {
    page_index: u32,
    node_ids: Vec<String>,
    /// The window's own selection (`local_selection`), None before the context is first read.
    selection: Option<Selection>,
    context: Option<(u32, Selection)>,
}

/// A video dropped on another page, waiting in the frame memory under `WAITING_VIDEO` for the answer to the page change
/// (`run_waiting_toolbox_video`): the point, the artifact, what to put back, the egui time it was dropped at, and the
/// selection generation and revision the page change left (a change of either since then, by the user or an agent, is not put back).
#[derive(Clone, Default)]
struct WaitingVideo {
    at: AddAt,
    artifact_id: String,
    before: Option<AddFocus>,
    since: f64,
    selection_generation: u64,
    revision: u64,
}

const WAITING_VIDEO: &str = "toolbox-waiting-video";
/// The wait for the page change's answer after which a video dropped on another page is not added (seconds).
const VIDEO_WAIT_SECONDS: f64 = 5.0;

/// What a tile does with the point it is clicked or dropped at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action {
    /// An add command that takes `at`.
    Command(&'static str),
    /// A new board memo, centred there (the memo editor opens).
    Memo,
    /// A video: the file dialog, then the video is centred there.
    Video,
    /// The icon picker opens at the tile or the drop point; the picked icon is centred on the point.
    Icon,
    /// A diagram shape of the shared shape library (`add.shape`), centred on the point.
    Shape(&'static str),
}

/// One tile: what it does, its name, and its icon.
#[derive(Clone, Copy)]
struct Tile {
    action: Action,
    label: &'static str,
    icon: &'static str,
}

/// An icon waiting for its pick: the point, the artifact it is on, and where the picker opens. Kept in the frame memory under
/// `PENDING_ICON`, and dropped after a frame that did not draw the toolbox (another tab, a closed sidebar, the home screen, another
/// artifact) so that it never comes back on its own (`forget_hidden_toolbox_icon`).
#[derive(Clone, Debug, Default)]
pub(super) struct PendingIcon {
    pub at: AddAt,
    pub artifact_id: String,
    pub anchor: Pos2,
    /// The frame the tile was let go in: a press in that same frame does not dismiss the picker.
    pub frame: u64,
    /// The last frame the toolbox drew the picker in.
    pub drawn: u64,
}

pub(super) const PENDING_ICON: &str = "toolbox-pending-icon";
pub(super) fn icon_picker_open(ctx: &Context) -> bool {
    ctx.data(|data| data.get_temp::<PendingIcon>(Id::new(PENDING_ICON)).is_some())
}
/// The id of the tile being dragged, in the frame memory (`drag_under_way`).
const TOOLBOX_DRAG: &str = "toolbox-drag";

/// GUI_DESIGN §5.3 and tokens `layout.panel.section`: the sidebar's and the panel's side margin is 12, a section has 4 above the
/// first and 8 below each. The group head is a sidebar row (`component.layer-row`, 28). Tiles are 8 apart both ways, as the home
/// tiles are (GUI_DESIGN §3): the layer rows touch because they are a list, tiles are not.
pub(super) const MARGIN: Margin = Margin { left: 12, right: 12, top: 4, bottom: 0 };
const SECTION_BOTTOM: f32 = 8.0;
const TILE_HEIGHT: f32 = 64.0;
const TILE_GAP: f32 = 8.0;

/// Whether Esc is the toolbox's now: a tile's drag (egui ends a drag on Esc, so in the frame of the Esc the tile's drag has just
/// stopped, and the tile must not drop it) or an icon waiting for its pick (Esc closes the picker). `handle_shortcuts` then
/// leaves Esc to it and does not clear the selection.
pub(super) fn takes_escape(ctx: &Context) -> bool {
    let tile = ctx.data(|data| data.get_temp::<Id>(Id::new(TOOLBOX_DRAG)));
    let dragging = tile.is_some_and(|tile| ctx.dragged_id() == Some(tile) || ctx.drag_stopped_id() == Some(tile));
    dragging || ctx.data(|data| data.get_temp::<PendingIcon>(Id::new(PENDING_ICON)).is_some())
}

/// The name of a sidebar tab in `nav.tab`, `ui.state` and `ui.layout`.
pub(super) fn left_tab_name(tab: LeftTab) -> &'static str {
    match tab {
        LeftTab::Layers => "layers",
        LeftTab::Tools => "tools",
        LeftTab::Ai => "ai",
    }
}

impl DesktopApp {
    /// The toolbox tab shows for a canvas or slides: the kinds whose tool bar has the insert tiles.
    pub(super) fn toolbox_shown(&self) -> bool {
        matches!(self.active.as_ref().map(|item| item.kind), Some(ArtifactKind::Canvas | ArtifactKind::Slides))
    }

    /// The sidebar tab on screen: the chosen one, except that a kind without the toolbox shows its first tab.
    pub(super) fn shown_left_tab(&self) -> LeftTab {
        match self.view_ref().left_tab {
            LeftTab::Tools if !self.toolbox_shown() => LeftTab::Layers,
            tab => tab,
        }
    }

    /// The tile groups the document takes. The feature checks follow the tool bar and its insert menu (`tools.rs`).
    fn toolbox_groups(&self) -> Vec<(&'static str, Vec<Tile>)> {
        let kind = self.active.as_ref().map(|item| item.kind);
        let canvas = kind == Some(ArtifactKind::Canvas);
        let slides = kind == Some(ArtifactKind::Slides);
        let tile = |action: Action, label: &'static str, icon: &'static str| Tile { action, label, icon };
        let mut basic = Vec::new();
        if self.feature_write_supported(Feature::Text) {
            basic.push(tile(Action::Command("add.text"), "텍스트", "tool-text"));
        }
        if canvas {
            basic.push(tile(Action::Memo, "메모", "tool-memo"));
        }
        if !slides && self.feature_write_supported(Feature::Tables) {
            basic.push(tile(Action::Command("canvas.table.add"), "표", "ins-table"));
        }
        let mut shapes = Vec::new();
        if self.feature_write_supported(Feature::Geometry) {
            // A slide takes no frame (the tool bar's rule).
            if !slides {
                shapes.push(tile(Action::Command("add.frame"), "프레임", "tool-frame"));
            }
            shapes.push(tile(Action::Command("add.rect"), "사각형", "tool-rect"));
            shapes.push(tile(Action::Command("add.oval"), "타원", "tool-oval"));
            shapes.push(tile(Action::Command("add.line"), "선", "tool-line"));
        }
        let mut media = Vec::new();
        if self.feature_write_supported(Feature::Images) {
            media.push(tile(Action::Command("add.image"), "이미지", "ins-image"));
            if !slides {
                media.push(tile(Action::Video, "동영상", "ins-video"));
            }
        }
        if self.feature_write_supported(Feature::VectorPaths) {
            media.push(tile(Action::Icon, "아이콘", "ins-icon"));
        }
        // Diagram shapes (encoder, decoder, data, store, operators ...) are Canvas paths from the shared shape library.
        let mut diagram = Vec::new();
        if canvas && self.feature_write_supported(Feature::VectorPaths) {
            for (shape, label, icon) in super::shape_insert::TOOLBOX_SHAPES {
                diagram.push(tile(Action::Shape(shape), label, icon));
            }
        }
        [("기본", basic), ("도형", shapes), ("미디어", media), ("다이어그램 도형", diagram)]
            .into_iter()
            .map(|(group, tiles)| {
                let tiles: Vec<Tile> = tiles
                    .into_iter()
                    .filter(|tile| match tile.action {
                        Action::Command(id) => commands::available(id),
                        Action::Shape(_) => commands::available("add.shape"),
                        _ => true,
                    })
                    .collect();
                (group, tiles)
            })
            .filter(|(_, tiles)| !tiles.is_empty())
            .collect()
    }

    /// The EMU of one unit of the document (px for canvas and slides).
    fn toolbox_unit_emu(&self) -> f64 {
        let kind = self.active.as_ref().map_or(ArtifactKind::Canvas, |item| item.kind);
        super::properties::unit_for(kind).emu()
    }

    /// The `at` argument of an add command: the page (default: the current one) and the centre `x`, `y` in document units.
    /// Nothing when `at` is absent. A page index that is not a whole number from 0 to `u32::MAX`, a page that is not there and
    /// a centre outside the page are refused.
    pub(super) fn add_at_from_args(&self, args: &Value) -> Result<Option<AddAt>, String> {
        let Some(at) = args.get("at") else {
            return Ok(None);
        };
        let coordinate = |key: &str| at.get(key).and_then(Value::as_f64).filter(|value| value.is_finite()).ok_or_else(|| format!("at.{key}는 숫자여야 합니다"));
        let (x, y) = (coordinate("x")?, coordinate("y")?);
        let page_index = match at.get("page_index") {
            None => self.view_ref().page_index,
            Some(index) => index.as_u64().and_then(|index| u32::try_from(index).ok()).ok_or("at.page_index는 0 이상의 정수여야 합니다")?,
        };
        let size = self.page_sizes().get(page_index as usize).cloned().ok_or("페이지를 찾지 못했습니다")?;
        let scale = self.toolbox_unit_emu();
        let center = ((x * scale).round() as i64, (y * scale).round() as i64);
        if !(0..=size.width).contains(&center.0) || !(0..=size.height).contains(&center.1) {
            return Err("at 위치가 쪽 밖입니다".into());
        }
        Ok(Some(AddAt { page_index, center }))
    }

    /// `add_at_from_args` for an add command, whose `at` page becomes the current one (`focus_add_page`).
    pub(super) fn take_add_at(&mut self, args: &Value) -> Result<Option<AddAt>, String> {
        let at = self.add_at_from_args(args)?;
        if let Some(at) = at {
            self.focus_add_page(at.page_index);
        }
        Ok(at)
    }

    /// Makes `page_index` the current page before an insert there, as a click on that page does: the selection is cleared,
    /// which tells the host the page (`selection.set`). The window's copy of the context follows at once, so the insert's
    /// selection guard (`selection_commit_guard`) and the table checks (`canvas_table_context_current`) name that page.
    pub(super) fn focus_add_page(&mut self, page_index: u32) {
        if self.view_ref().page_index == page_index {
            return;
        }
        self.focus_page(page_index);
        if let Some(context) = self.context.as_mut() {
            context.visible_page = page_index;
            context.selection = Selection::default();
        }
        self.select_clear();
    }

    /// The page, selection and context that `focus_add_page(page_index)` would change, as they are now. Nothing when
    /// `page_index` is the current page: an insert there changes neither.
    pub(super) fn add_focus_state(&self, page_index: u32) -> Option<AddFocus> {
        let current = self.view_ref().page_index;
        (current != page_index).then(|| AddFocus {
            page_index: current,
            node_ids: self.selected_nodes.clone(),
            selection: self.local_selection.clone(),
            context: self.context.as_ref().map(|context| (context.visible_page, context.selection.clone())),
        })
    }

    /// Puts back the page and selection `before` holds when an insert that moved to another page did not go ahead (`run_tile`):
    /// `focus_add_page` ran before the insert, so the page changed and the selection was cleared for nothing. The page goes
    /// back as a page click does (`focus_page`). The selection asked of the host again is the window's own, else the one the
    /// host showed.
    pub(super) fn restore_add_focus(&mut self, before: Option<AddFocus>) {
        let Some(AddFocus { page_index, node_ids, selection, context }) = before else { return };
        if self.view_ref().page_index == page_index {
            return;
        }
        self.focus_page(page_index);
        let shown = context.map(|(visible_page, shown)| {
            if let Some(context) = self.context.as_mut() {
                context.visible_page = visible_page;
                context.selection.clone_from(&shown);
            }
            shown
        });
        self.set_selected_ids(node_ids);
        if let Some(selection) = selection.or(shown) {
            self.set_selection(selection);
        }
    }

    /// The `at` object for `at`, in document units (the form `add_at_from_args` reads).
    fn at_value(&self, at: AddAt) -> Value {
        let scale = self.toolbox_unit_emu();
        json!({ "page_index": at.page_index, "x": at.center.0 as f64 / scale, "y": at.center.1 as f64 / scale })
    }

    /// Each page's rectangle on the screen, as the board draws it this frame. Nothing before the board has been drawn.
    pub(super) fn toolbox_page_rects(&self) -> Option<Vec<(u32, Rect)>> {
        let board_rect = self.board_rect?;
        let kind = self.active.as_ref()?.kind;
        let view = self.view_ref();
        let origin = board_rect.min + view.pan;
        let slots = board::arrange(kind, &self.page_sizes(), view.zoom);
        Some(slots.iter().map(|slot| (slot.page_index, board::to_screen(slot.world, origin, view.zoom))).collect())
    }

    /// The centre of the visible part of the current page; the page's centre when the page is not on screen or the board
    /// has not been drawn yet.
    fn toolbox_visible_centre(&self) -> Option<AddAt> {
        let index = self.view_ref().page_index;
        let size = self.page_sizes().get(index as usize).cloned()?;
        let centre = match (self.toolbox_page_rects(), self.board_rect) {
            (Some(rects), Some(board)) => {
                let page_rect = rects.into_iter().find(|(page, _)| *page == index)?.1;
                let visible = page_rect.intersect(board);
                let point = if visible.is_positive() { visible.center() } else { page_rect.center() };
                super::screen_to_page_emu(point, page_rect, size)?
            }
            _ => (size.width / 2, size.height / 2),
        };
        Some(AddAt { page_index: index, center: centre })
    }

    /// Where a tile let go at the screen point `pos` lands: the page under it, the point's place on it, the page's screen
    /// rectangle and size. Nothing unless the point is on the board and the board is the topmost layer there (the background
    /// layer: not a floating bar, a popup or the sidebar laid over the board), and nothing off the pages. The board rectangle
    /// stops at the sidebar, the panel and the window, so a page zoomed or panned under them takes no drop there.
    /// An open inline text edit is on its own page: a drop on another page would move the page under it (68p 3b), so it is
    /// refused (`toolbox_tile` says why). A drop on the edited page is taken, as a tile click is.
    fn toolbox_drop_target(&self, ctx: &Context, pos: Pos2) -> Option<(AddAt, Rect, Size)> {
        self.toolbox_drop_place(ctx, pos).filter(|(at, _, _)| !self.drop_leaves_inline_edit(*at))
    }

    fn drop_leaves_inline_edit(&self, at: AddAt) -> bool {
        self.inline_text_edit.as_ref().is_some_and(|edit| edit.page_index != at.page_index)
    }

    /// `toolbox_drop_target` before the inline text edit's refusal.
    fn toolbox_drop_place(&self, ctx: &Context, pos: Pos2) -> Option<(AddAt, Rect, Size)> {
        let board_rect = self.board_rect?;
        if !board_rect.contains(pos) || ctx.layer_id_at(pos) != Some(LayerId::background()) {
            return None;
        }
        let (index, page_rect) = self.toolbox_page_rects()?.into_iter().find(|(_, rect)| rect.contains(pos))?;
        let size: Size = self.page_sizes().get(index as usize).cloned()?;
        let center = super::screen_to_page_emu(pos, page_rect, size)?;
        Some((AddAt { page_index: index, center }, page_rect, size))
    }

    /// The size (EMU) of the object `action` inserts on a page of `page`, as its flow makes it: `add_node`, `add_canvas_table`
    /// with its default rows, `insert_image` (320 × 240 px), `video_node` (320 × 180 px), the board memo and the content icon
    /// (48 px).
    fn toolbox_object_size(action: Action, page: Size) -> (i64, i64) {
        let px = EMU_PER_CSS_PX;
        match action {
            Action::Command("add.text") => super::edit_fields::new_node_size(NodeKind::Text),
            Action::Command("canvas.table.add") => super::kind_commands::canvas_table_size(page, super::kind_commands::CANVAS_TABLE_DEFAULT_ROWS),
            Action::Command("add.image") => (320 * px, 240 * px),
            Action::Command(_) => super::edit_fields::new_node_size(NodeKind::Rectangle),
            Action::Video => (320 * px, 180 * px),
            Action::Memo => (super::board_memos::DEFAULT_MEMO_WIDTH, super::board_memos::DEFAULT_MEMO_HEIGHT),
            Action::Icon => (48 * px, 48 * px),
            Action::Shape(shape) => super::shape_insert::shape_size(shape),
        }
    }

    /// The outline of the object a tile would insert, at its size on the page under the pointer, centred on the pointer,
    /// with the tile's icon in its middle so the preview names what is dropped (a cylinder, a memo, a table). Nothing where a
    /// drop would be cancelled.
    fn toolbox_preview(&self, ctx: &Context, palette: &theme::Palette, tile: Tile, pos: Pos2) {
        let Some((at, page_rect, size)) = self.toolbox_drop_target(ctx, pos) else {
            return;
        };
        let (width, height) = Self::toolbox_object_size(tile.action, size);
        let bounds = ModelRect { x: at.center.0 - width / 2, y: at.center.1 - height / 2, width, height };
        let rect = super::page_to_screen(bounds, size, page_rect);
        let painter = ctx.layer_painter(LayerId::new(Order::Tooltip, Id::new("toolbox-preview")));
        let corner = CornerRadius::same(theme::RADIUS_CONTROL);
        painter.rect_filled(rect, corner, palette.accent_soft.gamma_multiply(0.5));
        painter.rect_stroke(rect, corner, Stroke::new(2.0, palette.accent), StrokeKind::Inside);
        // 24 px as on the tile; smaller when the outline is (a 20 px operator at a low zoom), and none when it has no room.
        let side = (rect.width().min(rect.height()) - 8.0).min(24.0);
        if side >= 8.0 {
            icons::paint_on(&painter, Rect::from_center_size(rect.center(), Vec2::splat(side)), tile.icon, palette.accent);
        }
    }

    /// The toolbox tab: the groups of tiles, two to a row, and the icon picker while an icon waits for its pick.
    pub(super) fn draw_toolbox(&mut self, ui: &mut Ui, palette: &theme::Palette) {
        let width = ui.available_width();
        let tile_width = (width - TILE_GAP) / 2.0;
        for (group, tiles) in self.toolbox_groups() {
            let (head, _) = ui.allocate_exact_size(Vec2::new(width, theme::LAYER_ROW), Sense::hover());
            ui.painter().text(head.left_center(), Align2::LEFT_CENTER, crate::i18n::tr(&group), theme::display_font(false, 12.0), palette.ink_3);
            for (index, row) in tiles.chunks(2).enumerate() {
                if index > 0 {
                    ui.add_space(TILE_GAP);
                }
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing = Vec2::splat(TILE_GAP);
                    for tile in row {
                        self.toolbox_tile(ui, palette, *tile, tile_width);
                    }
                });
            }
            ui.add_space(SECTION_BOTTOM);
        }
        let ctx = ui.ctx().clone();
        self.draw_toolbox_icon_picker(&ctx, palette);
    }

    /// One tile: the icon (24) over the name (12), hovered or dragged on the surface-2 background. A click acts at the
    /// visible centre; a drag shows the outline and acts where the pointer is let go. Esc during the drag cancels it.
    fn toolbox_tile(&mut self, ui: &mut Ui, palette: &theme::Palette, tile: Tile, width: f32) {
        let (rect, _) = ui.allocate_exact_size(Vec2::new(width, TILE_HEIGHT), Sense::hover());
        let response = ui.interact(rect, Id::new("toolbox-tile").with(tile.label), Sense::click_and_drag());
        let corner = CornerRadius::same(theme::RADIUS_CONTROL);
        if response.hovered() || response.dragged() {
            ui.painter().rect_filled(rect, corner, palette.surface_2);
        }
        icons::paint(ui, Rect::from_center_size(Pos2::new(rect.center().x, rect.top() + 6.0 + 12.0), Vec2::splat(24.0)), tile.icon, palette.ink_2);
        ui.painter().text(Pos2::new(rect.center().x, rect.bottom() - 10.0), Align2::CENTER_CENTER, crate::i18n::tr(&tile.label), theme::display_font(false, 12.0), palette.ink);
        crate::ui_record::note(ui, tile.label, &response);
        let ctx = ui.ctx().clone();
        if response.clicked() {
            if let Some(at) = self.toolbox_visible_centre() {
                self.run_tile(&ctx, tile, at, Pos2::new(rect.right() + 8.0, rect.top()));
            }
        }
        // Only the primary button's drag inserts: a secondary or middle button's drag shows no outline and inserts nothing (as
        // its click does not).
        if response.drag_started_by(egui::PointerButton::Primary) {
            ctx.data_mut(|data| data.insert_temp(Id::new(TOOLBOX_DRAG), response.id));
        }
        let pos = ctx.input(|input| input.pointer.latest_pos());
        if response.dragged_by(egui::PointerButton::Primary) {
            if let Some(pos) = pos {
                self.toolbox_preview(&ctx, palette, tile, pos);
            }
        }
        // egui ends a drag on Esc: that stop is a cancel, and the release that follows ends no drag.
        let escaped = ctx.input(|input| input.key_pressed(Key::Escape));
        if response.drag_stopped_by(egui::PointerButton::Primary) && !escaped {
            // A pointer that left the window has no position: the drop is cancelled.
            match pos.and_then(|pos| self.toolbox_drop_place(&ctx, pos)) {
                Some((at, _, _)) if self.drop_leaves_inline_edit(at) => self.set_error("텍스트 편집을 끝낸 뒤 다른 페이지에 놓으세요".into()),
                Some((at, _, _)) => self.run_tile(&ctx, tile, at, pos.unwrap_or(rect.right_top())),
                None => {}
            }
        }
    }

    /// Runs a tile at `at`: the add command, the memo editor, the video dialog, or the icon picker opened at `anchor`. The
    /// page an insert on another page made current, and the selection it cleared, are put back when the insert does not go
    /// ahead here: refused (an error), its file dialog cancelled, or its memo editor not opened (68p 3a). A refusal that
    /// comes back from the host later is not undone here.
    fn run_tile(&mut self, ctx: &Context, tile: Tile, at: AddAt, anchor: Pos2) {
        // A video while one is inserted or waits for its page change is refused before the page changes, so the refusal puts
        // nothing back.
        if tile.action == Action::Video && self.video_busy(ctx) {
            self.set_error("진행 중인 동영상 추가가 끝날 때까지 기다리세요".into());
            return;
        }
        let before = self.add_focus_state(at.page_index);
        let result = match tile.action {
            Action::Command(id) => {
                let args = json!({ "at": self.at_value(at) });
                let imports = self.pending_calls("asset.import");
                // The picture tile's dialog, cancelled, imports nothing.
                self.invoke(id, args).map(|()| id != "add.image" || self.pending_calls("asset.import") > imports)
            }
            Action::Memo => self.begin_board_memo_centred(ctx, at.page_index, at.center).map(|()| self.board_memo_editor.is_some()),
            // On another page the video waits for the answer to the page change (`run_waiting_toolbox_video`). A drag closes no
            // menu or popover (egui closes them on a click), so the drop closes them.
            Action::Video if before.is_some() => {
                egui::Popup::close_all(ctx);
                self.focus_add_page(at.page_index);
                let artifact_id = self.active.as_ref().map(|active| active.id.clone()).unwrap_or_default();
                let since = ctx.input(|input| input.time);
                let selection_generation = self.selection_generation;
                let revision = self.artifact.as_ref().map_or(0, |artifact| artifact.revision);
                let waiting = WaitingVideo { at, artifact_id, before, since, selection_generation, revision };
                ctx.data_mut(|data| data.insert_temp(Id::new(WAITING_VIDEO), waiting));
                return;
            }
            // A cancelled dialog starts no video insert.
            Action::Video => self.begin_canvas_video_insert_at(Some(at)).map(|()| self.video_insert.is_some()),
            Action::Icon => {
                let frame = ctx.cumulative_frame_nr();
                let artifact_id = self.active.as_ref().map(|active| active.id.clone()).unwrap_or_default();
                ctx.data_mut(|data| data.insert_temp(Id::new(PENDING_ICON), PendingIcon { at, artifact_id, anchor, frame, drawn: frame }));
                Ok(true)
            }
            Action::Shape(shape) => {
                let args = json!({ "shape": shape, "at": self.at_value(at) });
                self.invoke("add.shape", args).map(|()| true)
            }
        };
        match result {
            Ok(true) => {}
            Ok(false) => self.restore_add_focus(before),
            Err(message) => {
                self.restore_add_focus(before);
                self.set_error(message);
            }
        }
    }

    /// How many calls of `method` wait for their answer.
    fn pending_calls(&self, method: &str) -> usize {
        self.pending.values().filter(|pending| pending.method == method).count()
    }

    /// Whether a video insert runs or a video waits for its page change (`WAITING_VIDEO`, read this frame).
    fn video_busy(&self, ctx: &Context) -> bool {
        self.video_insert.is_some() || ctx.data(|data| data.get_temp::<WaitingVideo>(Id::new(WAITING_VIDEO))).is_some()
    }

    /// Puts back the page and selection of a video wait that did not go ahead, unless the selection or the document has changed
    /// since the page change, by the user or by another worker (an agent's commit): that change then stays.
    fn put_back_waiting_video(&mut self, waiting: WaitingVideo) {
        let changed = self.selection_generation != waiting.selection_generation || self.artifact.as_ref().map_or(0, |artifact| artifact.revision) != waiting.revision;
        if !changed {
            self.restore_add_focus(waiting.before);
        }
    }

    /// Ends a video wait that did not go ahead with `message`. The page and selection go back unless an input bound to the page
    /// is open (`page_input_open`): that input stays on the page it was opened on.
    fn end_waiting_video(&mut self, ctx: &Context, waiting: WaitingVideo, message: String) {
        if !self.page_input_open(ctx) {
            self.put_back_waiting_video(waiting);
        }
        self.set_error(message);
    }

    /// Whether an input bound to the current page is open: an inline text edit, a memo editor, the icon picker, or the section
    /// editor.
    fn page_input_open(&self, ctx: &Context) -> bool {
        self.inline_text_edit.is_some()
            || self.board_memo_editor.is_some()
            || ctx.data(|data| data.get_temp::<PendingIcon>(Id::new(PENDING_ICON))).is_some()
            || self.page_section_editor.is_some()
    }

    /// Whether an input is open that a file dialog must not open over: one bound to the page, or a key dialog.
    fn input_open(&self, ctx: &Context) -> bool {
        self.page_input_open(ctx) || self.key_dialog_open()
    }

    /// The video dropped on another page, once the page change's selection request is answered: the video insert checks that
    /// no selection change is on its way (`video_insert_intent`), so its dialog opens only then. The page and the selection
    /// are put back when it does not go ahead (refused, cancelled, or no answer within `VIDEO_WAIT_SECONDS`); a drop on an
    /// artifact or a page no longer shown is dropped. Called once a frame (`App::update`).
    pub(super) fn run_waiting_toolbox_video(&mut self, ctx: &Context) {
        let id = Id::new(WAITING_VIDEO);
        let Some(waiting) = ctx.data_mut(|data| data.remove_temp::<WaitingVideo>(id)) else {
            return;
        };
        if self.active.as_ref().is_none_or(|active| active.id != waiting.artifact_id) || self.view_ref().page_index != waiting.at.page_index {
            return;
        }
        // The editor was left (the home screen): there is no page to put back on, and no file dialog is opened.
        if !self.screen_is_editor() {
            self.set_error("편집 화면을 벗어나 동영상 추가를 취소했습니다".into());
            return;
        }
        if self.pending_calls("selection.set") > 0 {
            if ctx.input(|input| input.time) - waiting.since < VIDEO_WAIT_SECONDS {
                ctx.data_mut(|data| data.insert_temp(id, waiting));
            } else {
                self.end_waiting_video(ctx, waiting, "페이지 전환이 끝나지 않아 동영상을 추가하지 않았습니다".into());
            }
            return;
        }
        // An input the user opened since the drop: the file dialog would open over it.
        if self.input_open(ctx) {
            self.end_waiting_video(ctx, waiting, "다른 입력이 열려 있어 동영상을 추가하지 않았습니다. 끝낸 뒤 다시 놓으세요".into());
            return;
        }
        match self.begin_canvas_video_insert_at(Some(waiting.at)) {
            Ok(()) if self.video_insert.is_some() => {}
            Ok(()) => self.put_back_waiting_video(waiting),
            Err(message) => {
                self.put_back_waiting_video(waiting);
                self.set_error(message);
            }
        }
    }

    /// The icon picker (the insert menu's own grid, `tools.rs` icon_picker) while an icon waits for its pick. It closes on a
    /// pick, on Escape, and on a press outside it.
    fn draw_toolbox_icon_picker(&mut self, ctx: &Context, palette: &theme::Palette) {
        let id = Id::new(PENDING_ICON);
        let Some(mut pending) = ctx.data(|data| data.get_temp::<PendingIcon>(id)) else {
            return;
        };
        pending.drawn = ctx.cumulative_frame_nr();
        let shown = egui::Area::new(Id::new("toolbox-icon-picker")).order(Order::Foreground).fixed_pos(pending.anchor).show(ctx, |ui| {
            egui::Frame::popup(&ctx.style())
                .inner_margin(Margin::same(8))
                .show(ui, |ui| {
                    ui.set_width(260.0);
                    super::tools::icon_picker(ui, palette)
                })
                .inner
        });
        let later = ctx.cumulative_frame_nr() > pending.frame;
        let dismissed = later
            && ctx.input(|input| {
                input.key_pressed(Key::Escape) || (input.pointer.any_pressed() && input.pointer.interact_pos().is_some_and(|pos| !shown.response.rect.contains(pos)))
            });
        if let Some(name) = shown.inner {
            self.finish_icon_pick(ctx, &name);
        } else if dismissed {
            ctx.data_mut(|data| data.remove::<PendingIcon>(id));
        } else {
            ctx.data_mut(|data| data.insert_temp(id, pending));
        }
    }

    /// Drops the icon waiting for its pick when this frame did not draw its picker, or another artifact is open: the toolbox
    /// tab was left, the sidebar closed, the home screen shown or the artifact switched. Called once a frame after the screen is
    /// drawn (`App::update`).
    pub(super) fn forget_hidden_toolbox_icon(&self, ctx: &Context) {
        let id = Id::new(PENDING_ICON);
        let Some(pending) = ctx.data(|data| data.get_temp::<PendingIcon>(id)) else {
            return;
        };
        let same_artifact = self.active.as_ref().is_some_and(|active| active.id == pending.artifact_id);
        if pending.drawn != ctx.cumulative_frame_nr() || !same_artifact {
            ctx.data_mut(|data| data.remove::<PendingIcon>(id));
        }
    }

    /// The icon the picker chose goes to the point it waited for (`add.icon` with `at`). Nothing when no icon waits or it
    /// waited on another artifact.
    pub(super) fn finish_icon_pick(&mut self, ctx: &Context, name: &str) {
        let Some(pending) = ctx.data_mut(|data| data.remove_temp::<PendingIcon>(Id::new(PENDING_ICON))) else {
            return;
        };
        if self.active.as_ref().is_none_or(|active| active.id != pending.artifact_id) {
            return;
        }
        let args = json!({ "icon": name, "at": self.at_value(pending.at) });
        self.invoke_ui("add.icon", args);
    }
}
