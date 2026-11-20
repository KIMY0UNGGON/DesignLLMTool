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
        [("기본", basic), ("도형", shapes), ("미디어", media)]
            .into_iter()
            .map(|(group, tiles)| {
                let tiles: Vec<Tile> = tiles
                    .into_iter()
                    .filter(|tile| match tile.action {
                        Action::Command(id) => commands::available(id),
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
    fn toolbox_drop_target(&self, ctx: &Context, pos: Pos2) -> Option<(AddAt, Rect, Size)> {
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
        }
    }

    /// The outline of the object a tile would insert, at its size on the page under the pointer, centred on the pointer.
    /// Nothing where a drop would be cancelled.
    fn toolbox_preview(&self, ctx: &Context, palette: &theme::Palette, action: Action, pos: Pos2) {
        let Some((at, page_rect, size)) = self.toolbox_drop_target(ctx, pos) else {
            return;
        };
        let (width, height) = Self::toolbox_object_size(action, size);
        let bounds = ModelRect { x: at.center.0 - width / 2, y: at.center.1 - height / 2, width, height };
        let rect = super::page_to_screen(bounds, size, page_rect);
        let painter = ctx.layer_painter(LayerId::new(Order::Tooltip, Id::new("toolbox-preview")));
        let corner = CornerRadius::same(theme::RADIUS_CONTROL);
        painter.rect_filled(rect, corner, palette.accent_soft.gamma_multiply(0.5));
        painter.rect_stroke(rect, corner, Stroke::new(2.0, palette.accent), StrokeKind::Inside);
    }

    /// The toolbox tab: the groups of tiles, two to a row, and the icon picker while an icon waits for its pick.
    pub(super) fn draw_toolbox(&mut self, ui: &mut Ui, palette: &theme::Palette) {
        let width = ui.available_width();
        let tile_width = (width - TILE_GAP) / 2.0;
        for (group, tiles) in self.toolbox_groups() {
            let (head, _) = ui.allocate_exact_size(Vec2::new(width, theme::LAYER_ROW), Sense::hover());
            ui.painter().text(head.left_center(), Align2::LEFT_CENTER, group, theme::display_font(false, 12.0), palette.ink_3);
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
        ui.painter().text(Pos2::new(rect.center().x, rect.bottom() - 10.0), Align2::CENTER_CENTER, tile.label, theme::display_font(false, 12.0), palette.ink);
        crate::ui_record::note(ui, tile.label, &response);
        let ctx = ui.ctx().clone();
        if response.clicked() {
            if let Some(at) = self.toolbox_visible_centre() {
                self.run_tile(&ctx, tile, at, Pos2::new(rect.right() + 8.0, rect.top()));
            }
        }
        if response.drag_started() {
            ctx.data_mut(|data| data.insert_temp(Id::new(TOOLBOX_DRAG), response.id));
        }
        let pos = ctx.input(|input| input.pointer.latest_pos());
        if response.dragged() {
            if let Some(pos) = pos {
                self.toolbox_preview(&ctx, palette, tile.action, pos);
            }
        }
        // egui ends a drag on Esc: that stop is a cancel, and the release that follows ends no drag.
        let escaped = ctx.input(|input| input.key_pressed(Key::Escape));
        if response.drag_stopped() && !escaped {
            // A pointer that left the window has no position: the drop is cancelled.
            if let Some((at, _, _)) = pos.and_then(|pos| self.toolbox_drop_target(&ctx, pos)) {
                self.run_tile(&ctx, tile, at, pos.unwrap_or(rect.right_top()));
            }
        }
    }

    /// Runs a tile at `at`: the add command, the memo editor, the video dialog, or the icon picker opened at `anchor`.
    fn run_tile(&mut self, ctx: &Context, tile: Tile, at: AddAt, anchor: Pos2) {
        let result = match tile.action {
            Action::Command(id) => {
                let args = json!({ "at": self.at_value(at) });
                self.invoke(id, args)
            }
            Action::Memo => self.begin_board_memo_centred(ctx, at.page_index, at.center),
            Action::Video => {
                self.focus_add_page(at.page_index);
                self.begin_canvas_video_insert_at(Some(at))
            }
            Action::Icon => {
                let frame = ctx.cumulative_frame_nr();
                let artifact_id = self.active.as_ref().map(|active| active.id.clone()).unwrap_or_default();
                ctx.data_mut(|data| data.insert_temp(Id::new(PENDING_ICON), PendingIcon { at, artifact_id, anchor, frame, drawn: frame }));
                Ok(())
            }
        };
        if let Err(message) = result {
            self.set_error(message);
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
