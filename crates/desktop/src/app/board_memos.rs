//! Canvas board memos are local editor overlays backed by page-scoped Canvas operations.

use designllm_contracts::{ArtifactBody, ArtifactKind, CanvasBoardMemo, CanvasBoardMemoKind, Rect as EmuRect, Size, CAPABILITY_EXTENSION_BOARD_TITLES, EMU_PER_CSS_PX};
use eframe::egui::{self, Align2, Color32, Context, CornerRadius, Id, Pos2, Rect, Sense, Stroke, Ui, Vec2};

use super::{board::Tool, page_to_screen, screen_to_page_emu, BoardMemoEditor, BoardMemoGesture, BoardMemoMutation, DesktopApp, PendingAction};
use crate::theme;

pub(super) const DEFAULT_MEMO_WIDTH: i64 = 240 * EMU_PER_CSS_PX;
pub(super) const DEFAULT_MEMO_HEIGHT: i64 = 140 * EMU_PER_CSS_PX;
const DEFAULT_MEMO_COLOR: &str = "#FFF2A8";
const DEFAULT_TITLE_COLOR: &str = "#20252D";
/// The card under edit is at least this wide on screen: at a low zoom the card itself leaves no room for a word.
const EDIT_MIN_WIDTH: f32 = 160.0;
/// Between the card under edit and its colour bar.
const BAR_GAP: f32 = 6.0;
/// Memo colours are data the user picks, not UI tokens: the first is the default yellow. Eight swatches.
const MEMO_COLORS: [&str; 8] = ["#FFF2A8", "#D9F99D", "#BAE6FD", "#FBCFE8", "#DDD6FE", "#FED7AA", "#E5E7EB", "#FFFFFF"];
/// Title text colours: the ink default, then the app's accent, danger, warn and agent colours.
const TITLE_COLORS: [&str; 8] = ["#20252D", "#2F64E0", "#1F4BC0", "#C9342B", "#9A5B00", "#4B6B00", "#7A49D8", "#087A6C"];

impl DesktopApp {
    pub(super) fn board_titles_write_supported(&self) -> bool {
        self.context.as_ref()
            .and_then(|context| context.capability_extensions.get(CAPABILITY_EXTENSION_BOARD_TITLES))
            .is_some_and(|support| support.write == designllm_contracts::Support::Supported)
    }

    /// Whether the memo or title editor is on screen and so owns Esc and the keyboard (`draw_board_memo_editor` draws it
    /// on the editor screen once the board has been laid out).
    pub(super) fn board_memo_editor_shown(&self) -> bool {
        self.board_memo_editor.is_some() && self.screen == super::Screen::Editor && self.board_rect.is_some()
    }

    pub(super) fn handle_board_memo_shortcut(&mut self, ctx: &Context) {
        let kind = self.active.as_ref().map(|active| active.kind);
        let board = self.screen == super::Screen::Editor && matches!(kind, Some(ArtifactKind::Canvas | ArtifactKind::Figure | ArtifactKind::Slides));
        if !board || ctx.wants_keyboard_input() || self.key_dialog_open() {
            return;
        }
        // Figure and slides boards have no memos: their tool bar's select and hand tools take `V` and Esc only.
        if kind != Some(ArtifactKind::Canvas) {
            self.handle_board_tool_keys(ctx);
            return;
        }
        // `Delete` on a selected memo removes it (no object is selected with it, `set_selected_ids`); `edit.delete` is for
        // objects and does not see it. Not while a memo is being written, a text is edited inline, an app request runs or a
        // confirmation covers the window: the key is then left to them.
        let memo_delete = self.selected_board_memo.is_some()
            && self.selected_nodes.is_empty()
            && self.inline_text_edit.is_none()
            && self.board_memo_editor.is_none()
            && self.board_memo_mutation.is_none()
            && self.run_context.is_none()
            && !self.has_home_confirmation();
        if memo_delete && ctx.input(|input| input.key_pressed(egui::Key::Delete) && input.modifiers == egui::Modifiers::NONE) {
            ctx.input_mut(|input| { input.consume_key(egui::Modifiers::NONE, egui::Key::Delete); });
            if let Err(message) = self.delete_selected_board_memo() {
                self.set_error(message);
            }
            return;
        }
        self.handle_board_tool_keys(ctx);
        if !ctx.input(|input| input.key_pressed(egui::Key::N) && input.modifiers == egui::Modifiers::NONE) {
            return;
        }
        ctx.input_mut(|input| { input.consume_key(egui::Modifiers::NONE, egui::Key::N); });
        self.view().tool = Tool::Memo;
    }

    /// `V` picks the select tool, and Esc leaves the memo or hand tool for the select tool (a memo made picks the select tool
    /// itself, `begin_board_memo_at`). Neither acts while a memo editor, an inline text edit or a page section editor owns
    /// the keys, or a home confirmation covers the window; Esc also waits for an open menu, a memo save in flight, and the
    /// toolbox's Esc (its icon picker, a tile's drag: `toolbox::takes_escape`).
    /// An Esc that leaves a tool is consumed here, so `select.clear` (run after this) keeps the selection.
    fn handle_board_tool_keys(&mut self, ctx: &Context) {
        let keys_free = self.board_memo_editor.is_none()
            && self.inline_text_edit.is_none()
            && self.page_section_editor.is_none()
            && !self.has_home_confirmation()
            && !self.key_dialog_open();
        let tool = self.view_ref().tool;
        if !keys_free || tool == Tool::Select {
            return;
        }
        if ctx.input(|input| input.key_pressed(egui::Key::V) && input.modifiers == egui::Modifiers::NONE) {
            ctx.input_mut(|input| { input.consume_key(egui::Modifiers::NONE, egui::Key::V); });
            self.view().tool = Tool::Select;
        } else if self.board_memo_mutation.is_none() && !egui::Popup::is_any_open(ctx) && !super::toolbox::takes_escape(ctx)
            && ctx.input(|input| input.key_pressed(egui::Key::Escape) && input.modifiers == egui::Modifiers::NONE)
        {
            ctx.input_mut(|input| { input.consume_key(egui::Modifiers::NONE, egui::Key::Escape); });
            self.view().tool = Tool::Select;
        }
    }

    /// Selects memo `memo_id` in place of any selected object.
    fn select_board_memo(&mut self, memo_id: &str) {
        if !self.selected_nodes.is_empty() {
            self.select_clear();
        }
        self.selected_board_memo = Some(memo_id.to_owned());
    }

    /// Removes the selected memo in one memo change (`submit_board_memo_change`), after the checks every memo change makes:
    /// the page drawn at the document's revision, and no reservation on the page's memos. Returns why it did not.
    fn delete_selected_board_memo(&mut self) -> Result<(), String> {
        let Some(memo_id) = self.selected_board_memo.clone() else {
            return Ok(());
        };
        let page_index = self.artifact.as_ref().and_then(|artifact| match &artifact.body {
            ArtifactBody::Canvas(document) => document
                .board_memos
                .iter()
                .find(|memo| memo.id == memo_id)
                .and_then(|memo| document.pages.iter().position(|page| page.id == memo.page_id)),
            _ => None,
        });
        let page_index = page_index.and_then(|index| u32::try_from(index).ok()).ok_or("삭제할 메모를 찾지 못했습니다")?;
        let (artifact_id, revision, page_id, expected) = self.board_memo_page(page_index).ok_or("삭제할 메모를 찾지 못했습니다")?;
        let memo = expected.iter().find(|memo| memo.id == memo_id).cloned().ok_or("삭제할 메모를 찾지 못했습니다")?;
        let page_size = self.page_sizes().get(page_index as usize).copied().ok_or("삭제할 메모를 찾지 못했습니다")?;
        if !self.board_memo_page_ready(&artifact_id, revision, &page_id, page_index, page_size) {
            return Err("최신 페이지 그림을 기다리는 동안 메모를 삭제하지 않았습니다".into());
        }
        if self.memo_page_reserved(&artifact_id, &page_id, &expected) {
            return Err("페이지의 다른 편집이 진행 중입니다".into());
        }
        let desired = remove_page_memo(&expected, &memo_id).ok_or("삭제할 메모를 찾지 못했습니다")?;
        let change = BoardMemoEditor { artifact_id, revision, page_id, page_index, expected, memo, is_new: false, message: None };
        self.submit_board_memo_change(change, desired, None);
        Ok(())
    }

    /// Draws page-local board annotations outside the rendered page scene and its export path.
    pub(super) fn draw_board_memos(
        &mut self,
        ctx: &Context,
        ui: &mut Ui,
        page_index: u32,
        page_size: Size,
        page_rect: Rect,
    ) {
        let Some((artifact_id, revision, page_id, memos)) = self.board_memo_page(page_index) else {
            return;
        };
        let ready = self.board_memo_page_ready(&artifact_id, revision, &page_id, page_index, page_size);
        let view = self.view_ref();
        let space_down = ctx.input(|input| input.key_down(egui::Key::Space));
        let interactive = self.board_memo_input_blocked(space_down).is_none();
        let memo_tool = view.tool == Tool::Memo && interactive;

        if !ready {
            ui.painter().text(
                page_rect.left_top() + Vec2::new(8.0, 8.0),
                Align2::LEFT_TOP,
                crate::i18n::tr("메모 위치를 갱신하는 중입니다"),
                egui::FontId::proportional(12.0),
                theme::palette(ctx).ink_2,
            );
            return;
        }

        if memo_tool {
            let response = ui.interact(
                page_rect,
                Id::new(("board-memo-page", artifact_id.as_str(), page_id.as_str())),
                Sense::click(),
            );
            if response.clicked() {
                if let Some(position) = response.interact_pointer_pos() {
                    let over_memo = memos.iter().any(|memo| {
                        page_to_screen(memo.bounds, page_size, page_rect).contains(position)
                    });
                    if !over_memo {
                        self.region_clicked = true;
                        if let Some(top_left) = screen_to_page_emu(position, page_rect, page_size) {
                            self.begin_board_memo_at(page_index, top_left, page_size);
                        }
                    }
                }
            }
        }

        for memo in &memos {
            let mut screen_rect = page_to_screen(memo.bounds, page_size, page_rect);
            if screen_rect.width() < 12.0 || screen_rect.height() < 12.0 {
                continue;
            }
            let title_supported = memo.kind != CanvasBoardMemoKind::Title || self.board_titles_write_supported();
            if interactive && title_supported && self.board_memo_editor.is_none() && self.board_memo_mutation.is_none() {
                let response = ui.interact(
                    screen_rect,
                    Id::new(("board-memo", artifact_id.as_str(), memo.id.as_str())),
                    Sense::click_and_drag(),
                );
                super::board::note_board_press(ui.ctx(), &response);
                if response.drag_started_by(egui::PointerButton::Primary) {
                    self.region_clicked = true;
                    self.select_board_memo(&memo.id);
                    self.board_memo_gesture = Some(BoardMemoGesture {
                        artifact_id: artifact_id.clone(),
                        revision,
                        page_id: page_id.clone(),
                        page_index,
                        // Where the press began: egui reports the drag only after the click distance (see edit_fields.rs).
                        start: ui.input(|input| input.pointer.press_origin()).or(response.interact_pointer_pos()).unwrap_or(screen_rect.center()),
                        page_rect,
                        page_size,
                        tool_at_start: view.tool,
                        space_down_at_start: space_down,
                        zoom_at_start: view.zoom,
                        pan_at_start: view.pan,
                        transform_id_at_start: self.page_transforms.get(&page_index).map(|transform| transform.id.clone()),
                        invalidated: false,
                        memo_id: Some(memo.id.clone()),
                        original_bounds: Some(memo.bounds),
                        expected: memos.clone(),
                        memo: memo.clone(),
                    });
                }
                if response.dragged_by(egui::PointerButton::Primary) {
                    self.region_clicked = true;
                    if let Some(gesture) = self.board_memo_gesture.as_ref().filter(|gesture| gesture.memo_id.as_deref() == Some(memo.id.as_str())) {
                        let delta = response.interact_pointer_pos().unwrap_or(gesture.start) - gesture.start;
                        let bounds = moved_bounds(
                            gesture.original_bounds.unwrap_or(gesture.memo.bounds),
                            delta,
                            gesture.page_rect,
                            gesture.page_size,
                        );
                        screen_rect = page_to_screen(bounds, page_size, page_rect);
                    }
                }
                let already_selected = self.selected_board_memo.as_deref() == Some(memo.id.as_str());
                if response.double_clicked() || (response.clicked() && already_selected) {
                    self.region_clicked = true;
                    self.select_board_memo(&memo.id);
                    self.open_board_memo_editor(page_index, &memo.id);
                } else if response.clicked() {
                    self.region_clicked = true;
                    self.select_board_memo(&memo.id);
                }
            }

            // The card under edit is drawn by `draw_board_memo_editor`, above the board.
            if self.board_memo_editor.as_ref().is_some_and(|editor| editor.memo.id == memo.id) {
                continue;
            }
            let is_title = memo.kind == CanvasBoardMemoKind::Title;
            let palette = theme::palette(ctx);
            let color = crate::widgets::parse_hex(&memo.color).unwrap_or(if is_title {
                palette.ink
            } else {
                Color32::from_rgb(255, 242, 168)
            });
            let selected = self.selected_board_memo.as_deref() == Some(memo.id.as_str());
            if is_title {
                let preview = truncate(memo.text.lines().next().unwrap_or_default(), 42);
                ui.painter().text(
                    screen_rect.shrink(8.0).left_top(),
                    Align2::LEFT_TOP,
                    preview,
                    theme::font(theme::Type::BoardTitle),
                    color,
                );
                if selected {
                    ui.painter().rect_stroke(screen_rect, CornerRadius::same(4), Stroke::new(1.5, palette.accent), egui::StrokeKind::Inside);
                }
            } else {
                // A paper note: a soft float shadow, then the wrapped text, clipped to the card with an ellipsis when it is taller.
                theme::paint_shadow(ui.painter(), screen_rect, CornerRadius::same(6), theme::Elevation::Float);
                ui.painter().rect_filled(screen_rect, CornerRadius::same(6), color);
                if selected {
                    ui.painter().rect_stroke(screen_rect, CornerRadius::same(6), Stroke::new(1.5, palette.accent), egui::StrokeKind::Inside);
                }
                let text_rect = screen_rect.shrink(10.0);
                let font = theme::font(theme::Type::Body);
                let galley = ui.painter().layout(memo.text.clone(), font.clone(), palette.ink, text_rect.width().max(0.0));
                let overflow = galley.size().y > text_rect.height();
                ui.painter().with_clip_rect(screen_rect).galley(text_rect.min, galley, palette.ink);
                if overflow {
                    ui.painter().text(screen_rect.right_bottom() - Vec2::new(8.0, 6.0), Align2::RIGHT_BOTTOM, "…", font, palette.ink_2);
                }
            }
        }

        // A selected card (not under edit, not being dragged) has its bar: a swatch or delete is one board memo change.
        let selected = self.selected_board_memo.as_deref().and_then(|id| memos.iter().find(|memo| memo.id == id));
        let free = interactive && self.board_memo_editor.is_none() && self.board_memo_mutation.is_none() && self.board_memo_gesture.is_none();
        if let (true, Some(memo), Some(board)) = (free, selected, self.board_rect) {
            let is_title = memo.kind == CanvasBoardMemoKind::Title;
            if !is_title || self.board_titles_write_supported() {
                let bar = MemoBar {
                    id: Id::new(("board-memo-selected-bar", artifact_id.as_str(), memo.id.as_str())),
                    card: page_to_screen(memo.bounds, page_size, page_rect),
                    board,
                    is_title,
                    color: &memo.color,
                    enabled: true,
                    delete: true,
                    custom: None,
                    notice: None,
                };
                let actions = bar.show(ctx, &theme::palette(ctx));
                if let Some(hex) = actions.picked.filter(|hex| !hex.eq_ignore_ascii_case(&memo.color)) {
                    self.change_selected_memo(page_index, memo, Some(hex));
                } else if actions.delete {
                    self.change_selected_memo(page_index, memo, None);
                }
            }
        }
    }

    /// The colour (`Some`) or the deletion (`None`) of a selected memo from its bar: one board memo change against the
    /// page's memos, as a move is, without the editor.
    fn change_selected_memo(&mut self, page_index: u32, memo: &CanvasBoardMemo, color: Option<String>) {
        let Some((artifact_id, revision, page_id, expected)) = self.board_memo_page(page_index) else { return };
        let mut changed = memo.clone();
        let desired = match color {
            Some(hex) => {
                changed.color = hex;
                replace_page_memo(&expected, &changed, false)
            }
            None => remove_page_memo(&expected, &memo.id),
        };
        let Some(desired) = desired else { return };
        let deleted = !desired.iter().any(|item| item.id == memo.id);
        let editor = BoardMemoEditor { artifact_id, revision, page_id, page_index, expected, memo: changed, is_new: false, message: None };
        let current = self.board_memo_editor_is_current(&editor)
            && !self.memo_page_reserved(&editor.artifact_id, &editor.page_id, &editor.expected);
        let selected = (!deleted).then(|| memo.id.clone());
        self.submit_board_memo_change_with_message(
            editor,
            desired,
            selected,
            (!current).then(|| "작업물 또는 메모 예약이 바뀌었습니다. 메모 변경은 보존했고 최신 상태에 적용하지 않았습니다.".into()),
        );
    }

    /// The open memo or title editor, on its card above the board. It is drawn after the board (`update`), so it shows
    /// and keeps the keyboard whether or not its page is on screen or ready; a draft whose page changed gets the notice.
    pub(super) fn draw_board_memo_editor(&mut self, ctx: &Context) {
        if self.board_memo_editor.as_ref().is_some_and(|editor| editor.message.is_none() && !self.board_memo_editor_is_current(editor)) {
            if let Some(editor) = self.board_memo_editor.as_mut() {
                editor.message = Some("작업물 또는 메모가 바뀌었습니다. 이 초안은 최신 상태에 자동 적용하지 않습니다.".into());
            }
        }
        if !self.board_memo_editor_shown() { return; }
        let (Some(board), Some(editor)) = (self.board_rect, self.board_memo_editor.as_ref()) else { return };
        let card = self.current_memo_page_state(editor.page_index)
            .filter(|(artifact_id, page_id, _)| artifact_id == &editor.artifact_id && page_id == &editor.page_id)
            .zip(self.page_sizes().get(editor.page_index as usize).copied())
            .map(|((_, _, page_rect), page_size)| page_to_screen(editor.memo.bounds, page_size, page_rect))
            .filter(|card| card.is_positive())
            // A page that left the board keeps the draft at the board's corner, under its notice.
            .unwrap_or(Rect::from_min_size(board.min + Vec2::splat(16.0), Vec2::new(EDIT_MIN_WIDTH, 64.0)));
        self.draw_editing_memo(ctx, board, card);
    }

    /// The card under edit: the text field on the card's colour, as tall as its text (down to the board's bottom, then
    /// it scrolls with the caret), and a floating bar beside it with the swatches, the custom colour and delete.
    /// A primary press outside both, Esc, or a title's Enter confirms; a key that ends an IME composition does not.
    fn draw_editing_memo(&mut self, ctx: &Context, board: Rect, card: Rect) {
        let run_context_busy = self.run_context.is_some();
        let Some(editor) = self.board_memo_editor.as_mut() else { return };
        let artifact_id = editor.artifact_id.clone();
        let memo_id = editor.memo.id.clone();
        let is_title = editor.memo.kind == CanvasBoardMemoKind::Title;
        let is_new = editor.is_new;
        // The draft leaves the editor while it is drawn and goes back below (no early return in between): no copy a frame.
        let mut text = std::mem::take(&mut editor.memo.text);
        let mut color = std::mem::take(&mut editor.memo.color);
        let message = editor.message.take();
        let can_edit = message.is_none() && !run_context_busy;
        let palette = theme::palette(ctx);
        let text_id = memo_text_edit_id(&artifact_id, &memo_id);
        let card_id = Id::new(("board-memo-card", artifact_id.as_str(), memo_id.as_str()));
        let bar_id = Id::new(("board-memo-bar", artifact_id.as_str(), memo_id.as_str()));
        let composing_id = Id::new(("board-memo-composing", memo_id.as_str()));
        let custom_id = Id::new(("board-memo-custom-color", memo_id.as_str()));

        // Keys are read before the field runs: egui drops the focus on Esc before any widget. A key pressed while an IME
        // composition is open belongs to the IME (it ends or cancels the composition). The composition usually began
        // frames before the key, so its state is kept across frames.
        let mut composing = ctx.data(|data| data.get_temp::<bool>(composing_id).unwrap_or(false));
        let (mut escape, mut enter, mut primary_press) = (false, false, None);
        ctx.input(|input| {
            for event in &input.events {
                match event {
                    egui::Event::Ime(egui::ImeEvent::Preedit(preedit)) => composing = !preedit.is_empty(),
                    egui::Event::Ime(egui::ImeEvent::Commit(_) | egui::ImeEvent::Disabled) => composing = false,
                    egui::Event::Key { key: egui::Key::Escape, pressed: true, .. } => escape |= !composing,
                    egui::Event::Key { key: egui::Key::Enter, pressed: true, modifiers, .. } => enter |= !composing && modifiers.is_none(),
                    egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed: true, .. } => {
                        primary_press = primary_press.or(Some(*pos));
                    }
                    _ => {}
                }
            }
        });
        ctx.data_mut(|data| data.insert_temp(composing_id, composing));
        // A press is judged against where the card and the bar were seen (the last frame). A card not seen yet was opened
        // by this frame's press (the memo tool's click), which does not confirm it. Only a press on the window's panels
        // and board (the background layer) is outside: a press on a dialog, a notice or a menu above them is theirs.
        let (seen_card, seen_bar) = ctx.memory(|memory| (memory.area_rect(card_id), memory.area_rect(bar_id)));
        let outside = primary_press.zip(seen_card).is_some_and(|(pos, seen_card)| {
            !seen_card.contains(pos) && !seen_bar.is_some_and(|bar| bar.contains(pos))
                && ctx.layer_id_at(pos).is_none_or(|layer| layer == egui::LayerId::background())
                && ctx.memory(|memory| memory.top_modal_layer()).is_none()
        });
        if can_edit && !outside {
            give_keyboard(ctx, text_id, seen_card.is_none());
        }

        let font = theme::font(if is_title { theme::Type::BoardTitle } else { theme::Type::Body });
        let fill = crate::widgets::parse_hex(&color).unwrap_or(if is_title { palette.ink } else { Color32::from_rgb(255, 242, 168) });
        let text_color = if is_title { fill } else { palette.ink };
        let padding: i8 = if is_title { 4 } else { 10 };
        let pad = f32::from(padding);
        let row = ctx.fonts_mut(|fonts| fonts.row_height(&font));
        let width = card.width().max(EDIT_MIN_WIDTH).min(board.width());
        let room_below = board.bottom() - card.top().max(board.top()) - 2.0 * pad;
        let max_text_height = if room_below >= 3.0 * row { room_below } else { board.height() - 2.0 * pad }.max(row);
        let corner = CornerRadius::same(if is_title { 4 } else { 6 });
        let card_area = egui::Area::new(card_id)
            .order(egui::Order::Middle)
            .fixed_pos(card.min)
            .constrain_to(board)
            .show(ctx, |ui| {
                // The shadow and the paper go under the text once its size is known.
                let backdrop = [ui.painter().add(egui::Shape::Noop), ui.painter().add(egui::Shape::Noop), ui.painter().add(egui::Shape::Noop)];
                let framed = egui::Frame::new().inner_margin(egui::Margin::same(padding)).show(ui, |ui| {
                    ui.set_width(width - 2.0 * pad);
                    ui.set_min_height((card.height() - 2.0 * pad).clamp(row, max_text_height));
                    ui.add_enabled_ui(can_edit, |ui| {
                        egui::ScrollArea::vertical()
                            .id_salt(card_id)
                            .max_height(max_text_height)
                            .auto_shrink([false, true])
                            .show(ui, |ui| {
                                let field = crate::widgets::Field {
                                    width: f32::INFINITY,
                                    font: Some(font.clone()),
                                    color: Some(text_color),
                                    id: Some(text_id),
                                    borderless: true,
                                    enter_keeps_focus: is_title,
                                    ..Default::default()
                                };
                                if is_title {
                                    crate::widgets::edit_line(ui, &palette, &mut text, field);
                                } else {
                                    crate::widgets::edit_area(ui, &palette, &mut text, crate::widgets::Field { rows: 1, ..field });
                                }
                            });
                    });
                });
                let rect = framed.response.rect;
                if !is_title {
                    let [near, far] = theme::shadow(ctx, theme::Elevation::Float);
                    ui.painter().set(backdrop[0], near.as_shape(rect, corner));
                    ui.painter().set(backdrop[1], far.as_shape(rect, corner));
                    ui.painter().set(backdrop[2], egui::epaint::RectShape::filled(rect, corner, fill));
                }
                ui.painter().rect_stroke(rect, corner, Stroke::new(1.5, palette.accent), egui::StrokeKind::Inside);
            });
        let card_rect = card_area.response.rect;

        let custom_open = ctx.data(|data| data.get_temp::<bool>(custom_id).unwrap_or(false));
        // Protection notices (changed work, running request) with the draft's routes out.
        let notice = (!can_edit).then(|| message.as_deref().unwrap_or("앱 요청이 끝난 뒤 편집할 수 있습니다. 현재 초안은 보존됩니다."));
        let bar = MemoBar { id: bar_id, card: card_rect, board, is_title, color: &color, enabled: can_edit, delete: !is_new, custom: Some(custom_open), notice };
        let BarActions { picked, delete, custom_toggled, copy, discard } = bar.show(ctx, &palette);
        if custom_toggled { ctx.data_mut(|data| data.insert_temp(custom_id, !custom_open)); }

        if let Some(hex) = picked { color = hex; }
        if let Some(editor) = self.board_memo_editor.as_mut().filter(|editor| editor.memo.id == memo_id && editor.artifact_id == artifact_id) {
            editor.memo.text = text;
            editor.memo.color = color;
            editor.message = message;
        }
        if copy {
            if let Some(editor) = self.board_memo_editor.as_ref() { ctx.copy_text(memo_copy_payload(editor)); }
        }
        if discard {
            self.discard_editing_memo(ctx);
            return;
        }
        if delete {
            self.delete_board_memo_editor();
            if self.board_memo_editor.is_none() { forget_editing_memo(ctx, &memo_id); }
            return;
        }
        if outside || escape || (is_title && enter) {
            self.finish_editing_memo(ctx);
        }
        // A press on the bar (a swatch, the picker) took the keyboard from the field: it goes back now, so that the next
        // key is the memo's and not a board shortcut or the selected node's Enter. The IME keeps the place the field
        // gave it, so a composition is not switched off for the frame the field was without the keyboard.
        let ime_id = Id::new(("board-memo-ime", memo_id.as_str()));
        if let Some(ime) = ctx.output(|output| output.ime) {
            ctx.data_mut(|data| data.insert_temp(ime_id, ime));
        }
        if can_edit && self.board_memo_editor.as_ref().is_some_and(|editor| editor.memo.id == memo_id) && give_keyboard(ctx, text_id, false) {
            if let Some(ime) = ctx.data(|data| data.get_temp::<egui::output::IMEOutput>(ime_id)) {
                ctx.output_mut(|output| { output.ime.get_or_insert(ime); });
            }
        }
    }

    /// Confirms the card under edit: an empty new memo is dropped, an unchanged memo just closes, a change is saved, and
    /// a change that cannot be saved now goes to the memo notice with its draft.
    fn finish_editing_memo(&mut self, ctx: &Context) {
        let Some(editor) = self.board_memo_editor.as_ref() else { return };
        let memo_id = editor.memo.id.clone();
        let unchanged = editor.expected.iter().find(|memo| memo.id == memo_id)
            .is_some_and(|memo| memo.text == editor.memo.text && memo.color == editor.memo.color);
        let empty_new = editor.is_new && editor.memo.text.trim().is_empty();
        if empty_new || (!editor.is_new && unchanged) {
            self.discard_editing_memo(ctx);
        } else if editor.message.is_some() || self.run_context.is_some() {
            self.preserve_editing_memo();
        } else {
            self.save_board_memo_editor();
            // A save that found the page changed keeps the draft in the memo notice; a refused one (an empty title) has
            // said why, and the card goes back to the memo as it was, so the keyboard is not held by a card that cannot close.
            if self.board_memo_editor.as_ref().is_some_and(|editor| editor.message.is_some()) {
                self.preserve_editing_memo();
            } else if self.board_memo_editor.is_some() {
                self.discard_editing_memo(ctx);
            }
        }
        if self.board_memo_editor.is_none() { forget_editing_memo(ctx, &memo_id); }
    }

    /// A draft that cannot be saved now (its page or memos changed, or an app request is running) leaves the card for
    /// the memo notice (`draw_board_memo_mutation`), which keeps it with its copy and discard routes. Nothing is sent.
    fn preserve_editing_memo(&mut self) {
        let Some(editor) = self.board_memo_editor.take() else { return };
        let Some(desired) = replace_page_memo(&editor.expected, &editor.memo, editor.is_new) else {
            self.board_memo_editor = Some(editor);
            return;
        };
        let message = editor.message.clone().unwrap_or_else(|| "앱 요청 중에는 메모를 변경하지 않았습니다. 초안은 보존했습니다.".into());
        let memo_id = editor.memo.id.clone();
        self.submit_board_memo_change_with_message(editor, desired, Some(memo_id), Some(message));
    }

    fn discard_editing_memo(&mut self, ctx: &Context) {
        let Some(editor) = self.board_memo_editor.take() else { return };
        if editor.is_new { self.selected_board_memo = None; }
        self.clear_note_draft(&editor.memo.id);
        forget_editing_memo(ctx, &editor.memo.id);
    }

    pub(super) fn draw_board_memo_mutation(&mut self, ctx: &Context) {
        let Some(mutation) = self.board_memo_mutation.as_ref() else { return };
        let Some(board_rect) = self.board_rect else { return };
        let token = mutation.token.clone();
        let artifact_id = mutation.artifact_id.clone();
        let page_id = mutation.page_id.clone();
        let page_index = mutation.page_index;
        let revision = mutation.revision;
        let expected_count = mutation.expected.len();
        let desired_count = mutation.desired.len();
        let message = mutation.message.clone();
        let pending = mutation.pending || self.board_memo_request_in_flight();
        let outcome_uncertain = mutation.outcome_uncertain;
        let draft = mutation.editor.as_ref().map(|editor| (
            editor.artifact_id.clone(), editor.page_id.clone(), editor.page_index, editor.memo.clone(),
        ));
        let held_edit_id = mutation.held_edit_id.clone();
        let mut discard = false;
        let mut copy = false;
        let mut retry = false;
        let retry_allowed = self.board_memo_mutation_retry_allowed(mutation);
        use crate::widgets::{dialog_buttons, DialogButton, DialogChoice};
        let palette = theme::palette(ctx);
        let title = if pending { "메모 저장 중" } else if outcome_uncertain { "메모 결과 확인 필요" } else { "메모 변경 보존됨" };
        // A notice at the top middle of the board, stacked with the board's other notices (68d).
        crate::widgets::dialog(ctx, &palette, Id::new("board-memo-conflict"), title, crate::widgets::DialogPlace::BoardTop(board_rect), |ui| {
                        ui.label(crate::i18n::tr(&message.as_deref().unwrap_or("작업물의 최신 상태를 확인하고 있습니다. 메모를 보존했습니다.")));
                        ui.label(crate::i18n::tr(&format!(
                            "작업물 {} · 페이지 {} ({}) · 기준 revision {} · 페이지 메모 {}개 → {}개",
                            artifact_id,
                            page_index + 1,
                            page_id,
                            revision,
                            expected_count,
                            desired_count,
                        )));
                        if let Some((_, _, _, memo)) = draft.as_ref() {
                            ui.label(crate::i18n::tr("저장되지 않은 메모 초안:"));
                            ui.label(egui::RichText::new(truncate(&memo.text, 240)).monospace());
                            let pressed = dialog_buttons(ui, &palette, &[
                                DialogChoice::new("초안과 위치 복사", DialogButton::Plain).enabled(!pending),
                                DialogChoice::new("같은 기준으로 다시 시도", DialogButton::Primary).enabled(retry_allowed),
                                DialogChoice::new("초안 버리기", DialogButton::Danger).enabled(!pending),
                            ]);
                            (copy, retry, discard) = (pressed[0].clicked(), pressed[1].clicked(), pressed[2].clicked());
                        } else {
                            ui.label(crate::i18n::tr("이동 변경은 최신 상태에 자동 적용하지 않습니다."));
                            let pressed = dialog_buttons(ui, &palette, &[
                                DialogChoice::new("같은 기준으로 다시 시도", DialogButton::Primary).enabled(retry_allowed),
                                DialogChoice::new("이동 변경 버리기", DialogButton::Danger).enabled(!pending),
                            ]);
                            (retry, discard) = (pressed[0].clicked(), pressed[1].clicked());
                        }
                        // A held memo change never takes the held-edit check: it is held only by the apply preflight (apply.rs: a video,
                        // image replacement or crop being applied), which sets `can_retry` false, and the replies leave its failures out of the
                        // re-hold (replies.rs), so `check_held_edit` would do nothing and `Ready` is never reached. The draft's own copy and
                        // discard buttons above are its way out; only the automatic retry after a job's release is reported.
                        let held = held_edit_id.as_deref().and_then(|held_id| self.held_edits.iter().find(|held| held.id == held_id));
                        if held.is_some_and(|held| matches!(held.state, super::state::HeldEditState::Sending | super::state::HeldEditState::WaitingForJobRelease)) {
                            ui.label(crate::i18n::tr("보류한 메모 변경을 처리 중입니다."));
                        }
            });
        if copy {
            if let Some((artifact_id, page_id, page_index, memo)) = draft.as_ref() {
                ctx.copy_text(memo_copy_payload_parts(artifact_id, page_id, *page_index, memo));
            }
        }
        if retry { self.retry_board_memo_mutation(&token); }
        if discard { self.discard_board_memo_mutation(&token); }
    }

    pub(super) fn finish_board_memo_gesture_if_released(&mut self, ctx: &Context) {
        let (released, position) = ctx.input(|input| (
            input.pointer.button_released(egui::PointerButton::Primary),
            input.pointer.latest_pos(),
        ));
        if !released { return; }
        self.finish_board_memo_gesture_at(position);
    }

    /// Ends a memo drag whose release will not come: the primary button is up with no release in this frame, or the window
    /// lost focus (egui keeps a held button down then, so a release made in another window never reaches the drag). The
    /// memo keeps its place, and the settle the drag held back (`draw_board`) goes on.
    pub(super) fn drop_lost_board_memo_gesture(&mut self, ctx: &Context) {
        if self.board_memo_gesture.is_none() { return; }
        let lost = ctx.input(|input| !input.pointer.button_released(egui::PointerButton::Primary) && (
            !input.pointer.primary_down()
                || input.events.iter().any(|event| matches!(event, egui::Event::WindowFocused(false)))
        ));
        if lost {
            self.board_memo_gesture = None;
            self.set_error("메모를 끄는 동안 포인터 입력이 끊겨 이동을 취소했습니다. 기존 메모는 유지했습니다".into());
        }
    }

    pub(super) fn observe_board_memo_gesture(&mut self, ctx: &Context) {
        let Some(gesture) = self.board_memo_gesture.as_ref() else { return };
        let page_index = gesture.page_index;
        let current_view = self.view_ref();
        let current_tool = current_view.tool;
        let current_page_state = self.current_memo_page_state(page_index);
        let same_target_page = current_page_state.as_ref().is_some_and(|(artifact_id, page_id, _)| {
            artifact_id == &gesture.artifact_id && page_id == &gesture.page_id
        });
        let current_page_rect = current_page_state.as_ref().map(|(_, _, rect)| *rect);
        let current_transform_id = self.page_transforms.get(&page_index).map(|transform| transform.id.clone());
        let space_down = ctx.input(|input| input.key_down(egui::Key::Space));
        let space_changed_this_frame = ctx.input(|input| input.events.iter().any(|event| matches!(
            event,
            egui::Event::Key { key: egui::Key::Space, .. }
        )));
        if let Some(gesture) = self.board_memo_gesture.as_mut() {
            // The drag maps the pointer through `page_rect` and `page_size` alone (`moved_bounds`), not through the dpi. A page
            // texture at another dpi (the sharp render a settle asked for before the drag began) gives the page a new transform
            // id on the same rectangle, zoom and pan: that id becomes the drag's own. A new id with a moved page still ends it.
            let placement_kept = same_target_page
                && current_page_rect == Some(gesture.page_rect)
                && current_view.zoom.to_bits() == gesture.zoom_at_start.to_bits()
                && current_view.pan == gesture.pan_at_start;
            if placement_kept && current_transform_id != gesture.transform_id_at_start {
                gesture.transform_id_at_start = current_transform_id.clone();
            }
            observe_gesture_transform(
                gesture,
                current_tool,
                space_down,
                space_changed_this_frame,
                current_view.zoom,
                current_view.pan,
                same_target_page,
                current_page_rect,
                current_transform_id.as_deref(),
            );
        }
    }

    fn current_memo_page_state(&self, page_index: u32) -> Option<(String, String, Rect)> {
        if self.screen != super::Screen::Editor { return None; }
        let active = self.active.as_ref().filter(|active| active.kind == ArtifactKind::Canvas)?;
        let artifact = self.artifact.as_ref().filter(|artifact| artifact.kind == ArtifactKind::Canvas && artifact.id == active.id)?;
        let page_id = match &artifact.body {
            ArtifactBody::Canvas(document) => document.pages.get(page_index as usize)?.id.clone(),
            _ => return None,
        };
        let board_rect = self.board_rect?;
        let view = self.view_ref();
        let slots = crate::board::arrange(active.kind, &self.page_sizes(), view.zoom);
        let slot = slots.iter().find(|slot| slot.page_index == page_index)?;
        Some((artifact.id.clone(), page_id, crate::board::to_screen(slot.world, board_rect.min + view.pan, view.zoom)))
    }

    fn finish_board_memo_gesture_at(&mut self, position: Option<Pos2>) {
        let Some(gesture) = self.board_memo_gesture.take() else { return };
        if gesture.invalidated {
            self.set_error("메모를 끄는 동안 도구나 화면 변환이 바뀌어 이동을 취소했습니다. 기존 메모는 유지했습니다".into());
            return;
        }
        let position = position.unwrap_or(gesture.start);
        let bounds = moved_bounds(
            gesture.original_bounds.unwrap_or(gesture.memo.bounds),
            position - gesture.start,
            gesture.page_rect,
            gesture.page_size,
        );
        if bounds == gesture.memo.bounds { return; }
        if self.board_memo_page_ready(&gesture.artifact_id, gesture.revision, &gesture.page_id, gesture.page_index, gesture.page_size)
            && self.run_context.is_none()
        {
            self.submit_memo_move(gesture, bounds);
        } else {
            self.preserve_stale_memo_move(gesture, bounds);
        }
    }

    pub(super) fn board_memo_page(&self, page_index: u32) -> Option<(String, u64, String, Vec<CanvasBoardMemo>)> {
        let active = self.active.as_ref()?;
        let artifact = self.artifact.as_ref()?;
        let document = match &artifact.body {
            ArtifactBody::Canvas(document) if active.kind == ArtifactKind::Canvas && artifact.kind == ArtifactKind::Canvas => document,
            _ => return None,
        };
        let page = document.pages.get(page_index as usize)?;
        let memos = document.board_memos.iter().filter(|memo| memo.page_id == page.id).cloned().collect();
        Some((artifact.id.clone(), artifact.revision, page.id.clone(), memos))
    }

    fn board_memo_page_ready(&self, artifact_id: &str, revision: u64, page_id: &str, page_index: u32, page_size: Size) -> bool {
        let current = self.active.as_ref().zip(self.context.as_ref()).zip(self.artifact.as_ref())
            .is_some_and(|((active, context), artifact)| {
                active.id == artifact_id && artifact.id == artifact_id && context.artifact_id == artifact_id
                    && active.revision == revision && context.revision == revision && artifact.revision == revision
            }) && self.board_memo_page(page_index).is_some_and(|(id, seen_revision, seen_page_id, _)| {
                id == artifact_id && seen_revision == revision && seen_page_id == page_id
            });
        current && self.page_textures.get(&page_index).is_some_and(|page| {
            page.artifact_id == artifact_id && page.revision == revision && page.page.page_index == page_index
                && page.page.size == page_size && !page.snapshot_id.is_empty()
        })
    }

    /// Why the board takes no memo input now (a new memo, a memo's click or drag), or None when it does: an open memo,
    /// title or page section edit, an agent's run, an inline text edit, the hand tool, or Space held for a pan.
    fn board_memo_input_blocked(&self, space_down: bool) -> Option<&'static str> {
        if self.board_memo_editor.is_some() || self.board_memo_mutation.is_some() {
            Some("열려 있는 메모 편집이 끝난 뒤 메모를 추가하세요")
        } else if self.page_section_editor.is_some() || self.page_section_mutation.is_some() {
            Some("페이지 구역 편집이 끝난 뒤 메모를 추가하세요")
        } else if self.run_context.is_some() {
            Some("에이전트의 화면 명령이 끝난 뒤 메모를 추가하세요")
        } else if self.inline_text_edit.is_some() {
            Some("텍스트 편집을 끝낸 뒤 메모를 추가하세요")
        } else if self.view_ref().tool == Tool::Hand {
            Some("손 도구에서는 메모를 추가할 수 없습니다. 다른 도구를 고르세요")
        } else if space_down {
            Some("Space를 놓은 뒤 메모를 추가하세요")
        } else {
            None
        }
    }

    /// A new memo for the page `page_index` whose centre is at `center` (page EMU), opened for writing: the toolbox tile and
    /// its drop (68g). It takes the board's own memo conditions (`board_memo_input_blocked`); a refusal says why.
    pub(super) fn begin_board_memo_centred(&mut self, ctx: &Context, page_index: u32, center: (i64, i64)) -> Result<(), String> {
        let space_down = ctx.input(|input| input.key_down(egui::Key::Space));
        if let Some(reason) = self.board_memo_input_blocked(space_down) {
            return Err(reason.into());
        }
        let page_size = self.page_sizes().get(page_index as usize).copied().ok_or("메모를 넣을 페이지를 찾지 못했습니다")?;
        self.focus_add_page(page_index);
        let top_left = (center.0 - DEFAULT_MEMO_WIDTH / 2, center.1 - DEFAULT_MEMO_HEIGHT / 2);
        self.begin_board_memo_at(page_index, top_left, page_size);
        Ok(())
    }

    /// Opens a new memo whose top left is `top_left` (page EMU) on the page `page_index`.
    fn begin_board_memo_at(&mut self, page_index: u32, top_left: (i64, i64), page_size: Size) {
        let Some((artifact_id, revision, page_id, expected)) = self.board_memo_page(page_index) else { return };
        if !self.board_memo_page_ready(&artifact_id, revision, &page_id, page_index, page_size) {
            self.set_error("최신 페이지 그림이 준비될 때까지 메모를 만들 수 없습니다. 초안은 아직 변경하지 않았습니다".into());
            return;
        }
        if self.board_memo_editor.is_some() || self.board_memo_mutation.is_some()
            || self.page_section_editor.is_some() || self.page_section_mutation.is_some() || self.inline_text_edit.is_some()
        {
            return;
        }
        if self.memo_page_reserved(&artifact_id, &page_id, &expected) {
            self.set_error("페이지의 다른 편집이 끝날 때까지 메모를 만들 수 없습니다".into());
            return;
        }
        let (x, y) = top_left;
        let memo = CanvasBoardMemo {
            kind: CanvasBoardMemoKind::Memo,
            id: designllm_contracts::new_id(),
            page_id: page_id.clone(),
            bounds: EmuRect { x, y, width: DEFAULT_MEMO_WIDTH, height: DEFAULT_MEMO_HEIGHT },
            text: String::new(),
            color: DEFAULT_MEMO_COLOR.into(),
        };
        self.selected_board_memo = Some(memo.id.clone());
        self.board_memo_editor = Some(BoardMemoEditor {
            artifact_id,
            revision,
            page_id,
            page_index,
            expected,
            memo,
            is_new: true,
            message: None,
        });
        // The memo tool makes one memo per pick: the tool is back on select while the new memo is written.
        self.view().tool = Tool::Select;
    }

    fn open_board_memo_editor(&mut self, page_index: u32, memo_id: &str) {
        if self.board_memo_editor.is_some() || self.board_memo_mutation.is_some()
            || self.page_section_editor.is_some() || self.page_section_mutation.is_some() || self.inline_text_edit.is_some()
        { return; }
        let Some((artifact_id, revision, page_id, expected)) = self.board_memo_page(page_index) else { return };
        let Some(memo) = expected.iter().find(|memo| memo.id == memo_id).cloned() else { return };
        let Some(page_size) = self.page_sizes().get(page_index as usize).copied() else { return };
        if !self.board_memo_page_ready(&artifact_id, revision, &page_id, page_index, page_size) {
            self.set_error("최신 페이지 그림을 기다리는 동안 메모를 열지 않았습니다".into());
            return;
        }
        if self.memo_page_reserved(&artifact_id, &page_id, &expected) {
            self.set_error("페이지의 다른 편집이 진행 중입니다".into());
            return;
        }
        self.selected_board_memo = Some(memo.id.clone());
        self.board_memo_editor = Some(BoardMemoEditor {
            artifact_id,
            revision,
            page_id,
            page_index,
            expected,
            memo,
            is_new: false,
            message: None,
        });
    }

    pub(super) fn open_board_title_editor(&mut self, page_index: u32) {
        if !self.board_titles_write_supported() {
            self.set_error("이 작업물에서는 보드 제목을 편집할 수 없습니다".into());
            return;
        }
        if self.board_memo_editor.is_some() || self.board_memo_mutation.is_some()
            || self.page_section_editor.is_some() || self.page_section_mutation.is_some() || self.inline_text_edit.is_some()
        { return; }
        let Some((artifact_id, revision, page_id, expected)) = self.board_memo_page(page_index) else { return };
        if let Some(title) = expected.iter().find(|memo| memo.kind == CanvasBoardMemoKind::Title) {
            self.open_board_memo_editor(page_index, &title.id);
            return;
        }
        let Some(page_size) = self.page_sizes().get(page_index as usize).copied() else { return };
        if !self.board_memo_page_ready(&artifact_id, revision, &page_id, page_index, page_size) {
            self.set_error("최신 페이지 그림을 기다리는 동안 보드 제목을 열지 않았습니다".into());
            return;
        }
        if self.memo_page_reserved(&artifact_id, &page_id, &expected) {
            self.set_error("페이지의 다른 편집이 진행 중입니다".into());
            return;
        }
        let memo = CanvasBoardMemo {
            kind: CanvasBoardMemoKind::Title,
            id: designllm_contracts::new_id(),
            page_id: page_id.clone(),
            bounds: EmuRect {
                x: 48 * EMU_PER_CSS_PX,
                y: 28 * EMU_PER_CSS_PX,
                width: page_size.width.saturating_sub(96 * EMU_PER_CSS_PX).max(240 * EMU_PER_CSS_PX),
                height: 64 * EMU_PER_CSS_PX,
            },
            text: String::new(),
            color: DEFAULT_TITLE_COLOR.into(),
        };
        self.selected_board_memo = Some(memo.id.clone());
        self.board_memo_editor = Some(BoardMemoEditor {
            artifact_id,
            revision,
            page_id,
            page_index,
            expected,
            memo,
            is_new: true,
            message: None,
        });
    }

    fn board_memo_editor_is_current(&self, editor: &BoardMemoEditor) -> bool {
        if !self.board_memo_page_ready(
            &editor.artifact_id,
            editor.revision,
            &editor.page_id,
            editor.page_index,
            self.page_sizes().get(editor.page_index as usize).copied().unwrap_or_default(),
        ) {
            return false;
        }
        let Some((artifact_id, revision, page_id, current)) = self.board_memo_page(editor.page_index) else { return false };
        artifact_id == editor.artifact_id && revision == editor.revision && page_id == editor.page_id
            && current == editor.expected
    }

    fn memo_page_reserved(&self, artifact_id: &str, page_id: &str, memos: &[CanvasBoardMemo]) -> bool {
        if self.reservations_for.as_deref() != Some(artifact_id) { return false; }
        self.reservations.iter().flat_map(|reservation| reservation.node_ids.iter())
            .chain(self.user_reservations.iter().flat_map(|reservation| reservation.node_ids.iter()))
            .any(|id| id == page_id || memos.iter().any(|memo| &memo.id == id))
    }

    pub(super) fn save_board_memo_editor(&mut self) {
        let Some(editor) = self.board_memo_editor.clone() else { return };
        if self.run_context.is_some() {
            self.set_error("앱 요청이 끝난 뒤 메모를 저장하세요. 초안은 보존했습니다".into());
            return;
        }
        if crate::widgets::parse_hex(&editor.memo.color).is_none() {
            self.set_error("색상은 #RRGGBB 형식이어야 합니다".into());
            return;
        }
        if editor.memo.kind == CanvasBoardMemoKind::Title && editor.memo.text.trim().is_empty() {
            self.set_error("보드 제목을 입력하세요".into());
            return;
        }
        if editor.memo.bounds.width <= 0 || editor.memo.bounds.height <= 0 {
            self.set_error("메모 크기가 올바르지 않습니다".into());
            return;
        }
        if !self.board_memo_editor_is_current(&editor) || self.memo_page_reserved(&editor.artifact_id, &editor.page_id, &editor.expected) {
            if let Some(editor) = self.board_memo_editor.as_mut() {
                editor.message = Some("작업물, 페이지 또는 메모 예약이 바뀌었습니다. 최신 상태에 자동 적용하지 않습니다.".into());
            }
            return;
        }
        let Some(desired) = replace_page_memo(&editor.expected, &editor.memo, editor.is_new) else {
            if let Some(editor) = self.board_memo_editor.as_mut() { editor.message = Some("메모 대상이 더 이상 페이지에 없습니다. 초안은 보존했습니다.".into()); }
            return;
        };
        if editor.memo.page_id != editor.page_id {
            if let Some(editor) = self.board_memo_editor.as_mut() { editor.message = Some("메모의 페이지가 달라졌습니다. 초안을 보존했습니다".into()); }
            return;
        }
        let selected_memo_id = editor.memo.id.clone();
        self.submit_board_memo_change(editor, desired, Some(selected_memo_id));
    }

    fn delete_board_memo_editor(&mut self) {
        let Some(editor) = self.board_memo_editor.clone().filter(|editor| !editor.is_new) else { return };
        if self.run_context.is_some() {
            self.set_error("앱 요청이 끝난 뒤 메모를 삭제하세요. 초안은 보존했습니다".into());
            return;
        }
        if !self.board_memo_editor_is_current(&editor) || self.memo_page_reserved(&editor.artifact_id, &editor.page_id, &editor.expected) {
            if let Some(editor) = self.board_memo_editor.as_mut() {
                editor.message = Some("작업물 또는 예약이 바뀌었습니다. 메모 삭제를 보내지 않았고 초안을 보존했습니다.".into());
            }
            return;
        }
        let Some(desired) = remove_page_memo(&editor.expected, &editor.memo.id) else {
            if let Some(editor) = self.board_memo_editor.as_mut() { editor.message = Some("삭제할 메모를 찾지 못했습니다. 초안을 보존했습니다".into()); }
            return;
        };
        self.submit_board_memo_change(editor, desired, None);
    }

    fn submit_memo_move(&mut self, gesture: BoardMemoGesture, bounds: EmuRect) {
        if self.run_context.is_some() {
            self.preserve_stale_memo_move(gesture, bounds);
            return;
        }
        let artifact_id = gesture.artifact_id;
        let page_id = gesture.page_id;
        let revision = gesture.revision;
        let page_index = gesture.page_index;
        let expected = gesture.expected;
        let memo_id = gesture.memo.id.clone();
        let mut memo = gesture.memo;
        memo.bounds = bounds;
        let Some(desired) = replace_page_memo(&expected, &memo, false) else { return };
        let editor = BoardMemoEditor {
            artifact_id: artifact_id.clone(),
            revision,
            page_id: page_id.clone(),
            page_index,
            expected,
            memo,
            is_new: false,
            message: None,
        };
        let current = self.board_memo_editor_is_current(&editor)
            && !self.memo_page_reserved(&artifact_id, &page_id, &editor.expected);
        self.submit_board_memo_change_with_message(
            editor,
            desired,
            Some(memo_id),
            (!current).then(|| "작업물 또는 메모 예약이 바뀌었습니다. 이동 초안은 보존했고 최신 상태에 적용하지 않았습니다.".into()),
        );
    }

    fn preserve_stale_memo_move(&mut self, gesture: BoardMemoGesture, bounds: EmuRect) {
        let revision = gesture.revision;
        let page_index = gesture.page_index;
        let memo_id = gesture.memo.id.clone();
        let mut memo = gesture.memo;
        memo.bounds = bounds;
        let Some(desired) = replace_page_memo(&gesture.expected, &memo, false) else { return };
        let editor = BoardMemoEditor {
            artifact_id: gesture.artifact_id,
            revision,
            page_id: gesture.page_id,
            page_index,
            expected: gesture.expected,
            memo,
            is_new: false,
            message: None,
        };
        self.submit_board_memo_change_with_message(
            editor,
            desired,
            Some(memo_id),
            Some("페이지가 삭제되었거나 페이지 그림이 바뀌는 동안 이동했습니다. 이동 의도는 보존했지만 적용하지 않았습니다. 최신 화면에서 다시 움직이세요.".into()),
        );
    }

    fn submit_board_memo_change(
        &mut self,
        editor: BoardMemoEditor,
        desired: Vec<CanvasBoardMemo>,
        selected_memo_id: Option<String>,
    ) {
        self.submit_board_memo_change_with_message(editor, desired, selected_memo_id, None);
    }

    fn submit_board_memo_change_with_message(
        &mut self,
        editor: BoardMemoEditor,
        desired: Vec<CanvasBoardMemo>,
        selected_memo_id: Option<String>,
        message: Option<String>,
    ) {
        let token = designllm_contracts::new_id();
        let args = serde_json::json!({ "page_id": editor.page_id, "expected": editor.expected, "memos": desired });
        let retry_allowed = message.is_none();
        self.board_memo_editor = None;
        self.board_memo_mutation = Some(BoardMemoMutation {
            token: token.clone(),
            artifact_id: editor.artifact_id.clone(),
            revision: editor.revision,
            page_id: editor.page_id.clone(),
            page_index: editor.page_index,
            expected: editor.expected.clone(),
            desired,
            selected_memo_id,
            editor: Some(editor),
            message,
            pending: false,
            held_edit_id: None,
            retry_allowed,
            outcome_uncertain: false,
        });
        if self.run_context.is_some() {
            if let Some(mutation) = self.board_memo_mutation.as_mut().filter(|mutation| mutation.token == token) {
                mutation.message = Some("앱 요청 중에는 메모를 변경하지 않았습니다. 초안은 보존했습니다.".into());
                mutation.retry_allowed = false;
            }
            return;
        }
        if self.board_memo_mutation.as_ref().is_some_and(|mutation| mutation.message.is_some()) {
            self.set_error("메모 변경은 보존했지만 최신 상태가 달라 적용하지 않았습니다. 초안을 복사하세요".into());
            return;
        }
        // The change goes through the memo command (operation_commands.rs), which completes it as this mutation's own change.
        if self.invoke("canvas.board_memos.set", args).is_ok() {
            if let Some(mutation) = self.board_memo_mutation.as_mut().filter(|mutation| mutation.token == token) {
                mutation.pending = true;
            }
        } else {
            let held_id = self.held_edits.iter().find(|held| {
                matches!(&held.apply.after_action, PendingAction::BoardMemosChange { token: held_token } if held_token == &token)
            }).map(|held| held.id.clone());
            if let Some(mutation) = self.board_memo_mutation.as_mut().filter(|mutation| mutation.token == token) {
                mutation.held_edit_id = held_id;
                mutation.message.get_or_insert_with(|| "메모 변경 요청을 보내지 못했습니다. 초안을 보존했습니다.".into());
                mutation.retry_allowed = mutation.held_edit_id.is_none();
            }
        }
    }

    pub(super) fn fail_board_memo_change(&mut self, token: &str, error: &designllm_contracts::DltError) {
        let uncertain = matches!(
            &error.code,
            &designllm_contracts::ErrorCode::Io
                | &designllm_contracts::ErrorCode::Internal
                | &designllm_contracts::ErrorCode::HostNotRunning
                | &designllm_contracts::ErrorCode::Unavailable
                | &designllm_contracts::ErrorCode::StorageFailure
        );
        let mut refresh_current = false;
        if let Some(mutation) = self.board_memo_mutation.as_mut().filter(|mutation| mutation.token == token) {
            mutation.pending = false;
            mutation.outcome_uncertain = uncertain;
            mutation.retry_allowed = !uncertain && (error.retryable || error.code == designllm_contracts::ErrorCode::TargetBusy);
            refresh_current = self.active.as_ref().is_some_and(|active| active.id == mutation.artifact_id);
            mutation.message = Some(if uncertain {
                format!("{} 요청 결과가 불명확합니다. 이미 작업물에 반영됐을 수 있습니다. 메모 내용과 위치를 복사하고 최신 상태를 확인하세요.", super::user_error(error))
            } else {
                format!("{} 메모 초안은 보존했고 최신 상태에 자동 적용하지 않습니다.", super::user_error(error))
            });
        }
        if uncertain && refresh_current { self.refresh_active(); }
    }

    pub(super) fn finish_board_memo_change(&mut self, token: &str, value: &serde_json::Value) {
        if self.board_memo_mutation.as_ref().is_some_and(|mutation| mutation.token == token) {
            if let Some(mutation) = self.board_memo_mutation.take() {
                self.clear_applied_note_draft(&mutation);
                self.selected_board_memo = mutation.selected_memo_id.filter(|id| !id.is_empty());
            }
        }
        self.committed(value);
    }

    pub(super) fn finish_board_memo_change_offscreen(&mut self, token: &str) {
        if self.board_memo_mutation.as_ref().is_none_or(|mutation| mutation.token != token) { return; }
        let Some(mutation) = self.board_memo_mutation.take() else { return };
        self.clear_applied_note_draft(&mutation);
        self.selected_board_memo = mutation.selected_memo_id.filter(|id| !id.is_empty());
    }

    /// The change of a memo was applied: the memo's draft is done.
    fn clear_applied_note_draft(&mut self, mutation: &BoardMemoMutation) {
        if let Some(memo_id) = mutation.editor.as_ref().map(|editor| editor.memo.id.clone()) {
            self.clear_note_draft(&memo_id);
        }
    }

    /// Puts a recovered memo draft back into the memo editor, on the page it was made for.
    pub(super) fn restore_board_memo_draft(&mut self, memo: CanvasBoardMemo, page_index: u32, is_new: bool) {
        let Some((artifact_id, revision, page_id, expected)) = self.board_memo_page(page_index) else { return };
        self.selected_board_memo = Some(memo.id.clone());
        self.board_memo_editor = Some(BoardMemoEditor {
            artifact_id,
            revision,
            page_id,
            page_index,
            expected,
            memo,
            is_new,
            message: None,
        });
    }

    fn discard_board_memo_mutation(&mut self, token: &str) {
        let Some(mutation) = self.board_memo_mutation.as_ref().filter(|mutation| mutation.token == token && !mutation.pending) else { return };
        let held_id = mutation.held_edit_id.clone();
        let memo_id = mutation.editor.as_ref().map(|editor| editor.memo.id.clone());
        if let Some(held_id) = held_id {
            if self.held_edits.iter().find(|held| held.id == held_id).is_some_and(|held| held.state != super::state::HeldEditState::Sending) {
                self.held_edits.retain(|held| held.id != held_id);
                if self.held_refusal_id.as_deref() == Some(held_id.as_str()) {
                    self.held_refusal_id = None;
                    self.refusal = None;
                    self.refusal_ids.clear();
                    self.refusal_fixes.clear();
                }
                self.refresh_held_edit_notice();
            }
        }
        self.board_memo_mutation = None;
        // The discarded memo's draft goes with it, and no editor keeps showing that text (it would save the draft again).
        if let Some(memo_id) = memo_id {
            if self.board_memo_editor.as_ref().is_some_and(|editor| editor.memo.id == memo_id) {
                self.board_memo_editor = None;
            }
            self.clear_note_draft(&memo_id);
        }
    }

    fn board_memo_mutation_retry_allowed(&self, mutation: &BoardMemoMutation) -> bool {
        if mutation.pending || !mutation.retry_allowed || mutation.held_edit_id.is_some() || self.run_context.is_some() { return false; }
        let Some(page_size) = self.page_sizes().get(mutation.page_index as usize).copied() else { return false };
        if !self.board_memo_page_ready(&mutation.artifact_id, mutation.revision, &mutation.page_id, mutation.page_index, page_size) {
            return false;
        }
        let Some((artifact_id, revision, page_id, current)) = self.board_memo_page(mutation.page_index) else { return false };
        artifact_id == mutation.artifact_id && revision == mutation.revision && page_id == mutation.page_id
            && current == mutation.expected && !self.memo_page_reserved(&mutation.artifact_id, &mutation.page_id, &mutation.expected)
    }

    fn retry_board_memo_mutation(&mut self, token: &str) {
        let Some(mutation) = self.board_memo_mutation.as_ref().filter(|mutation| mutation.token == token) else { return };
        if !self.board_memo_mutation_retry_allowed(mutation) { return; }
        let args = serde_json::json!({ "page_id": mutation.page_id, "expected": mutation.expected, "memos": mutation.desired });
        if let Some(current) = self.board_memo_mutation.as_mut().filter(|current| current.token == token) {
            current.message = None;
            current.retry_allowed = false;
        }
        // The retry goes through the memo command, which completes it as this mutation's own change. `pending` is set when it is sent.
        if self.invoke("canvas.board_memos.set", args).is_ok() {
            if let Some(current) = self.board_memo_mutation.as_mut().filter(|current| current.token == token) {
                current.pending = true;
            }
        } else {
            let held_id = self.held_edits.iter().find(|held| {
                matches!(&held.apply.after_action, PendingAction::BoardMemosChange { token: held_token } if held_token == token)
            }).map(|held| held.id.clone());
            if let Some(current) = self.board_memo_mutation.as_mut().filter(|current| current.token == token) {
                current.held_edit_id = held_id;
                current.message.get_or_insert_with(|| "메모 재시도를 보내지 못했습니다. 초안을 보존했습니다.".into());
                current.retry_allowed = current.held_edit_id.is_none();
            }
        }
    }
}

fn observe_gesture_transform(
    gesture: &mut BoardMemoGesture,
    current_tool: Tool,
    space_down: bool,
    space_changed_this_frame: bool,
    current_zoom: f32,
    current_pan: Vec2,
    same_target_page: bool,
    current_page_rect: Option<Rect>,
    current_transform_id: Option<&str>,
) {
    let changed = same_target_page && (
        current_tool != gesture.tool_at_start
            || space_down != gesture.space_down_at_start
            || space_changed_this_frame
            || current_zoom.to_bits() != gesture.zoom_at_start.to_bits()
            || current_pan != gesture.pan_at_start
            || current_page_rect != Some(gesture.page_rect)
            || current_transform_id != gesture.transform_id_at_start.as_deref()
    );
    gesture.invalidated |= changed;
}

fn replace_page_memo(expected: &[CanvasBoardMemo], memo: &CanvasBoardMemo, is_new: bool) -> Option<Vec<CanvasBoardMemo>> {
    if expected.iter().any(|item| item.page_id != memo.page_id) { return None; }
    let mut desired = expected.to_vec();
    if is_new {
        if desired.iter().any(|item| item.id == memo.id) { return None; }
        desired.push(memo.clone());
    } else {
        let target = desired.iter_mut().find(|item| item.id == memo.id)?;
        *target = memo.clone();
    }
    Some(desired)
}

fn remove_page_memo(expected: &[CanvasBoardMemo], memo_id: &str) -> Option<Vec<CanvasBoardMemo>> {
    expected.iter().any(|memo| memo.id == memo_id).then(|| {
        expected.iter().filter(|memo| memo.id != memo_id).cloned().collect()
    })
}

/// Gives the keyboard back to the memo text field when nothing has it (a press on the bar or Esc dropped it), or when the
/// card first shows (`first`). A field that took the keyboard without a press (a dialog's, F2's) keeps it.
/// Whether the field was given the keyboard now.
fn give_keyboard(ctx: &Context, text_id: Id, first: bool) -> bool {
    ctx.memory_mut(|memory| {
        let give = !memory.has_focus(text_id) && (first || memory.focused().is_none());
        if give { memory.request_focus(text_id); }
        give
    })
}

/// The editor's frame-to-frame state (an open composition, the custom colour picker, the IME place) goes with the closed
/// editor.
fn forget_editing_memo(ctx: &Context, memo_id: &str) {
    ctx.data_mut(|data| {
        data.remove::<bool>(Id::new(("board-memo-composing", memo_id)));
        data.remove::<bool>(Id::new(("board-memo-custom-color", memo_id)));
        data.remove::<egui::output::IMEOutput>(Id::new(("board-memo-ime", memo_id)));
    });
}

/// What was pressed on a memo bar this frame.
struct BarActions {
    picked: Option<String>,
    delete: bool,
    custom_toggled: bool,
    copy: bool,
    discard: bool,
}

/// The floating bar beside a memo card: the colour swatches, the custom colour (`custom`: whether its picker is open,
/// `None` for no custom colour), delete, and a protection notice with the draft's copy and discard.
struct MemoBar<'a> {
    id: Id,
    card: Rect,
    board: Rect,
    is_title: bool,
    color: &'a str,
    enabled: bool,
    delete: bool,
    custom: Option<bool>,
    notice: Option<&'a str>,
}

impl MemoBar<'_> {
    fn show(self, ctx: &Context, palette: &theme::Palette) -> BarActions {
        let mut actions = BarActions { picked: None, delete: false, custom_toggled: false, copy: false, discard: false };
        let bar_size = ctx.memory(|memory| memory.area_rect(self.id)).map_or(Vec2::new(360.0, 40.0), |bar| bar.size());
        let (pivot, anchor) = bar_anchor(self.card, bar_size, self.board);
        // The Middle order of dialog windows: a dialog opened later is above the bar, not under it.
        egui::Area::new(self.id)
            .order(egui::Order::Middle)
            .pivot(pivot)
            .fixed_pos(anchor)
            .constrain_to(self.board)
            .show(ctx, |ui| {
                // tokens `popover`: surface, 1 px line, radius 10, the menu shadow; padding 4 for this small bar.
                egui::Frame::window(&ctx.style())
                    .fill(palette.surface)
                    .stroke(Stroke::new(1.0, palette.line))
                    .corner_radius(CornerRadius::same(10))
                    .inner_margin(egui::Margin::same(4))
                    .show(ui, |ui| {
                        ui.add_enabled_ui(self.enabled, |ui| {
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = 2.0;
                                let swatches: &[&str] = if self.is_title { &TITLE_COLORS } else { &MEMO_COLORS };
                                for hex in swatches {
                                    if color_swatch(ui, palette, hex, self.color.eq_ignore_ascii_case(hex)) {
                                        actions.picked = Some((*hex).into());
                                    }
                                }
                                if self.custom.is_some() && crate::widgets::labeled_button(ui, palette, None, "다른 색", false).clicked() {
                                    actions.custom_toggled = true;
                                }
                                if self.delete && crate::widgets::text_button(ui, palette, "삭제", palette.danger).clicked() {
                                    actions.delete = true;
                                }
                            });
                            if self.custom == Some(true) {
                                ui.add_space(4.0);
                                let mut current = crate::widgets::parse_hex(self.color).unwrap_or(palette.ink);
                                if egui::color_picker::color_picker_color32(ui, &mut current, egui::color_picker::Alpha::Opaque) {
                                    actions.picked = Some(hex_string(current));
                                }
                            }
                        });
                        if let Some(notice) = self.notice {
                            ui.horizontal(|ui| {
                                ui.label(egui::RichText::new(crate::i18n::tr(&notice)).font(theme::font(theme::Type::Caption)).color(palette.ink_2));
                                if crate::widgets::text_button(ui, palette, "초안 복사", palette.ink_2).clicked() { actions.copy = true; }
                                if crate::widgets::text_button(ui, palette, "초안 버리기", palette.ink_2).clicked() { actions.discard = true; }
                            });
                        }
                    });
            });
        // The bar was placed for its last size; when it grew or shrank (the picker opened or closed), the next frame
        // places it again, so it does not stay over the card.
        if ctx.memory(|memory| memory.area_rect(self.id)).is_some_and(|shown| shown.size() != bar_size) {
            ctx.request_repaint();
        }
        actions
    }
}

/// Where the colour bar goes: the first side of the card (above, below, right, left) where all of it fits on the board,
/// so it never covers the text. Only a card that fills the board leaves no side; then the bar is above, kept on the board.
fn bar_anchor(card: Rect, bar: Vec2, board: Rect) -> (Align2, Pos2) {
    let gap = Vec2::splat(BAR_GAP);
    let sides = [
        (Align2::LEFT_BOTTOM, card.left_top() - Vec2::new(0.0, gap.y)),
        (Align2::LEFT_TOP, card.left_bottom() + Vec2::new(0.0, gap.y)),
        (Align2::LEFT_TOP, card.right_top() + Vec2::new(gap.x, 0.0)),
        (Align2::RIGHT_TOP, card.left_top() - Vec2::new(gap.x, 0.0)),
    ];
    let fits = |(pivot, anchor): &(Align2, Pos2)| {
        let rect = pivot.anchor_size(*anchor, bar);
        // Kept on the board, the bar may slide sideways (or up and down beside the card) but not onto the card.
        let slid = Rect::from_min_size(rect.min.clamp(board.min, (board.max - bar).max(board.min)), bar);
        board.contains_rect(slid) && !slid.intersects(card)
    };
    sides.into_iter().find(fits).unwrap_or(sides[0])
}

/// One colour swatch: 16 px chip in a 28 px hit area (§5.4 and §6 touch size); selected = 1.5 px accent ring.
fn color_swatch(ui: &mut Ui, palette: &theme::Palette, hex: &str, selected: bool) -> bool {
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(28.0), Sense::click());
    let chip = Rect::from_center_size(rect.center(), Vec2::splat(16.0));
    let fill = crate::widgets::parse_hex(hex).unwrap_or(palette.line);
    ui.painter().rect_filled(chip, CornerRadius::same(4), fill);
    ui.painter().rect_stroke(chip, CornerRadius::same(4), Stroke::new(1.0, palette.line_strong), egui::StrokeKind::Inside);
    if selected {
        ui.painter().rect_stroke(rect.shrink(2.0), CornerRadius::same(6), Stroke::new(1.5, palette.accent), egui::StrokeKind::Inside);
    } else if response.hovered() {
        ui.painter().rect_stroke(rect.shrink(2.0), CornerRadius::same(6), Stroke::new(1.0, palette.accent), egui::StrokeKind::Inside);
    }
    response.clicked()
}

fn hex_string(color: egui::Color32) -> String {
    format!("#{:02X}{:02X}{:02X}", color.r(), color.g(), color.b())
}

/// The text field of the memo or title editor, fixed per memo so that tests can give it the keyboard.
pub(super) fn memo_text_edit_id(artifact_id: &str, memo_id: &str) -> Id {
    Id::new(("board-memo-text", artifact_id, memo_id))
}

fn memo_copy_payload(editor: &BoardMemoEditor) -> String {
    memo_copy_payload_parts(&editor.artifact_id, &editor.page_id, editor.page_index, &editor.memo)
}

fn memo_copy_payload_parts(artifact_id: &str, page_id: &str, page_index: u32, memo: &CanvasBoardMemo) -> String {
    let kind = if memo.kind == CanvasBoardMemoKind::Title { "보드 제목" } else { "보드 메모" };
    format!(
        "{}\n\n종류: {}\n작업물 ID: {}\n페이지 {}\n페이지 ID: {}\n위치 EMU: {}, {}\n크기 EMU: {} × {}\n색상: {}",
        memo.text,
        kind,
        artifact_id,
        page_index + 1,
        page_id,
        memo.bounds.x,
        memo.bounds.y,
        memo.bounds.width,
        memo.bounds.height,
        memo.color,
    )
}

fn moved_bounds(bounds: EmuRect, delta: Vec2, page_rect: Rect, page_size: Size) -> EmuRect {
    let dx = f64::from(delta.x) / f64::from(page_rect.width()) * page_size.width as f64;
    let dy = f64::from(delta.y) / f64::from(page_rect.height()) * page_size.height as f64;
    EmuRect { x: bounds.x.saturating_add(round_i64(dx)), y: bounds.y.saturating_add(round_i64(dy)), ..bounds }
}

fn round_i64(value: f64) -> i64 {
    if value >= i64::MAX as f64 { i64::MAX }
    else if value <= i64::MIN as f64 { i64::MIN }
    else { value.round() as i64 }
}

fn truncate(text: &str, max_chars: usize) -> String {
    let mut chars = text.chars();
    let prefix: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() { format!("{prefix}…") } else { prefix }
}

#[cfg(test)]
#[path = "board_memo_tests.rs"]
mod tests;
