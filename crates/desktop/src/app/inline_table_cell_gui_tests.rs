//! U05 (68 item 11): typing into a table the window has just inserted. The window runs on a real host
//! (`test_support::Window`): the table is made by `canvas.table.add` and the board draws the core's render. Every frame
//! after that is the application's own (`eframe::App::raw_input_hook`, then `eframe::App::update` with a test
//! `eframe::Frame`), so the registry's shortcuts, the event poll and the dialogs run in their real order. `Driver`
//! plays the RPC worker: at once (each frame's requests are answered before the next frame), or with latency (a call
//! comes back a frame later, a render and its preview `RENDER_FRAMES` frames later, as the application's worker
//! answers). Every cell of a new table is empty.

use std::collections::VecDeque;

use designllm_contracts::{ArtifactBody, CanvasTable, Intent, Node, Operation};
use eframe::egui::{self, Event, ImeEvent, Key, Modifiers, Pos2, Vec2};
use serde_json::json;

use super::test_support::{primary_button, Host, Window};
use crate::rpc::{RpcPayload, RpcReply, Work};

/// A canvas with a new 2 × 2 table made by the desktop's table command, and a window that shows it.
fn new_table(host: &mut Host) -> (String, Window, CanvasTable) {
    let canvas = host.create("canvas", "표 입력");
    let mut window = Window::open_with_previews(host, &canvas);
    window.app.screen = super::Screen::Editor;
    window.draw(host, 1.0);
    window.app.invoke("canvas.table.add", json!({ "rows": 2, "columns": 2 })).expect("the table is added");
    window.settle(host);
    window.draw(host, 1.0);
    let table = table_of(host, &canvas);
    (canvas, window, table)
}

fn table_of(host: &mut Host, canvas: &str) -> CanvasTable {
    let artifact = host.artifact(canvas);
    let ArtifactBody::Canvas(document) = &artifact.body else { panic!("a canvas") };
    document.pages[0].nodes.iter().find_map(|node| node.ext.table.clone()).expect("the table node")
}

fn table_node(host: &mut Host, canvas: &str) -> Node {
    let artifact = host.artifact(canvas);
    let ArtifactBody::Canvas(document) = &artifact.body else { panic!("a canvas") };
    document.pages[0].nodes.iter().find(|node| node.ext.table.is_some()).cloned().expect("the table node")
}

fn cell_text(host: &mut Host, canvas: &str, cell: &str) -> String {
    let artifact = host.artifact(canvas);
    super::doc_tree::find_node(&artifact, cell).map(|node| node.text.clone()).expect("the cell")
}

fn set_cell_text(host: &mut Host, window: &mut Window, canvas: &str, cell: &str, text: &str) {
    host.apply(canvas, vec![Operation::SetText { node_id: cell.into(), text: text.into() }], [Intent::Content].into_iter().collect())
        .expect("the cell text is set");
    window.app.refresh_active();
    window.settle(host);
}

/// The screen rectangle of `node` (a cell or the table), from the frame the core laid it out in and where page 0 was drawn.
fn frame_rect(window: &Window, node: &str) -> egui::Rect {
    let page = window.app.page_textures.get(&0).expect("page 0 is drawn");
    let frame = page.frames.get(node).unwrap_or_else(|| panic!("the render has a frame for {node}"));
    super::page_to_screen(*frame, page.page.size, window.app.page_rect.expect("page 0 is drawn"))
}

fn centre(window: &Window, node: &str) -> Pos2 {
    frame_rect(window, node).center()
}

fn key(key: Key) -> Event {
    key_with(key, Modifiers::NONE)
}

fn key_with(key: Key, modifiers: Modifiers) -> Event {
    Event::Key { key, physical_key: Some(key), pressed: true, repeat: false, modifiers }
}

fn editing(window: &Window) -> Option<String> {
    window.app.inline_text_edit.as_ref().map(|edit| edit.node_id.clone())
}

fn conflict(window: &Window) -> Option<String> {
    window.app.inline_text_edit.as_ref().and_then(|edit| edit.conflict.clone())
}

fn caret_range(window: &Window) -> (usize, usize) {
    let range = egui::TextEdit::load_state(&window.ctx, window.app.text_editor_id)
        .and_then(|state| state.cursor.char_range())
        .expect("the editor has a caret");
    (range.primary.index.min(range.secondary.index), range.primary.index.max(range.secondary.index))
}

const RENDER_FRAMES: u32 = 6;

/// The application's frames and its RPC worker.
struct Driver {
    /// `None`: every request is answered before the next frame.
    latency: Option<VecDeque<(u32, Work)>>,
    /// Frames a call waits for its answer (with latency).
    call_frames: u32,
    /// Renders are not answered at all (a render that never comes back).
    renders_held: bool,
    frame: u32,
    time: f64,
    /// Every call method the window sent, oldest first.
    sent: Vec<String>,
}

impl Driver {
    fn at_once() -> Self {
        Self { latency: None, call_frames: 1, renders_held: false, frame: 0, time: 100.0, sent: Vec::new() }
    }

    fn slow() -> Self {
        Self { latency: Some(VecDeque::new()), ..Self::at_once() }
    }

    /// One frame of the application at egui time `self.time`, then the answers that are due.
    fn frame(&mut self, window: &mut Window, host: &mut Host, events: Vec<Event>) {
        self.frame += 1;
        self.time += 0.016;
        let mut input = super::test_support::screen(1.0, 1280.0, 800.0);
        input.time = Some(self.time);
        input.events = events;
        eframe::App::raw_input_hook(&mut window.app, &window.ctx, &mut input);
        let mut frame = eframe::Frame::_new_kittest();
        let app = &mut window.app;
        let _ = window.ctx.run(input, |ctx| eframe::App::update(app, ctx, &mut frame));
        let Some(held) = self.latency.as_mut() else {
            let before = window.calls.len();
            window.settle(host);
            self.sent.extend(window.calls[before..].iter().map(|(method, _, _)| method.clone()));
            return;
        };
        for work in window.work.try_iter() {
            let delay = match &work {
                Work::Call { method, .. } if method == "render.get" => RENDER_FRAMES,
                Work::ReadAsset { .. } => RENDER_FRAMES,
                _ => self.call_frames,
            };
            if let Work::Call { method, .. } = &work {
                self.sent.push(method.clone());
            }
            held.push_back((self.frame + delay, work));
        }
        let renders_held = self.renders_held;
        let mut kept = VecDeque::new();
        while let Some((due, work)) = held.pop_front() {
            let render = matches!(&work, Work::Call { method, .. } if method == "render.get") || matches!(work, Work::ReadAsset { .. });
            if due > self.frame || (render && renders_held) {
                kept.push_back((due, work));
                continue;
            }
            match work {
                Work::Call { ticket, method, params } => {
                    let result = host.call(&method, params).map(RpcPayload::Json);
                    window.replies.send(RpcReply { ticket, result }).expect("the window listens");
                }
                Work::ReadAsset { ticket, asset } => {
                    let result = Ok(super::test_support::preview_pixels(host, &asset));
                    window.replies.send(RpcReply { ticket, result }).expect("the window listens");
                }
                _ => {}
            }
        }
        *held = kept;
    }

    fn idle(&mut self, window: &mut Window, host: &mut Host, frames: u32) {
        for _ in 0..frames {
            self.frame(window, host, Vec::new());
        }
    }

    fn press(&mut self, window: &mut Window, host: &mut Host, at: Pos2) {
        self.frame(window, host, vec![Event::PointerMoved(at), primary_button(at, true)]);
    }

    fn release(&mut self, window: &mut Window, host: &mut Host, at: Pos2) {
        self.frame(window, host, vec![primary_button(at, false)]);
    }

    /// A click (press and release in two frames), then a pause past egui's double-click delay so the next press is a new
    /// click.
    fn click(&mut self, window: &mut Window, host: &mut Host, at: Pos2) {
        self.press(window, host, at);
        self.release(window, host, at);
        self.idle(window, host, 2);
        self.time += 0.5;
    }

    /// `count` clicks in a row at `at`, within the double-click delay.
    fn clicks(&mut self, window: &mut Window, host: &mut Host, at: Pos2, count: usize) {
        for _ in 0..count {
            self.press(window, host, at);
            self.release(window, host, at);
            self.idle(window, host, 1);
        }
        self.idle(window, host, 2);
        self.time += 0.5;
    }

    fn drag(&mut self, window: &mut Window, host: &mut Host, from: Pos2, to: Pos2) {
        self.press(window, host, from);
        for step in 1..=4 {
            let at = from + (to - from) * (step as f32 / 4.0);
            self.frame(window, host, vec![Event::PointerMoved(at)]);
        }
        self.release(window, host, to);
        self.idle(window, host, 2);
        self.time += 0.5;
    }

    fn keys(&mut self, window: &mut Window, host: &mut Host, events: Vec<Event>) {
        self.frame(window, host, events);
        self.idle(window, host, 1);
    }
}

#[test]
fn table_cell_a_click_on_an_empty_cell_puts_the_caret_in_it_typing_is_stored_and_escape_selects_the_table() {
    let mut host = Host::start();
    let (canvas, mut window, table) = new_table(&mut host);
    let mut app = Driver::at_once();
    let cell = table.rows[0].cell_ids[0].clone();

    let at = centre(&window, &cell);
    app.click(&mut window, &mut host, at);
    assert_eq!(editing(&window).as_deref(), Some(cell.as_str()), "one click opens the empty cell; notice: {:?}", window.message());
    assert_eq!(window.ctx.memory(|memory| memory.focused()), Some(window.app.text_editor_id), "the cell editor has the keyboard");
    app.keys(&mut window, &mut host, vec![Event::Text("ab".into())]);
    assert_eq!(window.app.text_draft, "ab", "the typed text is in the editor");
    app.keys(&mut window, &mut host, vec![key(Key::Escape)]);
    app.idle(&mut window, &mut host, 2);
    assert!(window.app.inline_text_edit.is_none(), "Escape ends the edit");
    assert_eq!(window.app.selected_nodes, vec![table_node(&mut host, &canvas).id], "Escape leaves the whole table selected");
    assert_eq!(cell_text(&mut host, &canvas, &cell), "ab", "the cell keeps the typed text");

    // The text is stored: a restarted host reads it back.
    host.restart();
    assert_eq!(cell_text(&mut host, &canvas, &cell), "ab", "the text is kept after the host reopens the document");
}

#[test]
fn table_cell_double_click_opens_an_empty_cell_selects_a_word_and_a_triple_click_its_line() {
    let mut host = Host::start();
    let (canvas, mut window, table) = new_table(&mut host);
    let mut app = Driver::at_once();
    let empty = table.rows[1].cell_ids[1].clone();
    let at = centre(&window, &empty);
    app.clicks(&mut window, &mut host, at, 2);
    assert_eq!(editing(&window).as_deref(), Some(empty.as_str()), "the double click opens the empty cell; notice: {:?}", window.message());
    app.keys(&mut window, &mut host, vec![key(Key::Escape)]);

    let filled = table.rows[1].cell_ids[0].clone();
    set_cell_text(&mut host, &mut window, &canvas, &filled, "abcdef ghijkl");
    app.idle(&mut window, &mut host, 3);
    let rect = frame_rect(&window, &filled);
    let on_first_word = Pos2::new(rect.left() + 7.0, rect.center().y);
    app.clicks(&mut window, &mut host, on_first_word, 2);
    assert_eq!(editing(&window).as_deref(), Some(filled.as_str()), "the double click opens the cell; notice: {:?}", window.message());
    assert_eq!(caret_range(&window), (0, 6), "the second click selects the word under the pointer");
    app.clicks(&mut window, &mut host, on_first_word, 3);
    assert_eq!(caret_range(&window), (0, 13), "the third click selects the line");
}

#[test]
fn table_cell_ime_commit_in_an_empty_cell_inserts_the_syllable_once() {
    let mut host = Host::start();
    let (canvas, mut window, table) = new_table(&mut host);
    let mut app = Driver::at_once();
    let cell = table.rows[1].cell_ids[1].clone();

    let at = centre(&window, &cell);
    app.click(&mut window, &mut host, at);
    assert_eq!(editing(&window).as_deref(), Some(cell.as_str()), "the click opens the cell; notice: {:?}", window.message());
    app.frame(&mut window, &mut host, vec![Event::Ime(ImeEvent::Enabled), Event::Ime(ImeEvent::Preedit("ㅎ".into()))]);
    app.frame(&mut window, &mut host, vec![Event::Ime(ImeEvent::Preedit("한".into()))]);
    app.keys(&mut window, &mut host, vec![Event::Ime(ImeEvent::Commit("한".into()))]);
    assert_eq!(conflict(&window), None, "after the commit");
    app.keys(&mut window, &mut host, vec![key(Key::Escape)]);
    assert_eq!(cell_text(&mut host, &canvas, &cell), "한", "the committed syllable is stored once");
}

/// F2 is `edit.rename`'s shortcut: the registry runs it before the board draws. On a table or a text it opens the text;
/// on anything else it stays the rename.
#[test]
fn table_cell_enter_and_f2_open_the_selected_cell_or_the_table_s_last_cell_and_f2_elsewhere_stays_rename() {
    for open_key in [Key::Enter, Key::F2] {
        let mut host = Host::start();
        let (canvas, mut window, table) = new_table(&mut host);
        let mut app = Driver::at_once();
        let (first, second) = (table.rows[0].cell_ids[0].clone(), table.rows[0].cell_ids[1].clone());
        // A cell selected the way the layer list selects it.
        window.app.set_selected_ids(vec![second.clone()]);
        window.app.set_selection(designllm_contracts::Selection { node_ids: vec![second.clone()], text_range: None });
        app.idle(&mut window, &mut host, 2);
        assert!(window.app.inline_text_edit.is_none(), "{open_key:?}: selecting does not edit");
        app.keys(&mut window, &mut host, vec![key(open_key)]);
        assert_eq!(editing(&window).as_deref(), Some(second.as_str()), "{open_key:?} opens the selected cell; notice: {:?}", window.message());
        app.keys(&mut window, &mut host, vec![Event::Text("x".into())]);
        app.keys(&mut window, &mut host, vec![key(Key::Escape)]);
        app.idle(&mut window, &mut host, 2);
        assert_eq!(cell_text(&mut host, &canvas, &second), "x", "{open_key:?}: the cell keeps its text");

        // Escape left the table selected: the key opens the cell edited last.
        let table_id = table_node(&mut host, &canvas).id;
        assert_eq!(window.app.selected_nodes, vec![table_id.clone()]);
        app.keys(&mut window, &mut host, vec![key(open_key)]);
        assert_eq!(editing(&window).as_deref(), Some(second.as_str()), "{open_key:?} on the table opens its cell edited last");
        app.keys(&mut window, &mut host, vec![key(Key::Escape)]);
        app.idle(&mut window, &mut host, 2);
        // A table selected anew (its edited cell forgotten) opens its first cell.
        window.app.cell_nav = Default::default();
        window.app.set_selected_ids(vec![table_id.clone()]);
        window.app.set_selection(designllm_contracts::Selection { node_ids: vec![table_id], text_range: None });
        app.idle(&mut window, &mut host, 2);
        app.keys(&mut window, &mut host, vec![key(open_key)]);
        assert_eq!(editing(&window).as_deref(), Some(first.as_str()), "{open_key:?} on a table opens its first cell");
        app.keys(&mut window, &mut host, vec![key(Key::Escape)]);
    }

    // F2 on a shape without text is the rename: no text edit opens.
    let mut host = Host::start();
    let (canvas, mut window, _) = new_table(&mut host);
    let mut app = Driver::at_once();
    let page_id = match &host.artifact(&canvas).body { ArtifactBody::Canvas(document) => document.pages[0].id.clone(), _ => unreachable!() };
    let shape: Node = serde_json::from_value(json!({
        "id": "shape-1", "kind": "rectangle", "bounds": { "x": 100_000, "y": 3_000_000, "width": 900_000, "height": 600_000 },
    })).expect("a shape");
    host.apply(&canvas, vec![Operation::AddNode { page_id, parent_id: None, node: shape }], [Intent::Content, Intent::Design].into_iter().collect())
        .expect("the shape is added");
    window.app.refresh_active();
    window.settle(&mut host);
    window.app.set_selected_ids(vec!["shape-1".into()]);
    window.app.set_selection(designllm_contracts::Selection { node_ids: vec!["shape-1".into()], text_range: None });
    app.idle(&mut window, &mut host, 2);
    app.keys(&mut window, &mut host, vec![key(Key::F2)]);
    assert!(window.app.inline_text_edit.is_none(), "F2 on a shape opens no text");
    assert!(window.message().is_some_and(|message| message.starts_with("edit.rename")), "F2 on a shape is the rename: {:?}", window.message());
}

#[test]
fn table_cell_tab_and_shift_tab_move_between_cells_and_keys_typed_during_the_hand_over_reach_the_next_cell() {
    let mut host = Host::start();
    let (canvas, mut window, table) = new_table(&mut host);
    let mut app = Driver::slow();
    app.idle(&mut window, &mut host, 20);
    let cells: Vec<String> = table.rows.iter().flat_map(|row| row.cell_ids.clone()).collect();

    let at = centre(&window, &cells[0]);
    app.click(&mut window, &mut host, at);
    app.idle(&mut window, &mut host, 10);
    assert_eq!(editing(&window).as_deref(), Some(cells[0].as_str()), "the click opens the first cell; notice: {:?}", window.message());
    // Each text is typed in the frame right after Tab, while the old cell's last change is still on its way.
    app.frame(&mut window, &mut host, vec![Event::Text("가".into())]);
    for (index, text) in ["b", "c", "d"].into_iter().enumerate() {
        app.frame(&mut window, &mut host, vec![key(Key::Tab)]);
        app.frame(&mut window, &mut host, vec![Event::Text(text.into())]);
        app.idle(&mut window, &mut host, 40);
        assert_eq!(editing(&window).as_deref(), Some(cells[index + 1].as_str()), "Tab moves to cell {}; notice: {:?}", index + 1, window.message());
        assert_eq!(conflict(&window), None, "after Tab");
    }
    // Tab in the last cell keeps the edit there.
    app.keys(&mut window, &mut host, vec![key(Key::Tab)]);
    app.idle(&mut window, &mut host, 10);
    assert_eq!(editing(&window).as_deref(), Some(cells[3].as_str()), "Tab at the table's end stays in the last cell");
    app.keys(&mut window, &mut host, vec![key_with(Key::Tab, Modifiers::SHIFT)]);
    app.idle(&mut window, &mut host, 40);
    assert_eq!(editing(&window).as_deref(), Some(cells[2].as_str()), "Shift+Tab moves to the previous cell");
    app.keys(&mut window, &mut host, vec![key(Key::Escape)]);
    app.idle(&mut window, &mut host, 60);
    let stored: Vec<String> = cells.iter().map(|cell| cell_text(&mut host, &canvas, cell)).collect();
    assert_eq!(stored, ["가", "b", "c", "d"], "each cell keeps the text typed in it");
}

#[test]
fn table_cell_arrow_keys_on_the_edge_rows_move_to_the_cell_beside_wrapped_rows_stay_and_enter_breaks_the_line() {
    let mut host = Host::start();
    let (canvas, mut window, table) = new_table(&mut host);
    let mut app = Driver::at_once();
    let (top_left, top_right, bottom_left) =
        (table.rows[0].cell_ids[0].clone(), table.rows[0].cell_ids[1].clone(), table.rows[1].cell_ids[0].clone());

    let at = centre(&window, &top_left);
    app.click(&mut window, &mut host, at);
    app.keys(&mut window, &mut host, vec![Event::Text("a".into())]);
    app.keys(&mut window, &mut host, vec![key(Key::ArrowRight)]);
    app.idle(&mut window, &mut host, 1);
    assert_eq!(editing(&window).as_deref(), Some(top_right.as_str()), "ArrowRight at the end of the text moves to the right cell");
    // At the table's right edge the arrow stays the editor's.
    app.keys(&mut window, &mut host, vec![key(Key::ArrowRight)]);
    assert_eq!(editing(&window).as_deref(), Some(top_right.as_str()), "ArrowRight at the table's edge stays in the cell");
    app.keys(&mut window, &mut host, vec![key(Key::ArrowLeft)]);
    app.idle(&mut window, &mut host, 1);
    assert_eq!(editing(&window).as_deref(), Some(top_left.as_str()), "ArrowLeft at the start of an empty cell moves to the left cell");

    // Inside the text the arrow moves the caret only; Enter is a line break in the cell.
    app.keys(&mut window, &mut host, vec![key(Key::ArrowLeft)]);
    assert_eq!(editing(&window).as_deref(), Some(top_left.as_str()), "ArrowLeft inside the text stays in the cell");
    app.keys(&mut window, &mut host, vec![key(Key::ArrowRight), key(Key::Enter), Event::Text("b".into())]);
    assert_eq!(editing(&window).as_deref(), Some(top_left.as_str()), "Enter stays in the cell");
    assert_eq!(window.app.text_draft, "a\nb", "Enter breaks the line in the cell");
    app.keys(&mut window, &mut host, vec![key(Key::ArrowDown)]);
    app.idle(&mut window, &mut host, 1);
    assert_eq!(editing(&window).as_deref(), Some(bottom_left.as_str()), "ArrowDown on the last line moves to the cell below");
    app.keys(&mut window, &mut host, vec![key(Key::Escape)]);
    assert_eq!(cell_text(&mut host, &canvas, &top_left), "a\nb");

    // A text wrapped over rows: ArrowDown on its first drawn row stays in the cell.
    set_cell_text(&mut host, &mut window, &canvas, &top_right, &"가나다라 ".repeat(30));
    app.idle(&mut window, &mut host, 3);
    let rect = frame_rect(&window, &top_right);
    app.click(&mut window, &mut host, Pos2::new(rect.left() + 8.0, rect.top() + 8.0));
    assert_eq!(editing(&window).as_deref(), Some(top_right.as_str()));
    assert_eq!(window.app.text_adapter.caret_row_edges(0), Some((true, false)), "the text wraps over rows");
    app.keys(&mut window, &mut host, vec![key(Key::ArrowDown)]);
    app.idle(&mut window, &mut host, 1);
    assert_eq!(editing(&window).as_deref(), Some(top_right.as_str()), "ArrowDown on a wrapped first row stays in the cell");
}

#[test]
fn table_cell_a_click_elsewhere_ends_the_edit_and_a_late_hit_reply_moves_it_without_a_conflict() {
    let mut host = Host::start();
    let (canvas, mut window, table) = new_table(&mut host);
    let mut app = Driver::at_once();
    let (first, other) = (table.rows[0].cell_ids[0].clone(), table.rows[1].cell_ids[1].clone());

    let at = centre(&window, &first);
    app.click(&mut window, &mut host, at);
    app.keys(&mut window, &mut host, vec![Event::Text("2".into())]);
    let at = centre(&window, &other);
    app.click(&mut window, &mut host, at);
    app.idle(&mut window, &mut host, 1);
    assert_eq!(editing(&window).as_deref(), Some(other.as_str()), "the clicked cell is edited; notice: {:?}", window.message());
    assert_eq!(cell_text(&mut host, &canvas, &first), "2", "the first cell's edit was stored");
    // A click on the empty board ends the edit and clears the selection.
    let board = window.app.board_rect.expect("the board is drawn");
    app.click(&mut window, &mut host, Pos2::new(board.left() + 8.0, board.bottom() - 8.0));
    assert!(window.app.inline_text_edit.is_none(), "a click off the page ends the edit");
    assert!(window.app.selected_nodes.is_empty(), "a click off the page clears the selection");

    // Two clicks on two cells before the first reply: the edit opens on the first, then moves to the second.
    let mut slow = Driver { call_frames: 4, ..Driver::slow() };
    slow.time = app.time;
    slow.idle(&mut window, &mut host, 10);
    let (a, b) = (centre(&window, &first), centre(&window, &other));
    slow.press(&mut window, &mut host, a);
    slow.release(&mut window, &mut host, a);
    slow.time += 0.5;
    slow.press(&mut window, &mut host, b);
    slow.release(&mut window, &mut host, b);
    slow.idle(&mut window, &mut host, 40);
    assert_eq!(editing(&window).as_deref(), Some(other.as_str()), "the edit ends on the cell clicked last; notice: {:?}", window.message());
    assert_eq!(conflict(&window), None, "the late reply did not pull the selection from under the edit");
}

#[test]
fn table_cell_drag_inside_a_cell_selects_text_the_border_drag_moves_the_table_and_a_border_click_selects_it() {
    let mut host = Host::start();
    let (canvas, mut window, table) = new_table(&mut host);
    let mut app = Driver::at_once();
    let cell = table.rows[1].cell_ids[0].clone();
    set_cell_text(&mut host, &mut window, &canvas, &cell, "abcdefghij");
    app.idle(&mut window, &mut host, 3);
    let table_id = table_node(&mut host, &canvas).id;
    let before = table_node(&mut host, &canvas).bounds;
    let rect = frame_rect(&window, &cell);
    app.drag(&mut window, &mut host, Pos2::new(rect.left() + 6.0, rect.center().y), Pos2::new(rect.left() + 40.0, rect.center().y));
    assert_eq!(editing(&window).as_deref(), Some(cell.as_str()), "the drag opens the cell; notice: {:?}", window.message());
    let (start, end) = caret_range(&window);
    assert!(end > start, "the drag selects text from the press: {start}..{end}");
    assert_eq!(table_node(&mut host, &canvas).bounds, before, "a text drag does not move the table");
    app.keys(&mut window, &mut host, vec![key(Key::Escape)]);
    app.idle(&mut window, &mut host, 2);

    let border = frame_rect(&window, &table_id);
    let on_border = Pos2::new(border.left() + border.width() * 0.3, border.top() + 1.0);
    app.drag(&mut window, &mut host, on_border, on_border + Vec2::new(0.0, 40.0));
    app.idle(&mut window, &mut host, 3);
    let after = table_node(&mut host, &canvas).bounds;
    assert!(window.app.inline_text_edit.is_none(), "a border drag opens no cell");
    assert!(after.y > before.y && after.x == before.x, "the border drag moves the table down: {before:?} -> {after:?}");

    window.app.select_clear();
    app.idle(&mut window, &mut host, 3);
    let border = frame_rect(&window, &table_id);
    app.click(&mut window, &mut host, Pos2::new(border.left() + 1.0, border.center().y));
    assert!(window.app.inline_text_edit.is_none(), "a border click opens no cell");
    assert_eq!(window.app.selected_nodes, vec![table_id], "a border click selects the table");
}

/// The user's report: in the application a render of the page comes back frames after a commit. Each key typed into a
/// cell commits, and until the page's render of that commit came back the cell dropped the next keys (`raw_input_hook`
/// removed them) and hid its editor, so the keyboard went to the board's shortcuts.
#[test]
fn table_cell_keys_typed_while_the_render_of_the_last_key_is_on_its_way_all_arrive_and_escape_selects_the_table() {
    let mut host = Host::start();
    let (canvas, mut window, table) = new_table(&mut host);
    let mut app = Driver::slow();
    app.idle(&mut window, &mut host, 20);
    let cell = table.rows[1].cell_ids[0].clone();
    let at = centre(&window, &cell);
    app.click(&mut window, &mut host, at);
    app.idle(&mut window, &mut host, 10);
    assert_eq!(editing(&window).as_deref(), Some(cell.as_str()), "the click opens the cell; notice: {:?}", window.message());

    for letter in ["a", "b", "c", "d", "e"] {
        app.frame(&mut window, &mut host, vec![Event::Text(letter.into())]);
        app.idle(&mut window, &mut host, 2);
        assert_eq!(window.ctx.memory(|memory| memory.focused()), Some(window.app.text_editor_id), "after {letter}: the cell editor keeps the keyboard");
        assert_eq!(conflict(&window), None, "after {letter}");
    }
    assert_eq!(window.app.text_draft, "abcde", "every key typed is in the editor");
    app.frame(&mut window, &mut host, vec![key(Key::Escape)]);
    app.idle(&mut window, &mut host, 150);
    assert!(window.app.inline_text_edit.is_none(), "Escape ends the edit once the queued keys are stored");
    assert_eq!(cell_text(&mut host, &canvas, &cell), "abcde", "every key typed is stored");
    assert_eq!(window.app.selected_nodes, vec![table_node(&mut host, &canvas).id], "Escape leaves the whole table selected");
}

#[test]
fn table_cell_ime_composition_while_the_render_is_on_its_way_commits_once() {
    let mut host = Host::start();
    let (canvas, mut window, table) = new_table(&mut host);
    let mut app = Driver::slow();
    app.idle(&mut window, &mut host, 20);
    let cell = table.rows[0].cell_ids[1].clone();
    let at = centre(&window, &cell);
    app.click(&mut window, &mut host, at);
    app.idle(&mut window, &mut host, 10);
    app.frame(&mut window, &mut host, vec![Event::Text("a".into())]);
    app.idle(&mut window, &mut host, 2);
    app.frame(&mut window, &mut host, vec![Event::Ime(ImeEvent::Enabled), Event::Ime(ImeEvent::Preedit("ㅎ".into()))]);
    app.frame(&mut window, &mut host, vec![Event::Ime(ImeEvent::Preedit("한".into()))]);
    app.frame(&mut window, &mut host, vec![Event::Ime(ImeEvent::Commit("한".into()))]);
    app.idle(&mut window, &mut host, 60);
    assert_eq!(conflict(&window), None, "after the commit");
    app.frame(&mut window, &mut host, vec![key(Key::Escape)]);
    app.idle(&mut window, &mut host, 60);
    assert_eq!(cell_text(&mut host, &canvas, &cell), "a한", "the text and the committed syllable are stored once");
}

/// A render that never comes back: the cell shows that it waits, asks for the render again after 1.5 s, and after 5 s
/// keeps the queued input as the draft behind the conflict notice.
#[test]
fn table_cell_a_render_that_does_not_come_back_is_asked_again_then_the_input_is_kept_as_a_draft() {
    let mut host = Host::start();
    let (_canvas, mut window, table) = new_table(&mut host);
    let mut app = Driver::slow();
    app.idle(&mut window, &mut host, 20);
    let cell = table.rows[0].cell_ids[0].clone();
    let at = centre(&window, &cell);
    app.click(&mut window, &mut host, at);
    app.idle(&mut window, &mut host, 10);
    assert_eq!(editing(&window).as_deref(), Some(cell.as_str()));

    app.renders_held = true;
    app.frame(&mut window, &mut host, vec![Event::Text("a".into())]);
    app.idle(&mut window, &mut host, 4);
    app.frame(&mut window, &mut host, vec![Event::Text("b".into())]);
    app.idle(&mut window, &mut host, 2);
    let renders = |app: &Driver| app.sent.iter().filter(|method| *method == "render.get").count();
    let asked = renders(&app);
    app.time += 0.5;
    app.idle(&mut window, &mut host, 2);
    assert!(window.app.cell_wait_shown(app.time), "the cell shows that it waits");
    assert_eq!(window.app.text_draft, "ab", "the input is in the editor");
    assert_eq!(conflict(&window), None, "a short wait is no conflict");
    app.time += 1.2;
    app.idle(&mut window, &mut host, 2);
    assert_eq!(renders(&app), asked + 1, "after 1.5 s the render is asked for again");
    app.time += 3.5;
    app.idle(&mut window, &mut host, 2);
    let edit = window.app.inline_text_edit.as_ref().expect("the edit is kept");
    assert!(edit.conflict.as_deref().is_some_and(|message| message.contains("초안")), "after 5 s the input is kept as a draft: {:?}", edit.conflict);
    assert!(edit.retry_allowed, "the queued input can be sent again");
    assert_eq!(window.app.text_draft, "ab", "the draft holds the input");
    assert!(window.message().is_some_and(|message| message.contains("초안")), "a notice says so: {:?}", window.message());
    // Escape is the notice's: it does not clear the selection under the kept draft.
    app.keys(&mut window, &mut host, vec![key(Key::Escape)]);
    assert_eq!(window.app.selected_nodes, vec![cell], "Escape on the notice keeps the cell selected");
}

/// Where text cannot be edited (an AI run on the view) a click on a cell only selects it and a double click says why.
#[test]
fn table_cell_a_cell_that_cannot_open_says_why_on_a_double_click() {
    let mut host = Host::start();
    let (_canvas, mut window, table) = new_table(&mut host);
    let cell = table.rows[0].cell_ids[0].clone();
    window.app.run_context = Some(super::ui_channel::RunContext {
        run_id: "run-1".into(),
        deadline: std::time::Instant::now() + std::time::Duration::from_secs(600),
        command_id: "select.clear".into(),
        actor: "agent".into(),
        allowed_intents: Default::default(),
        await_rpc_reply: false,
        mutation_attempted: false,
        rpc_ticket: None,
        enqueue_error: None,
        expected: None,
        prepared_asset: None,
    });
    let at = centre(&window, &cell);
    window.app.hit_test_page(0, at, 1.0, false, false);
    window.settle(&mut host);
    assert!(window.app.inline_text_edit.is_none(), "a click only selects");
    assert_eq!(window.message(), None, "a click says nothing");
    window.app.hit_test_page(0, at, 1.0, false, true);
    window.settle(&mut host);
    assert!(window.app.inline_text_edit.is_none());
    assert!(window.message().is_some_and(|message| message.contains("AI")), "a double click says why: {:?}", window.message());
    window.app.run_context = None;
}

/// An agent's run on the view: the window edits no text meanwhile.
fn agent_run() -> super::ui_channel::RunContext {
    super::ui_channel::RunContext {
        run_id: "run-2".into(),
        deadline: std::time::Instant::now() + std::time::Duration::from_secs(600),
        command_id: "select.clear".into(),
        actor: "agent".into(),
        allowed_intents: Default::default(),
        await_rpc_reply: false,
        mutation_attempted: false,
        rpc_ticket: None,
        enqueue_error: None,
        expected: None,
        prepared_asset: None,
    }
}

/// Review 2, 1: keys of one frame around a key that leaves the cell. The keys before it are the cell's, the keys after it
/// the next cell's; a second leaving key in the same frame waits for the next frame.
#[test]
fn table_cell_one_frame_of_text_tab_text_puts_each_text_in_its_cell_and_a_second_tab_waits_a_frame() {
    for slow in [false, true] {
        let mut host = Host::start();
        let (canvas, mut window, table) = new_table(&mut host);
        let mut app = if slow { Driver::slow() } else { Driver::at_once() };
        app.idle(&mut window, &mut host, if slow { 20 } else { 1 });
        let cells: Vec<String> = table.rows.iter().flat_map(|row| row.cell_ids.clone()).collect();
        let at = centre(&window, &cells[0]);
        app.click(&mut window, &mut host, at);
        app.idle(&mut window, &mut host, if slow { 10 } else { 1 });
        assert_eq!(editing(&window).as_deref(), Some(cells[0].as_str()), "slow {slow}: the click opens the first cell");
        app.frame(&mut window, &mut host, vec![Event::Text("b".into()), key(Key::Tab), Event::Text("c".into())]);
        app.idle(&mut window, &mut host, if slow { 60 } else { 3 });
        assert_eq!(editing(&window).as_deref(), Some(cells[1].as_str()), "slow {slow}: Tab moves to the second cell; notice: {:?}", window.message());
        app.frame(&mut window, &mut host, vec![key(Key::Tab), key(Key::Tab), Event::Text("x".into())]);
        app.idle(&mut window, &mut host, if slow { 90 } else { 4 });
        assert_eq!(editing(&window).as_deref(), Some(cells[3].as_str()), "slow {slow}: two Tabs move two cells, one a frame");
        app.keys(&mut window, &mut host, vec![key(Key::Escape)]);
        app.idle(&mut window, &mut host, if slow { 60 } else { 2 });
        let stored: Vec<String> = cells.iter().map(|cell| cell_text(&mut host, &canvas, cell)).collect();
        assert_eq!(stored, ["b", "c", "", "x"], "slow {slow}: no key is lost and each lands in its cell");
    }
}

/// Review 2, 2: keys held for a cell that then cannot open are dropped, not given to the board's shortcuts, and a notice
/// says the text was not entered.
#[test]
fn table_cell_keys_held_for_a_cell_that_cannot_open_are_dropped_with_a_notice_not_run_as_shortcuts() {
    let mut host = Host::start();
    let (canvas, mut window, table) = new_table(&mut host);
    let mut app = Driver::slow();
    app.idle(&mut window, &mut host, 20);
    let cells: Vec<String> = table.rows.iter().flat_map(|row| row.cell_ids.clone()).collect();
    let at = centre(&window, &cells[0]);
    app.click(&mut window, &mut host, at);
    app.idle(&mut window, &mut host, 10);
    // The change of "a" is on its way when Tab comes: the hand-over waits for it.
    app.frame(&mut window, &mut host, vec![Event::Text("a".into())]);
    app.frame(&mut window, &mut host, vec![key(Key::Tab), Event::Text("z".into()), key(Key::Delete)]);
    assert!(window.app.inline_text_edit.is_some(), "the first cell's edit is still ending");
    window.app.run_context = Some(agent_run());
    app.idle(&mut window, &mut host, 60);
    window.app.run_context = None;
    app.idle(&mut window, &mut host, 5);
    assert!(window.app.inline_text_edit.is_none(), "the next cell did not open");
    assert!(window.message().is_some_and(|message| message.contains("넣지 않았습니다")), "a notice says the text was not entered: {:?}", window.message());
    assert_eq!(table_of(&mut host, &canvas).rows.len(), 2, "the held Delete did not run on the board");
    assert_eq!(cell_text(&mut host, &canvas, &cells[0]), "a");
    assert_eq!(cell_text(&mut host, &canvas, &cells[1]), "", "the held text went nowhere");
}

/// Review 2, 3 and decision (d): F2 is taken only when it opens a text; a table with no cell to open keeps the rename.
#[test]
fn table_cell_f2_on_a_table_with_no_cell_to_open_stays_the_rename() {
    let mut host = Host::start();
    let (canvas, mut window, table) = new_table(&mut host);
    let mut app = Driver::at_once();
    let cells: Vec<String> = table.rows.iter().flat_map(|row| row.cell_ids.clone()).collect();
    host.apply(&canvas, vec![Operation::SetNodeAppearance { node_ids: cells, image_fit: None, locked: Some(true) }], [Intent::Design].into_iter().collect())
        .expect("the cells are locked");
    window.app.refresh_active();
    window.settle(&mut host);
    let table_id = table_node(&mut host, &canvas).id;
    window.app.set_selected_ids(vec![table_id.clone()]);
    window.app.set_selection(designllm_contracts::Selection { node_ids: vec![table_id], text_range: None });
    app.idle(&mut window, &mut host, 2);
    app.keys(&mut window, &mut host, vec![key(Key::F2)]);
    assert!(window.app.inline_text_edit.is_none(), "no cell opens");
    assert!(window.message().is_some_and(|message| message.starts_with("edit.rename")), "F2 went on to the rename: {:?}", window.message());
}

/// Review 2, 5 and decision (b): the band reaches 4 points outside the table too; a click on it while a cell is edited
/// selects the table and opens no edge cell; Shift adds the table to the selection.
#[test]
fn table_cell_the_border_band_outside_the_table_selects_it_and_ends_an_edit_without_opening_an_edge_cell() {
    let mut host = Host::start();
    let (canvas, mut window, table) = new_table(&mut host);
    let mut app = Driver::at_once();
    let table_id = table_node(&mut host, &canvas).id;
    let border = frame_rect(&window, &table_id);
    app.click(&mut window, &mut host, Pos2::new(border.center().x, border.top() - 3.0));
    assert!(window.app.inline_text_edit.is_none(), "a click outside the table on its band opens no cell");
    assert_eq!(window.app.selected_nodes, vec![table_id.clone()], "the band outside the table selects it");

    let at = centre(&window, &table.rows[0].cell_ids[0]);
    app.click(&mut window, &mut host, at);
    assert!(window.app.inline_text_edit.is_some(), "the cell opens");
    // The band over another edge cell (the edited cell's own edge is its editor's: the caret goes there).
    app.click(&mut window, &mut host, Pos2::new(border.right() - 1.0, border.center().y + 4.0));
    app.idle(&mut window, &mut host, 2);
    assert!(window.app.inline_text_edit.is_none(), "a band click ends the edit and opens no edge cell");
    assert_eq!(window.app.selected_nodes, vec![table_id.clone()], "a band click during an edit selects the table");

    window.app.select_clear();
    app.idle(&mut window, &mut host, 2);
    let shift = Modifiers::SHIFT;
    let on_band = Pos2::new(border.center().x, border.bottom() + 2.0);
    app.frame(&mut window, &mut host, vec![Event::PointerMoved(on_band), Event::PointerButton { pos: on_band, button: egui::PointerButton::Primary, pressed: true, modifiers: shift }]);
    app.frame(&mut window, &mut host, vec![Event::PointerButton { pos: on_band, button: egui::PointerButton::Primary, pressed: false, modifiers: shift }]);
    app.idle(&mut window, &mut host, 2);
    assert_eq!(window.app.selected_nodes, vec![table_id], "Shift on the band adds the table");
}

/// Decision (b): the selected table's handles take the press before its border band.
#[test]
fn table_cell_a_handle_of_the_selected_table_resizes_before_its_border_band_moves() {
    let mut host = Host::start();
    let (canvas, mut window, _table) = new_table(&mut host);
    let mut app = Driver::at_once();
    let table = table_node(&mut host, &canvas);
    window.app.set_selected_ids(vec![table.id.clone()]);
    window.app.set_selection(designllm_contracts::Selection { node_ids: vec![table.id.clone()], text_range: None });
    app.idle(&mut window, &mut host, 3);
    let border = frame_rect(&window, &table.id);
    app.drag(&mut window, &mut host, border.right_bottom(), border.right_bottom() + Vec2::new(30.0, 0.0));
    app.idle(&mut window, &mut host, 3);
    let after = table_node(&mut host, &canvas).bounds;
    assert_eq!((after.x, after.y), (table.bounds.x, table.bounds.y), "the corner keeps the table in place: {:?} -> {after:?}", table.bounds);
    assert!(after.width > table.bounds.width, "the corner handle widens the table: {:?} -> {after:?}", table.bounds);
}
