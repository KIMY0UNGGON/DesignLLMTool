//! The board memo and title editor on its card, driven through `App::update`: typing and line breaks at any zoom, IME
//! composition, the colour bar, Enter, Esc and outside presses, and the protection of a draft whose page changed.

use designllm_contracts::{CanvasBoardMemo, CanvasBoardMemoKind};
use eframe::egui::{epaint::Shape, epaint::TextShape, FullOutput, Id, ImeEvent, Key, OutputCommand};

use super::*;

const MEMO_ID: &str = "m1";

fn memo(kind: CanvasBoardMemoKind, text: &str) -> CanvasBoardMemo {
    let is_title = kind == CanvasBoardMemoKind::Title;
    CanvasBoardMemo {
        kind,
        id: MEMO_ID.into(),
        page_id: "page-1".into(),
        bounds: if is_title {
            designllm_contracts::Rect { x: 48 * 9525, y: 28 * 9525, width: 1184 * 9525, height: 64 * 9525 }
        } else {
            designllm_contracts::Rect { x: 400 * 9525, y: 200 * 9525, width: 240 * 9525, height: 140 * 9525 }
        },
        text: text.into(),
        color: if is_title { "#20252D".into() } else { "#FFF2A8".into() },
    }
}

fn key(key: Key) -> Event {
    Event::Key { key, physical_key: Some(key), pressed: true, repeat: false, modifiers: Modifiers::NONE }
}

fn button(position: Pos2, button: PointerButton, pressed: bool) -> Event {
    Event::PointerButton { pos: position, button, pressed, modifiers: Modifiers::NONE }
}

fn preedit(text: &str) -> Event {
    Event::Ime(ImeEvent::Preedit(text.into()))
}

fn commit(text: &str) -> Event {
    Event::Ime(ImeEvent::Commit(text.into()))
}

/// The canvas fixture of `inline_text_harness` (a selected text node on page 1) with `memos` on its page, drawn through
/// the window's own `update`, at a clock that keeps clicks in separate frames from being a double click.
struct Board {
    harness: Harness,
    clock: f64,
}

impl Board {
    fn new(memos: Vec<CanvasBoardMemo>) -> Self {
        let mut harness = inline_text_harness();
        if let Some(ArtifactBody::Canvas(document)) = harness.app.artifact.as_mut().map(|artifact| &mut artifact.body) {
            document.board_memos = memos;
        }
        let mut board = Self { harness, clock: 0.0 };
        board.frame(Vec::new());
        let _ = board.harness.sent();
        board
    }

    fn frame(&mut self, events: Vec<Event>) -> FullOutput {
        self.clock += 0.5;
        let ctx = self.harness.ctx.clone();
        let mut input = screen(1.0, 1280.0, 800.0);
        input.time = Some(self.clock);
        input.events = events;
        eframe::App::raw_input_hook(&mut self.harness.app, &ctx, &mut input);
        let mut frame = eframe::Frame::_new_kittest();
        ctx.run(input, |ctx| eframe::App::update(&mut self.harness.app, ctx, &mut frame))
    }

    /// Sets the board zoom (the fit already ran) and draws it.
    fn zoom(&mut self, zoom: f32) {
        let view = self.harness.app.view();
        view.zoom = zoom;
        view.fitted = true;
        self.frame(Vec::new());
        self.frame(Vec::new());
        let _ = self.harness.sent();
    }

    /// Opens the editor on `memo` with the state `begin_board_memo_at` (a new memo) or `open_board_memo_editor` sets.
    fn open(&mut self, memo: CanvasBoardMemo, is_new: bool) {
        let expected = self.page_memos();
        self.harness.app.selected_board_memo = Some(memo.id.clone());
        self.harness.app.board_memo_editor = Some(super::super::BoardMemoEditor {
            artifact_id: "a1".into(),
            revision: 1,
            page_id: "page-1".into(),
            page_index: 0,
            expected,
            memo,
            is_new,
            message: None,
        });
        self.frame(Vec::new());
        let _ = self.harness.sent();
    }

    fn page_memos(&self) -> Vec<CanvasBoardMemo> {
        match self.harness.app.artifact.as_ref().map(|artifact| &artifact.body) {
            Some(ArtifactBody::Canvas(document)) => document.board_memos.clone(),
            _ => Vec::new(),
        }
    }

    fn type_text(&mut self, text: &str) -> FullOutput {
        self.frame(vec![Event::Text(text.into())])
    }

    fn press_key(&mut self, pressed: Key) -> FullOutput {
        self.frame(vec![key(pressed)])
    }

    fn click(&mut self, position: Pos2) -> FullOutput {
        self.frame(vec![
            Event::PointerMoved(position),
            button(position, PointerButton::Primary, true),
            button(position, PointerButton::Primary, false),
        ])
    }

    fn text(&self) -> Option<String> {
        self.harness.app.board_memo_editor.as_ref().map(|editor| editor.memo.text.clone())
    }

    fn color(&self) -> Option<String> {
        self.harness.app.board_memo_editor.as_ref().map(|editor| editor.memo.color.clone())
    }

    fn field_focused(&self) -> bool {
        let memo_id = self.harness.app.board_memo_editor.as_ref().map_or(MEMO_ID, |editor| editor.memo.id.as_str());
        let id = super::super::board_memos::memo_text_edit_id("a1", memo_id);
        self.harness.ctx.memory(|memory| memory.has_focus(id))
    }

    fn area(&self, name: &str) -> Rect {
        self.harness.ctx.memory(|memory| memory.area_rect(Id::new((name, "a1", MEMO_ID)))).unwrap_or_else(|| panic!("{name} is drawn"))
    }

    fn board_rect(&self) -> Rect {
        self.harness.app.board_rect.expect("the board was drawn")
    }

    /// A point on the board away from the card and its bar.
    fn outside(&self) -> Pos2 {
        let board = self.board_rect();
        let point = Pos2::new(board.right() - 24.0, board.bottom() - 24.0);
        assert!(!self.area("board-memo-card").contains(point) && !self.area("board-memo-bar").contains(point));
        point
    }

    /// The memo lists the window sent: the memo command reaches the core as a `changes.apply` of `set_canvas_board_memos`.
    fn memo_changes(&mut self) -> Vec<Value> {
        fn operation(value: &Value) -> Option<&Value> {
            match value {
                Value::Object(map) if map.contains_key("memos") && map.contains_key("expected") => Some(value),
                Value::Object(map) => map.values().find_map(operation),
                Value::Array(items) => items.iter().find_map(operation),
                _ => None,
            }
        }
        self.harness.sent().into_iter()
            .filter(|request| request.1 == "changes.apply")
            .filter_map(|request| operation(&request.2).cloned())
            .collect()
    }
}

/// Every text shape of a frame with the clip rectangle it is painted in.
fn texts(output: &FullOutput) -> Vec<(TextShape, Rect)> {
    fn walk(shape: &Shape, clip: Rect, found: &mut Vec<(TextShape, Rect)>) {
        match shape {
            Shape::Text(text) => found.push((text.clone(), clip)),
            Shape::Vec(shapes) => shapes.iter().for_each(|shape| walk(shape, clip, found)),
            _ => {}
        }
    }
    let mut found = Vec::new();
    for clipped in &output.shapes {
        walk(&clipped.shape, clipped.clip_rect, &mut found);
    }
    found
}

/// Where row `row` of the text `text` was painted on screen, and the clip rectangle it was painted in.
fn painted_row(output: &FullOutput, text: &str, row: usize) -> Option<(Rect, Rect)> {
    texts(output).into_iter().find(|(shape, _)| shape.galley.text() == text).and_then(|(shape, clip)| {
        shape.galley.rows.get(row).map(|placed| (placed.rect().translate(shape.pos.to_vec2()), clip))
    })
}

/// The middle of the colour swatch `hex` painted in the frame (its chip is the only rectangle filled with that colour).
fn swatch_center(output: &FullOutput, hex: &str) -> Pos2 {
    let fill = crate::widgets::parse_hex(hex).expect("a swatch colour");
    output.shapes.iter().find_map(|clipped| match &clipped.shape {
        Shape::Rect(rect) if rect.fill == fill && rect.rect.width() < 20.0 => Some(rect.rect.center()),
        _ => None,
    }).unwrap_or_else(|| panic!("swatch {hex} is painted"))
}

/// The middle of a label (a button's text) painted in the frame.
fn label_center(output: &FullOutput, label: &str) -> Pos2 {
    texts(output).into_iter().find(|(shape, _)| shape.galley.text() == label)
        .map(|(shape, _)| shape.galley.rect.translate(shape.pos.to_vec2()).center())
        .unwrap_or_else(|| panic!("{label} is painted"))
}

/// The row is painted and none of it is cut away: inside its clip rectangle and inside the board. The caret the IME is
/// told about is on the board too.
fn assert_row_visible(board: &Board, output: &FullOutput, text: &str, row: usize, context: &str) {
    let (rect, clip) = painted_row(output, text, row).unwrap_or_else(|| panic!("{context}: row {row} of {text:?} is painted"));
    let board_rect = board.board_rect();
    assert!(clip.contains_rect(rect), "{context}: row {row} {rect:?} is cut by the clip {clip:?}");
    assert!(board_rect.contains_rect(rect), "{context}: row {row} {rect:?} is outside the board {board_rect:?}");
    // The caret is a row height tall, a little taller than the row's glyphs: its middle is in view.
    let caret = output.platform_output.ime.as_ref().map(|ime| ime.cursor_rect).unwrap_or_else(|| panic!("{context}: the field tells the IME its caret"));
    assert!(clip.contains(caret.center()) && board_rect.contains(caret.center()), "{context}: the caret {caret:?} is in view");
}

#[test]
fn memo_second_line_after_enter_is_drawn_where_it_can_be_seen_at_every_zoom() {
    for zoom in [1.0, 0.4, 0.25] {
        let mut board = Board::new(Vec::new());
        board.zoom(zoom);
        let context = format!("zoom {zoom} (board zoom {})", board.harness.app.view_ref().zoom);
        board.open(memo(CanvasBoardMemoKind::Memo, ""), true);
        assert!(board.field_focused(), "{context}: the new memo takes the keyboard");
        board.type_text("ab");
        board.press_key(Key::Enter);
        assert!(board.field_focused(), "{context}: Enter keeps the keyboard in the memo");
        let output = board.type_text("cd");
        assert_eq!(board.text().as_deref(), Some("ab\ncd"), "{context}: the second line takes typed text");
        assert_row_visible(&board, &output, "ab\ncd", 0, &context);
        assert_row_visible(&board, &output, "ab\ncd", 1, &context);
        assert!(board.harness.app.inline_text_edit.is_none(), "{context}: Enter does not start editing the selected text node");
    }
}

#[test]
fn a_memo_longer_than_its_card_and_the_board_keeps_its_last_line_and_caret_in_view() {
    for zoom in [1.0, 0.25] {
        let mut board = Board::new(Vec::new());
        board.zoom(zoom);
        board.open(memo(CanvasBoardMemoKind::Memo, ""), true);
        board.type_text("line 0");
        for line in 1..60 {
            board.press_key(Key::Enter);
            board.type_text(&format!("line {line}"));
        }
        // The field asks its scroll area to show the caret; the scroll is applied when the window repaints.
        let output = board.frame(Vec::new());
        let text = board.text().expect("the editor is open");
        assert_eq!(text.lines().count(), 60);
        assert_row_visible(&board, &output, &text, 59, &format!("zoom {zoom}"));
        let card = board.area("board-memo-card");
        assert!(board.board_rect().contains_rect(card), "zoom {zoom}: the grown card {card:?} stays on the board");
    }
}

#[test]
fn korean_on_the_first_and_the_second_line_goes_into_the_memo_once() {
    let mut board = Board::new(Vec::new());
    board.zoom(0.25);
    board.open(memo(CanvasBoardMemoKind::Memo, ""), true);
    board.frame(vec![Event::Ime(ImeEvent::Enabled), preedit("ㅎ")]);
    board.frame(vec![commit("하")]);
    assert_eq!(board.text().as_deref(), Some("하"));
    board.press_key(Key::Enter);
    assert_eq!(board.text().as_deref(), Some("하\n"), "Enter after a commit starts a second line and keeps the editor open");
    board.frame(vec![preedit("ㅎ")]);
    let output = board.frame(vec![commit("하")]);
    assert_eq!(board.text().as_deref(), Some("하\n하"), "the second line takes the Korean commit once");
    assert_row_visible(&board, &output, "하\n하", 1, "zoom 0.25");
}

#[test]
fn korean_commit_goes_into_the_memo_and_the_title() {
    for kind in [CanvasBoardMemoKind::Memo, CanvasBoardMemoKind::Title] {
        let mut board = Board::new(Vec::new());
        board.open(memo(kind, ""), true);
        board.frame(vec![Event::Ime(ImeEvent::Enabled), preedit("ㅎ")]);
        board.frame(vec![commit("하")]);
        assert_eq!(board.text().as_deref(), Some("하"), "{kind:?}: the committed syllable is in the text, once");
        assert!(board.harness.app.board_memo_editor.is_some(), "{kind:?}: a commit does not close the editor");
    }
}

#[test]
fn a_paragraph_text_target_does_not_take_the_memo_commit() {
    let mut board = Board::new(Vec::new());
    board.open(memo(CanvasBoardMemoKind::Memo, ""), true);
    // A canvas text edit left a paragraph target behind: the document adapter must not take the memo's IME.
    board.harness.app.text_target = Some(super::super::state::TextTarget::Paragraph {
        paragraph_id: "text-1".into(),
        original: "A".into(),
        editable: true,
    });
    board.harness.app.text_draft = "A".into();
    board.frame(vec![Event::Ime(ImeEvent::Enabled), preedit("ㅎ")]);
    board.frame(vec![commit("하")]);
    assert_eq!(board.text().as_deref(), Some("하"), "the memo receives the commit");
    assert_eq!(board.harness.app.text_draft, "A", "the paragraph draft is not changed by the memo's commit");
}

#[test]
fn enter_during_a_composition_does_not_save_or_close_the_memo_or_title() {
    for kind in [CanvasBoardMemoKind::Memo, CanvasBoardMemoKind::Title] {
        let mut board = Board::new(Vec::new());
        board.open(memo(kind, ""), true);
        board.frame(vec![Event::Ime(ImeEvent::Enabled), preedit("하")]);
        board.press_key(Key::Enter);
        assert!(board.harness.app.board_memo_editor.is_some(), "{kind:?}: Enter during the composition keeps the editor open");
        assert!(board.field_focused(), "{kind:?}: the field keeps the keyboard");
        assert!(board.memo_changes().is_empty(), "{kind:?}: Enter during the composition does not save");
    }
}

#[test]
fn an_enter_that_commits_a_composition_breaks_the_memo_line_and_confirms_the_title() {
    let mut board = Board::new(Vec::new());
    board.open(memo(CanvasBoardMemoKind::Memo, ""), true);
    board.frame(vec![Event::Ime(ImeEvent::Enabled), preedit("하")]);
    // The IME commits the syllable, then passes the Enter on.
    board.frame(vec![commit("하"), key(Key::Enter)]);
    board.type_text("x");
    assert_eq!(board.text().as_deref(), Some("하\nx"), "the memo keeps the syllable and breaks the line");

    let mut board = Board::new(Vec::new());
    board.open(memo(CanvasBoardMemoKind::Title, ""), true);
    board.frame(vec![Event::Ime(ImeEvent::Enabled), preedit("하")]);
    board.frame(vec![commit("하"), key(Key::Enter)]);
    assert!(board.harness.app.board_memo_editor.is_none(), "the title is confirmed");
    let changes = board.memo_changes();
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0]["memos"][0]["text"], "하", "with the committed syllable");
}

#[test]
fn escape_during_a_composition_that_began_frames_earlier_belongs_to_the_ime() {
    for kind in [CanvasBoardMemoKind::Memo, CanvasBoardMemoKind::Title] {
        let mut board = Board::new(Vec::new());
        board.open(memo(kind, ""), true);
        board.frame(vec![Event::Ime(ImeEvent::Enabled), preedit("하")]);
        // Esc comes frames after the composition began, without a preedit of its own.
        board.press_key(Key::Escape);
        assert!(board.harness.app.board_memo_editor.is_some(), "{kind:?}: Esc during the composition keeps the editor open");
        assert!(board.field_focused(), "{kind:?}: the field keeps the keyboard through the IME's Esc");
        assert!(board.memo_changes().is_empty(), "{kind:?}: nothing is saved during the composition");
        board.frame(vec![commit("하")]);
        assert_eq!(board.text().as_deref(), Some("하"), "{kind:?}: the composition is committed into the field");
        board.press_key(Key::Escape);
        assert!(board.harness.app.board_memo_editor.is_none(), "{kind:?}: Esc after the composition confirms");
        assert_eq!(board.memo_changes().len(), 1, "{kind:?}: the confirmed text is saved");
    }
}

#[test]
fn enter_confirms_a_board_title() {
    let mut board = Board::new(Vec::new());
    board.open(memo(CanvasBoardMemoKind::Title, ""), true);
    board.type_text("Q4 plan");
    board.press_key(Key::Enter);
    assert!(board.harness.app.board_memo_editor.is_none(), "Enter confirms the title");
    let changes = board.memo_changes();
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0]["memos"][0]["text"], "Q4 plan");
    assert_eq!(changes[0]["memos"][0]["kind"], "title");
}

#[test]
fn swatches_and_the_custom_colour_keep_the_keyboard_in_the_memo() {
    let mut board = Board::new(Vec::new());
    board.zoom(0.25);
    board.open(memo(CanvasBoardMemoKind::Memo, ""), true);
    let typed = board.type_text("ab");
    board.click(swatch_center(&typed, "#D9F99D"));
    assert_eq!(board.color().as_deref(), Some("#D9F99D"), "the swatch sets the memo colour");
    assert!(board.field_focused(), "the field has the keyboard again after the swatch");
    board.press_key(Key::Enter);
    board.type_text("cd");
    assert_eq!(board.text().as_deref(), Some("ab\ncd"), "Enter and typing after the swatch go to the memo");
    assert!(board.harness.app.inline_text_edit.is_none(), "Enter does not start editing the selected text node");

    let output = board.frame(Vec::new());
    board.click(label_center(&output, "다른 색"));
    board.frame(Vec::new());
    // `r` is the rectangle tool's shortcut: it must reach the memo.
    let output = board.type_text("r");
    assert_eq!(board.text().as_deref(), Some("ab\ncdr"), "typing after opening the custom colour goes to the memo");
    assert!(board.harness.sent().iter().all(|request| request.1 != "changes.apply"), "the letter does not add a rectangle");
    let bar = board.area("board-memo-bar");
    let card = board.area("board-memo-card");
    assert!(!bar.intersects(card), "the opened picker {bar:?} does not cover the card {card:?}");
    assert!(board.board_rect().expand(0.5).contains_rect(bar), "the bar {bar:?} is on the board");
    assert_row_visible(&board, &output, "ab\ncdr", 1, "after the picker");
}

#[test]
fn the_colour_bar_stays_on_the_board_and_off_the_card() {
    // A memo at the page's top-left corner (the page is near the board's top at the fit) and one at its bottom-right.
    for (x, y) in [(0, 0), (1040, 580)] {
        let mut board = Board::new(Vec::new());
        let mut corner = memo(CanvasBoardMemoKind::Memo, "");
        corner.bounds.x = x * 9525;
        corner.bounds.y = y * 9525;
        board.open(corner, true);
        let output = board.frame(Vec::new());
        board.click(label_center(&output, "다른 색"));
        // The picker opens on the next frame; the frame after it places the grown bar (the editor asks for that repaint).
        board.frame(Vec::new());
        board.frame(Vec::new());
        let bar = board.area("board-memo-bar");
        let card = board.area("board-memo-card");
        assert!(board.board_rect().expand(0.5).contains_rect(bar), "memo at ({x}, {y}): the bar {bar:?} is on the board");
        assert!(!bar.intersects(card), "memo at ({x}, {y}): the bar {bar:?} does not cover the card {card:?}");
    }
}

#[test]
fn only_a_primary_press_outside_confirms() {
    let mut board = Board::new(Vec::new());
    board.open(memo(CanvasBoardMemoKind::Memo, ""), true);
    board.type_text("ab");
    let outside = board.outside();
    for pressed in [PointerButton::Middle, PointerButton::Secondary] {
        board.frame(vec![Event::PointerMoved(outside), button(outside, pressed, true)]);
        board.frame(vec![button(outside, pressed, false)]);
        assert!(board.harness.app.board_memo_editor.is_some(), "a {pressed:?} press outside keeps the editor open");
        assert!(board.field_focused(), "a {pressed:?} press outside leaves the keyboard in the memo");
    }
    board.type_text("c");
    assert_eq!(board.text().as_deref(), Some("abc"));
    let inside = board.area("board-memo-card").center();
    board.click(inside);
    assert!(board.harness.app.board_memo_editor.is_some(), "a press on the card keeps editing");
    board.frame(vec![Event::PointerMoved(outside), button(outside, PointerButton::Primary, true)]);
    assert!(board.harness.app.board_memo_editor.is_none(), "a primary press outside confirms");
    let changes = board.memo_changes();
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0]["memos"][0]["text"], "abc");
}

#[test]
fn escape_drops_an_empty_new_memo_and_saves_typed_text() {
    let mut board = Board::new(Vec::new());
    board.open(memo(CanvasBoardMemoKind::Memo, ""), true);
    board.press_key(Key::Escape);
    assert!(board.harness.app.board_memo_editor.is_none(), "Esc on an empty new memo drops it");
    assert!(board.memo_changes().is_empty(), "an empty memo is not sent");
    assert_eq!(board.harness.app.selected_board_memo, None);

    let mut board = Board::new(Vec::new());
    board.open(memo(CanvasBoardMemoKind::Memo, ""), true);
    board.type_text("ab");
    board.press_key(Key::Escape);
    assert!(board.harness.app.board_memo_editor.is_none() && board.harness.app.board_memo_mutation.is_some(), "Esc after typing saves");
    assert_eq!(board.memo_changes().len(), 1);
}

#[test]
fn a_second_click_on_a_selected_memo_edits_it_in_place_and_delete_removes_it() {
    let mut board = Board::new(vec![memo(CanvasBoardMemoKind::Memo, "old")]);
    board.frame(Vec::new());
    let card = super::super::page_to_screen(memo(CanvasBoardMemoKind::Memo, "").bounds, Size::slide(), page_rect(&board));
    board.click(card.center());
    assert_eq!(board.harness.app.selected_board_memo.as_deref(), Some(MEMO_ID), "the first click selects the memo");
    assert!(board.harness.app.board_memo_editor.is_none(), "the first click does not edit");
    board.click(card.center());
    let editor = board.harness.app.board_memo_editor.as_ref().expect("the second click edits the memo");
    assert!(!editor.is_new && editor.memo.text == "old");
    board.frame(Vec::new());
    assert!(board.field_focused(), "the memo text field takes the keyboard");
    board.type_text("!");
    assert_eq!(board.text().as_deref(), Some("old!"));

    let output = board.frame(Vec::new());
    board.click(label_center(&output, "삭제"));
    assert!(board.harness.app.board_memo_editor.is_none(), "delete closes the editor");
    let changes = board.memo_changes();
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0]["memos"], json!([]), "the memo is removed from its page");
    assert_eq!(changes[0]["expected"][0]["text"], "old");
}

/// The screen rectangle of page 1, from where the board drew the fixture's text node.
fn page_rect(board: &Board) -> Rect {
    let view = board.harness.app.view_ref();
    let slots = crate::board::arrange(ArtifactKind::Canvas, &board.harness.app.page_sizes(), view.zoom);
    crate::board::to_screen(slots[0].world, board.board_rect().min + view.pan, view.zoom)
}

#[test]
fn a_draft_whose_page_memos_changed_keeps_its_copy_discard_and_escape_routes() {
    let changed = |board: &mut Board| {
        let mut other = memo(CanvasBoardMemoKind::Memo, "someone else's");
        other.id = "m2".into();
        if let Some(ArtifactBody::Canvas(document)) = board.harness.app.artifact.as_mut().map(|artifact| &mut artifact.body) {
            document.board_memos.push(other);
        }
    };

    let mut board = Board::new(Vec::new());
    board.open(memo(CanvasBoardMemoKind::Memo, ""), true);
    board.type_text("draft");
    changed(&mut board);
    board.frame(Vec::new());
    let output = board.frame(Vec::new());
    assert!(board.harness.app.board_memo_editor.as_ref().is_some_and(|editor| editor.message.is_some()), "the draft gets the notice");
    board.click(label_center(&output, "초안 버리기"));
    assert!(board.harness.app.board_memo_editor.is_none(), "discarding closes the editor");
    assert!(board.harness.app.board_memo_mutation.is_none() && board.memo_changes().is_empty(), "nothing is sent");

    let mut board = Board::new(Vec::new());
    board.open(memo(CanvasBoardMemoKind::Memo, ""), true);
    board.type_text("draft");
    changed(&mut board);
    let output = board.frame(Vec::new());
    let copied = output.platform_output.commands.iter().any(|command| matches!(command, OutputCommand::CopyText(_)));
    assert!(!copied, "nothing is copied before the button");
    let output = board.click(label_center(&output, "초안 복사"));
    let copied = output.platform_output.commands.iter().find_map(|command| match command {
        OutputCommand::CopyText(text) => Some(text.clone()),
        _ => None,
    });
    assert!(copied.is_some_and(|text| text.starts_with("draft")), "the draft is copied with its place");
    board.press_key(Key::Escape);
    assert!(board.harness.app.board_memo_editor.is_none(), "Esc closes the card of a stale draft");
    let mutation = board.harness.app.board_memo_mutation.as_ref().expect("the draft is kept in the memo notice");
    assert!(mutation.message.is_some() && !mutation.pending, "the notice explains it and nothing is pending");
    assert_eq!(mutation.editor.as_ref().map(|editor| editor.memo.text.as_str()), Some("draft"));
    assert!(board.memo_changes().is_empty(), "a stale draft is not sent");
}

#[test]
fn the_editor_shows_and_takes_escape_while_its_page_picture_is_not_ready() {
    let mut board = Board::new(Vec::new());
    board.open(memo(CanvasBoardMemoKind::Memo, ""), true);
    board.type_text("ab");
    board.harness.app.page_textures.clear();
    board.frame(Vec::new());
    let output = board.frame(Vec::new());
    assert!(texts(&output).iter().any(|(shape, _)| shape.galley.text() == "ab"), "the draft is still drawn on its card");
    assert!(texts(&output).iter().any(|(shape, _)| shape.galley.text() == "초안 버리기"), "the notice offers the routes out");
    board.press_key(Key::Escape);
    assert!(board.harness.app.board_memo_editor.is_none(), "Esc closes the editor");
    assert!(board.harness.app.board_memo_mutation.as_ref().is_some_and(|mutation| mutation.editor.is_some()), "the draft is kept");
    assert!(board.memo_changes().is_empty());
}

#[test]
fn the_memo_tool_click_opens_a_focused_card() {
    let mut board = Board::new(Vec::new());
    board.harness.app.selected_node = None;
    board.harness.app.selected_nodes.clear();
    board.press_key(Key::N);
    assert_eq!(board.harness.app.view_ref().tool, super::super::board::Tool::Memo, "N picks the memo tool");
    let page = page_rect(&board);
    board.click(page.center());
    assert!(board.harness.app.board_memo_editor.as_ref().is_some_and(|editor| editor.is_new), "the click makes a new memo");
    board.frame(Vec::new());
    assert!(board.field_focused(), "the new card has the keyboard");
    board.type_text("hi");
    board.press_key(Key::Enter);
    board.type_text("there");
    assert_eq!(board.text().as_deref(), Some("hi\nthere"));
}

#[test]
fn the_memo_tool_makes_one_memo_then_picks_select_and_v_or_esc_leaves_it() {
    let mut board = Board::new(Vec::new());
    board.harness.app.selected_node = None;
    board.harness.app.selected_nodes.clear();
    board.press_key(Key::N);
    let page = page_rect(&board);
    board.click(page.center());
    assert!(board.harness.app.board_memo_editor.as_ref().is_some_and(|editor| editor.is_new), "the click makes a new memo");
    assert_eq!(board.harness.app.view_ref().tool, super::super::board::Tool::Select, "the memo tool makes one memo, then the tool is select");
    board.type_text("hi");
    board.press_key(Key::Escape);
    assert_eq!(board.memo_changes().len(), 1, "the one memo is saved");
    let token = board.harness.app.board_memo_mutation.as_ref().map(|mutation| mutation.token.clone()).expect("the save is in flight");
    board.harness.app.finish_board_memo_change_offscreen(&token);

    board.frame(Vec::new());
    board.click(page.center() - eframe::egui::Vec2::new(60.0, 60.0));
    assert!(board.harness.app.board_memo_editor.is_none(), "the next click on the page makes no memo");
    assert!(board.memo_changes().is_empty(), "and sends no memo change");

    board.press_key(Key::N);
    assert_eq!(board.harness.app.view_ref().tool, super::super::board::Tool::Memo, "N picks the memo tool again");
    board.press_key(Key::V);
    assert_eq!(board.harness.app.view_ref().tool, super::super::board::Tool::Select, "V picks the select tool");

    let mut board = Board::new(Vec::new());
    board.press_key(Key::N);
    board.press_key(Key::Escape);
    assert_eq!(board.harness.app.view_ref().tool, super::super::board::Tool::Select, "Esc leaves the memo tool when no memo is made");
    assert!(board.harness.app.board_memo_editor.is_none());
}

#[test]
fn a_board_title_or_memo_editor_does_not_open_while_an_inline_text_edit_is_open() {
    let mut board = Board::new(Vec::new());
    board.press_key(Key::Enter);
    assert!(board.harness.app.inline_text_edit.is_some(), "Enter on the selected text node starts an inline edit");
    let _ = board.harness.sent();
    board.harness.app.open_board_title_editor(0);
    assert!(board.harness.app.board_memo_editor.is_none(), "the title editor waits until the inline edit is finished");
    assert!(board.harness.app.inline_text_edit.is_some(), "the inline edit keeps the keyboard");
}

/// A press at `position` only (the release comes in a later frame, if at all).
fn press_at(board: &mut Board, position: Pos2) -> FullOutput {
    board.frame(vec![Event::PointerMoved(position), button(position, PointerButton::Primary, true)])
}

#[test]
fn presses_on_the_close_dialog_or_a_notice_do_not_save_the_memo() {
    let mut board = Board::new(Vec::new());
    board.open(memo(CanvasBoardMemoKind::Memo, ""), true);
    board.type_text("draft");
    board.harness.app.close_confirmation_open = true;
    board.frame(Vec::new());
    let output = board.frame(Vec::new());
    board.click(label_center(&output, "계속 작업"));
    assert!(!board.harness.app.close_confirmation_open, "the dialog's button did its own work");
    assert!(board.harness.app.board_memo_editor.is_some(), "a press on the dialog is not a press outside the memo");
    assert!(board.memo_changes().is_empty(), "the memo is not saved by the dialog's button");

    board.harness.app.close_confirmation_open = true;
    board.frame(Vec::new());
    let output = board.frame(Vec::new());
    let discard = board.harness.app.close_discard_label(false);
    press_at(&mut board, label_center(&output, discard));
    assert!(board.memo_changes().is_empty(), "closing without the draft does not save it first");

    let mut board = Board::new(Vec::new());
    board.open(memo(CanvasBoardMemoKind::Memo, ""), true);
    board.type_text("draft");
    board.harness.app.set_error("알림 한 줄".into());
    board.frame(Vec::new());
    let output = board.frame(Vec::new());
    let notice = texts(&output).into_iter().find(|(shape, _)| shape.galley.text().contains("알림 한 줄"))
        .map(|(shape, _)| shape.galley.rect.translate(shape.pos.to_vec2()).center()).expect("the notice is painted");
    press_at(&mut board, notice);
    assert!(board.harness.app.board_memo_editor.is_some(), "a press on a notice is not a press outside the memo");
    assert!(board.memo_changes().is_empty());
}

#[test]
fn a_refused_confirm_closes_the_card_with_the_memo_as_it_was() {
    let mut board = Board::new(vec![memo(CanvasBoardMemoKind::Title, "Old")]);
    board.harness.app.view().page_index = 0;
    let mut cleared = memo(CanvasBoardMemoKind::Title, "");
    cleared.color = "#20252D".into();
    board.open(cleared, false);
    board.press_key(Key::Escape);
    assert!(board.harness.app.board_memo_editor.is_none(), "an empty title cannot be saved: the card closes");
    assert!(board.memo_changes().is_empty(), "nothing is sent");
    assert_eq!(board.page_memos()[0].text, "Old", "the title stays as it was");
    assert!(board.harness.message().is_some_and(|(_, text)| text.contains("보드 제목")), "the window says why");
    board.frame(Vec::new());
    assert!(!board.field_focused(), "no closed card holds the keyboard");
}

#[test]
fn a_field_that_took_the_keyboard_without_a_press_keeps_it() {
    let mut board = Board::new(Vec::new());
    board.open(memo(CanvasBoardMemoKind::Memo, ""), true);
    assert!(board.field_focused());
    // A dialog's field or F2's rename field asks for the keyboard when it opens, without a press.
    let other = Id::new("a-field-opened-by-a-shortcut");
    board.harness.ctx.memory_mut(|memory| memory.request_focus(other));
    board.frame(Vec::new());
    assert!(!board.field_focused(), "the memo does not take the keyboard back from that field");
}

#[test]
fn the_bar_is_in_the_dialog_order_so_a_later_dialog_covers_it() {
    let mut board = Board::new(Vec::new());
    board.open(memo(CanvasBoardMemoKind::Memo, ""), true);
    board.frame(Vec::new());
    let bar = board.area("board-memo-bar");
    let layer = board.harness.ctx.layer_id_at(bar.center()).expect("the bar is drawn");
    assert_eq!(layer.order, eframe::egui::Order::Middle, "the bar is not above dialog windows");
}

#[test]
fn a_selected_memo_has_its_bar_and_a_swatch_or_delete_is_one_change_without_editing() {
    let mut board = Board::new(vec![memo(CanvasBoardMemoKind::Memo, "note")]);
    let output = board.frame(Vec::new());
    assert!(!texts(&output).iter().any(|(shape, _)| shape.galley.text() == "삭제"), "an unselected memo has no bar");
    board.harness.app.selected_board_memo = Some(MEMO_ID.into());
    board.frame(Vec::new());
    let output = board.frame(Vec::new());
    assert!(!texts(&output).iter().any(|(shape, _)| shape.galley.text() == "다른 색"), "the selected bar has swatches and delete only");
    board.click(swatch_center(&output, "#BAE6FD"));
    assert!(board.harness.app.board_memo_editor.is_none(), "a swatch does not open the editor");
    let changes = board.memo_changes();
    assert_eq!(changes.len(), 1, "one board memo change");
    assert_eq!(changes[0]["memos"][0]["color"], "#BAE6FD");
    assert_eq!(changes[0]["memos"][0]["text"], "note", "only the colour changes");

    let mut board = Board::new(vec![memo(CanvasBoardMemoKind::Memo, "note")]);
    board.harness.app.selected_board_memo = Some(MEMO_ID.into());
    board.frame(Vec::new());
    let output = board.frame(Vec::new());
    board.click(label_center(&output, "삭제"));
    assert!(board.harness.app.board_memo_editor.is_none());
    let changes = board.memo_changes();
    assert_eq!(changes.len(), 1, "one board memo change");
    assert_eq!(changes[0]["memos"], json!([]), "the memo is removed");
}

#[test]
fn an_editor_that_is_not_on_screen_leaves_esc_and_the_ime_to_the_window() {
    let mut board = Board::new(Vec::new());
    board.open(memo(CanvasBoardMemoKind::Memo, ""), true);
    assert!(board.harness.app.board_memo_editor_shown());
    board.harness.app.screen = super::super::Screen::Home;
    assert!(!board.harness.app.board_memo_editor_shown(), "on the home screen the editor is not drawn");
    board.harness.app.board_rect = None;
    board.harness.app.screen = super::super::Screen::Editor;
    assert!(!board.harness.app.board_memo_editor_shown(), "before the board is laid out the editor is not drawn");
}

#[test]
fn a_swatch_or_an_outside_press_during_a_composition_keeps_the_syllable_once() {
    let mut board = Board::new(Vec::new());
    board.open(memo(CanvasBoardMemoKind::Memo, ""), true);
    let output = board.frame(vec![Event::Ime(ImeEvent::Enabled), preedit("하")]);
    let output = board.click(swatch_center(&output, "#D9F99D"));
    assert!(output.platform_output.ime.is_some(), "the IME keeps its place through the swatch's frame");
    assert!(board.field_focused());
    board.frame(vec![commit("하")]);
    assert_eq!(board.text().as_deref(), Some("하"), "the syllable is in the memo once");

    let mut board = Board::new(Vec::new());
    board.open(memo(CanvasBoardMemoKind::Memo, ""), true);
    board.frame(vec![Event::Ime(ImeEvent::Enabled), preedit("하")]);
    let outside = board.outside();
    press_at(&mut board, outside);
    assert!(board.harness.app.board_memo_editor.is_none(), "the outside press confirms");
    let changes = board.memo_changes();
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0]["memos"][0]["text"], "하", "the syllable being composed is kept");
}

// 68p 1 (Esc and V on the memo and hand tools, the guards, the ui.state value) and 68o 5 (V on figure and slides boards).

fn set_tool(board: &mut Board, tool: super::super::board::Tool) {
    board.harness.app.view().tool = tool;
}

fn tool_of(board: &Board) -> super::super::board::Tool {
    board.harness.app.view_ref().tool
}

/// Presses V, then Esc, with the tool at `tool`: neither key may change it while `why` holds. `arm` sets the state again before
/// each key: a frame clears a dialog's flag once its own handling has run, as a dialog does when its request is gone.
fn assert_v_and_esc_keep_the_tool(board: &mut Board, tool: super::super::board::Tool, why: &str, arm: &dyn Fn(&mut Board)) {
    set_tool(board, tool);
    arm(board);
    board.press_key(Key::V);
    assert_eq!(tool_of(board), tool, "V does nothing while {why}");
    arm(board);
    board.press_key(Key::Escape);
    assert_eq!(tool_of(board), tool, "Esc does nothing while {why}");
}
/// The memo shortcut pass alone, with no frame drawn. Used for a board kind whose body the canvas fixture does not hold.
fn press_memo_pass(board: &mut Board, pressed: Key) {
    let mut input = screen(1.0, 1280.0, 800.0);
    input.events = vec![key(pressed)];
    let ctx = board.harness.ctx.clone();
    let app = &mut board.harness.app;
    let _ = ctx.run(input, |ctx| app.handle_board_memo_shortcut(ctx));
}

#[test]
fn esc_leaves_the_memo_tool_and_keeps_the_selection_until_a_second_esc() {
    let mut board = Board::new(Vec::new());
    assert_eq!(board.harness.app.selected_nodes, vec!["text-1".to_owned()], "the fixture selects the text node");
    board.press_key(Key::N);
    assert_eq!(tool_of(&board), super::super::board::Tool::Memo, "N picks the memo tool");
    board.press_key(Key::Escape);
    assert_eq!(tool_of(&board), super::super::board::Tool::Select, "the first Esc leaves the memo tool");
    assert_eq!(board.harness.app.selected_nodes, vec!["text-1".to_owned()], "and keeps the selection");
    board.press_key(Key::Escape);
    assert!(board.harness.app.selected_nodes.is_empty(), "the next Esc, on the select tool, clears the selection");
}

#[test]
fn esc_and_v_leave_the_hand_tool_and_keep_the_selection() {
    use super::super::board::Tool;
    let mut board = Board::new(Vec::new());
    set_tool(&mut board, Tool::Hand);
    board.press_key(Key::Escape);
    assert_eq!(tool_of(&board), Tool::Select, "Esc leaves the hand tool");
    assert_eq!(board.harness.app.selected_nodes, vec!["text-1".to_owned()], "and keeps the selection");
    set_tool(&mut board, Tool::Hand);
    board.press_key(Key::V);
    assert_eq!(tool_of(&board), Tool::Select, "V leaves the hand tool");
    assert_eq!(board.harness.app.selected_nodes, vec!["text-1".to_owned()], "and keeps the selection");
}

#[test]
fn ui_state_reports_the_memo_tool_as_memo() {
    use super::super::board::Tool;
    let mut board = Board::new(Vec::new());
    set_tool(&mut board, Tool::Memo);
    assert_eq!(board.harness.app.ui_state().tool.as_deref(), Some("memo"));
    set_tool(&mut board, Tool::Hand);
    assert_eq!(board.harness.app.ui_state().tool.as_deref(), Some("hand"));
    set_tool(&mut board, Tool::Select);
    assert_eq!(board.harness.app.ui_state().tool.as_deref(), Some("select"));
}

#[test]
fn v_and_esc_do_nothing_while_the_memo_editor_is_open() {
    use super::super::board::Tool;
    let mut board = Board::new(Vec::new());
    board.open(memo(CanvasBoardMemoKind::Memo, ""), true);
    assert!(board.harness.app.board_memo_editor.is_some(), "the memo editor is open");
    assert_v_and_esc_keep_the_tool(&mut board, Tool::Hand, "the memo editor is open", &|_| {});
}

#[test]
fn v_and_esc_do_nothing_while_an_inline_text_edit_is_open() {
    use super::super::board::Tool;
    let mut board = Board::new(Vec::new());
    board.press_key(Key::Enter);
    assert!(board.harness.app.inline_text_edit.is_some(), "Enter starts the inline edit");
    assert_v_and_esc_keep_the_tool(&mut board, Tool::Hand, "an inline text edit is open", &|_| {});
}

#[test]
fn v_and_esc_do_nothing_while_a_page_section_editor_is_open() {
    use super::super::board::Tool;
    let mut board = Board::new(Vec::new());
    board.harness.app.page_section_editor = Some(super::super::page_sections::SectionEditor {
        artifact_id: "a1".into(),
        revision: 1,
        expected: Vec::new(),
        section: designllm_contracts::CanvasPageSection { id: "s1".into(), name: "섹션".into(), start_page_id: "page-1".into() },
        page_index: 0,
        is_new: true,
        message: None,
    });
    assert_v_and_esc_keep_the_tool(&mut board, Tool::Hand, "a page section editor is open", &|_| {});
}

#[test]
fn v_and_esc_do_nothing_while_a_home_confirmation_is_open() {
    use super::super::board::Tool;
    let mut board = Board::new(Vec::new());
    let arm = |board: &mut Board| {
        let run = designllm_contracts::UiRun {
            run_id: "run-1".into(),
            view_id: "view-1".into(),
            expected: None,
            command_id: "add.paragraph".into(),
            args: serde_json::json!({}),
            actor: "agent".into(),
            permission: designllm_contracts::UiPermission::Change,
            allowed_intents: Default::default(),
            status: designllm_contracts::UiRunStatus::Queued,
            timeout_ms: 10_000,
            result: None,
            error: None,
        };
        board.harness.app.ui.pending_home_create = Some((run, std::time::Instant::now() + std::time::Duration::from_secs(60)));
    };
    assert_v_and_esc_keep_the_tool(&mut board, Tool::Hand, "a home confirmation covers the window", &arm);
}
#[test]
fn v_and_esc_do_nothing_while_a_key_dialog_is_open() {
    use super::super::board::Tool;
    let mut board = Board::new(Vec::new());
    assert_v_and_esc_keep_the_tool(&mut board, Tool::Hand, "a key dialog is open", &|board| board.harness.app.close_confirmation_open = true);
}

#[test]
fn esc_does_nothing_while_a_memo_save_is_in_flight() {
    use super::super::board::Tool;
    let mut board = Board::new(Vec::new());
    board.harness.app.board_memo_mutation = Some(super::super::state::BoardMemoMutation {
        token: "t1".into(),
        artifact_id: "a1".into(),
        revision: 1,
        page_id: "page-1".into(),
        page_index: 0,
        expected: Vec::new(),
        desired: Vec::new(),
        selected_memo_id: None,
        editor: None,
        message: None,
        pending: true,
        retry_allowed: false,
        outcome_uncertain: false,
        held_edit_id: None,
    });
    set_tool(&mut board, Tool::Memo);
    board.press_key(Key::Escape);
    assert_eq!(tool_of(&board), Tool::Memo, "Esc does nothing while a memo save is in flight");
}

#[test]
fn esc_does_nothing_while_a_menu_is_open() {
    use super::super::board::Tool;
    let mut board = Board::new(Vec::new());
    set_tool(&mut board, Tool::Memo);
    eframe::egui::Popup::open_id(&board.harness.ctx, Id::new("guard-test-menu"));
    board.press_key(Key::Escape);
    assert_eq!(tool_of(&board), Tool::Memo, "Esc does nothing while a menu is open");
}

#[test]
fn v_picks_the_select_tool_on_figure_and_slides_boards() {
    use super::super::board::Tool;
    for kind in [ArtifactKind::Figure, ArtifactKind::Slides] {
        let mut board = Board::new(Vec::new());
        board.harness.app.active.as_mut().expect("the fixture is open").kind = kind;
        set_tool(&mut board, Tool::Hand);
        press_memo_pass(&mut board, Key::V);
        assert_eq!(tool_of(&board), Tool::Select, "V picks the select tool on {kind:?}");
    }
}

#[test]
fn v_does_nothing_on_a_figure_board_while_a_text_field_has_the_keyboard() {
    use super::super::board::Tool;
    let mut board = Board::new(Vec::new());
    board.harness.app.active.as_mut().expect("the fixture is open").kind = ArtifactKind::Figure;
    set_tool(&mut board, Tool::Hand);
    let ctx = board.harness.ctx.clone();
    let mut text = String::new();
    let mut field = |ctx: &eframe::egui::Context| {
        eframe::egui::CentralPanel::default().show(ctx, |ui| {
            ui.text_edit_singleline(&mut text).request_focus();
        });
    };
    let _ = ctx.run(screen(1.0, 1280.0, 800.0), |ctx| field(ctx));
    let mut input = screen(1.0, 1280.0, 800.0);
    input.events = vec![key(Key::V)];
    let app = &mut board.harness.app;
    let _ = ctx.run(input, |ctx| {
        field(ctx);
        app.handle_board_memo_shortcut(ctx);
    });
    assert_eq!(tool_of(&board), Tool::Hand, "V is typed into the field, not taken as the tool key");
}
