//! U05 (68 item 11): typing into a table the window has just inserted. The window runs on a real host
//! (`test_support::Window`): the table is made by `canvas.table.add` and the board draws the core's render. Every frame
//! after that is the application's own (`eframe::App::raw_input_hook`, then `eframe::App::update` with a test
//! `eframe::Frame`), so the registry's shortcuts, the event poll and the dialogs run in their real order. `Driver`
//! plays the RPC worker: at once (each frame's requests are answered before the next frame), or with latency (a call
//! comes back a frame later, a render and its preview `RENDER_FRAMES` frames later, as the application's worker
//! answers). Every cell of a new table is empty.

use std::collections::VecDeque;

use designllm_contracts::{ArtifactBody, CanvasTable, FlowBlock, Intent, Node, Operation};
use eframe::egui::{self, Event, ImeEvent, Key, Modifiers, Pos2, Vec2};
use serde_json::json;

use super::test_support::{primary_button, Host, Window};
use crate::grapheme::TextInputEdit;
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

#[test]
fn office_table_cells_accept_input_on_the_page() {
    for kind in ["docx", "hwpx"] {
        let mut host = Host::start();
        let id = host.create(kind, "Office table edit");
        let mut window = Window::open_with_previews(&mut host, &id);
        window.app.screen = super::Screen::Editor;
        window.step(&mut host, "add.table", json!({"rows":1,"columns":2})).unwrap();
        let ArtifactBody::Flow(before) = host.artifact(&id).body else { panic!("office document") };
        let table = before.sections[0].blocks.iter().find_map(|block| match block {
            FlowBlock::Table(table) => Some(table), _ => None,
        }).unwrap();
        let FlowBlock::Paragraph(cell) = &table.rows[0][0].blocks[0] else { panic!("editable paragraph") };
        let cell_id = cell.id.clone();
        window.draw(&mut host, 1.0);
        let point = centre(&window, &cell_id);
        window.click(&mut host, 1.0, point, false);
        assert_eq!(editing(&window).as_deref(), Some(cell_id.as_str()), "{kind}");
        window.frame(&mut host, 1.0, vec![Event::Text("한글".into())]);
        let artifact = host.artifact(&id);
        let updated = super::doc_tree::find_paragraph(&artifact, &cell_id).expect("the original paragraph");
        assert_eq!(super::doc_tree::paragraph_text(updated), "한글", "{kind}");
        let revision = artifact.revision;
        let ArtifactBody::Flow(after) = artifact.body else { panic!("office document") };
        let edited = after.sections[0].blocks.iter().find_map(|block| match block {
            FlowBlock::Table(table) => Some(table), _ => None,
        }).unwrap();
        assert_eq!(edited.column_widths, table.column_widths);
        assert_eq!(edited.rows[0][1].id, table.rows[0][1].id);

        let save = std::env::temp_dir().join(format!("designllm-office-edit-{}.{kind}", designllm_contracts::new_id()));
        host.call("artifact.save", json!({"request_id":designllm_contracts::new_id(), "artifact_id":id,
            "expected_revision":revision, "target_path":save})).expect("edited document saves in its native format");
        let reopened = host.call("artifact.open", json!({"request_id":designllm_contracts::new_id(), "actor":"test", "path":save}));
        std::fs::remove_file(save).expect("remove the saved test file");
        let reopened = reopened.expect("saved document reopens");
        let reopened_id = reopened["id"].as_str().unwrap();
        let ArtifactBody::Flow(saved) = host.artifact(reopened_id).body else { panic!("office document") };
        let saved_cell = saved.sections[0].blocks.iter().find_map(|block| match block {
            FlowBlock::Table(table) => table.rows.first()?.first()?.blocks.first(), _ => None,
        });
        let Some(FlowBlock::Paragraph(saved_cell)) = saved_cell else { panic!("saved paragraph") };
        assert_eq!(super::doc_tree::paragraph_text(saved_cell), "한글");

        let mut window = Window::open_with_previews(&mut host, reopened_id);
        window.app.screen = super::Screen::Editor;
        window.draw(&mut host, 1.0);
        let page = &window.app.page_textures[&0].page;
        let line = page.source_map.iter().find(|region| region.logical_range.as_ref().is_some_and(|range| {
            range.paragraph_id == saved_cell.id && range.end > range.start
        })).expect("rendered text of the saved cell");
        assert_ne!(line.node_id, saved_cell.id, "the glyph's hit names a run");
        let glyph = super::page_to_screen(line.grapheme_rects[0], page.size, window.app.page_rect.unwrap());
        let after_first = Pos2::new(glyph.left() + glyph.width() * 0.8, glyph.center().y);
        window.click(&mut host, 1.0, after_first, false);
        assert_eq!(editing(&window).as_deref(), Some(saved_cell.id.as_str()), "{kind}");
        window.frame(&mut host, 1.0, vec![Event::Text("!".into())]);
        let artifact = host.artifact(reopened_id);
        let updated = super::doc_tree::find_paragraph(&artifact, &saved_cell.id).unwrap();
        assert_eq!(super::doc_tree::paragraph_text(updated), "한!글");
    }
}

#[test]
fn a_paginated_flow_paragraph_keeps_its_full_editor_and_caret() {
    use designllm_contracts::{new_id, Paragraph, TextRun};
    let mut host = Host::start();
    let id = host.create("docx", "Paginated paragraph");
    let ArtifactBody::Flow(doc) = host.artifact(&id).body else { panic!("office document") };
    let paragraph_id = new_id();
    let original = "Line\n".repeat(95) + "End";
    host.apply(&id, vec![Operation::InsertFlowBlock {
        section_id: doc.sections[0].id.clone(), index: 0,
        block: FlowBlock::Paragraph(Paragraph { id: paragraph_id.clone(), style_ref: None, format: Default::default(),
            runs: vec![TextRun::new(new_id(), original.clone())] }),
    }], [Intent::Content].into_iter().collect()).unwrap();

    let mut window = Window::open_with_previews(&mut host, &id);
    window.app.screen = super::Screen::Editor;
    window.draw(&mut host, 1.0);
    assert!(window.app.begin_inline_text_edit(&window.ctx, &paragraph_id, 0,
        Some(designllm_contracts::TextPosition { paragraph_id: paragraph_id.clone(), grapheme_index: 4, trailing: true })));
    assert!(window.app.flow_uses_paragraph_editor());
    let mut driver = Driver::slow();
    driver.renders_held = true;
    driver.frame(&mut window, &mut host, vec![Event::Text("!".into())]);
    driver.idle(&mut window, &mut host, 4);
    let revision = window.app.inline_text_edit.as_ref().unwrap().revision;
    assert!(window.app.page_textures[&0].revision < revision, "the edit is acknowledged before its render");
    assert!(window.app.flow_uses_paragraph_editor(), "the editor stays open while rendering");
    let artifact = host.artifact(&id);
    let result = super::doc_tree::paragraph_text(super::doc_tree::find_paragraph(&artifact, &paragraph_id).unwrap());
    assert_eq!(result, "Line!".to_owned() + &original[4..]);
}

#[test]
fn modifier_clicks_select_separate_runs_in_one_flow_paragraph() {
    use designllm_contracts::{new_id, Paragraph, TextRun};
    let mut host = Host::start();
    let id = host.create("docx", "Separate styled runs");
    let ArtifactBody::Flow(doc) = host.artifact(&id).body else { panic!("office document") };
    let plain = TextRun::new(new_id(), "Plain ");
    let mut bold = TextRun::new(new_id(), "Bold");
    bold.style.bold = true;
    let run_ids = [plain.id.clone(), bold.id.clone()];
    host.apply(&id, vec![Operation::InsertFlowBlock {
        section_id: doc.sections[0].id.clone(), index: 0,
        block: FlowBlock::Paragraph(Paragraph { id: new_id(), style_ref: None, format: Default::default(), runs: vec![plain, bold] }),
    }], [Intent::Content].into_iter().collect()).unwrap();

    let mut window = Window::open_with_previews(&mut host, &id);
    window.app.screen = super::Screen::Editor;
    let mut driver = Driver::at_once();
    driver.idle(&mut window, &mut host, 3);
    let page = &window.app.page_textures[&0].page;
    let page_rect = window.app.page_rect.unwrap();
    let points = run_ids.each_ref().map(|run| {
        let region = page.source_map.iter().find(|region| &region.node_id == run).unwrap();
        super::page_to_screen(region.bounds, page.size, page_rect).center()
    });
    driver.modifiers = Modifiers::SHIFT;
    for point in points { driver.click(&mut window, &mut host, point); }
    assert_eq!(window.app.selected_nodes, run_ids);
    assert!(editing(&window).is_none());
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
    /// The keyboard modifiers each frame's input carries (egui reads Shift from here, not from the pointer event).
    modifiers: Modifiers,
}

impl Driver {
    fn at_once() -> Self {
        Self { latency: None, call_frames: 1, renders_held: false, frame: 0, time: 100.0, sent: Vec::new(), modifiers: Modifiers::NONE }
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
        input.modifiers = self.modifiers;
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
                Work::Call { ticket, method, params, .. } => {
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
    // The arrows are the cell editor's: they never move the table (68 item 15).
    let table_bounds = table_node(&mut host, &canvas).bounds;

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
    assert_eq!(table_node(&mut host, &canvas).bounds, table_bounds, "no arrow moved the table");
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

/// 68c: a composition whose `Enabled` the cell's editor did not see (only `Preedit` and `Commit` reach it, in every cell
/// after the first) opens at its first preedit: no conflict, no lost or doubled text.
#[test]
fn table_cell_a_composition_without_enabled_in_each_next_cell_commits_without_a_conflict() {
    for slow in [false, true] {
        let mut host = Host::start();
        let (canvas, mut window, table) = new_table(&mut host);
        let mut app = if slow { Driver::slow() } else { Driver::at_once() };
        app.idle(&mut window, &mut host, if slow { 20 } else { 1 });
        let cells: Vec<String> = table.rows.iter().flat_map(|row| row.cell_ids.clone()).collect();

        let at = centre(&window, &cells[0]);
        app.click(&mut window, &mut host, at);
        idle_until(&mut app, &mut window, &mut host, 30, &format!("slow {slow}: the click opens the first cell"), |window| editing(window).as_deref() == Some(cells[0].as_str()));
        app.frame(&mut window, &mut host, vec![Event::Ime(ImeEvent::Enabled), Event::Ime(ImeEvent::Preedit("ㅎ".into()))]);
        app.frame(&mut window, &mut host, vec![Event::Ime(ImeEvent::Preedit("한".into()))]);
        app.keys(&mut window, &mut host, vec![Event::Ime(ImeEvent::Commit("한".into()))]);
        assert_eq!(conflict(&window), None, "slow {slow}: the first commit: notice {:?}", window.message());

        let syllables = ["글", "자", "다"];
        for (index, syllable) in syllables.into_iter().enumerate() {
            app.keys(&mut window, &mut host, vec![key(Key::Tab)]);
            let next = cells[index + 1].as_str();
            idle_until(&mut app, &mut window, &mut host, 120, &format!("slow {slow}: Tab moves to cell {}", index + 1), |window| editing(window).as_deref() == Some(next));
            // No `Enabled` here.
            app.frame(&mut window, &mut host, vec![Event::Ime(ImeEvent::Preedit("ㅎ".into()))]);
            app.frame(&mut window, &mut host, vec![Event::Ime(ImeEvent::Preedit(syllable.into()))]);
            app.keys(&mut window, &mut host, vec![Event::Ime(ImeEvent::Commit(syllable.into()))]);
            assert_eq!(conflict(&window), None, "slow {slow}: the commit in cell {}: notice {:?}", index + 1, window.message());
        }
        app.keys(&mut window, &mut host, vec![key(Key::Escape)]);
        idle_until(&mut app, &mut window, &mut host, 200, &format!("slow {slow}: Escape ends the edit"), |window| window.app.inline_text_edit.is_none());
        let stored: Vec<String> = cells.iter().map(|cell| cell_text(&mut host, &canvas, cell)).collect();
        assert_eq!(stored, ["한", "글", "자", "다"], "slow {slow}: each cell keeps its committed syllable once");
    }
}

/// 68c: a commit of a composition this edit did not open (neither its open one nor one opened this frame) is kept as a
/// conflict and applied to nothing.
#[test]
fn an_ime_commit_begun_in_another_session_is_kept_as_a_conflict_and_applies_nothing() {
    let mut host = Host::start();
    let (canvas, mut window, table) = new_table(&mut host);
    let mut app = Driver::at_once();
    let cell = table.rows[0].cell_ids[0].clone();
    let at = centre(&window, &cell);
    app.click(&mut window, &mut host, at);
    assert_eq!(editing(&window).as_deref(), Some(cell.as_str()), "the click opens the cell; notice: {:?}", window.message());
    let stale = TextInputEdit {
        request_id: String::new(), range: 0..0, paragraph_index: 0, paragraph_range: Some(0..0), caret_after: 1,
        paragraph_after: 0, caret_after_in_paragraph: 1, text: "x".into(), ime_commit: true, ime_generation: 7, after_text: None,
    };
    window.app.queue_inline_text_edits(vec![stale], 0..0);
    assert_eq!(conflict(&window).as_deref(), Some("IME 조합 상태가 바뀌었습니다. 입력을 유지했습니다"), "the stale commit is refused");
    app.idle(&mut window, &mut host, 4);
    assert_eq!(cell_text(&mut host, &canvas, &cell), "", "the stale commit is not applied");
}

/// Two empty text nodes, `text-a` and `text-b`, under the table, drawn.
fn add_two_texts(host: &mut Host, window: &mut Window, app: &mut Driver, canvas: &str) {
    let page_id = match &host.artifact(canvas).body { ArtifactBody::Canvas(document) => document.pages[0].id.clone(), _ => unreachable!() };
    for (id, y) in [("text-a", 3_000_000), ("text-b", 4_000_000)] {
        let node: Node = serde_json::from_value(json!({
            "id": id, "kind": "text", "bounds": { "x": 100_000, "y": y, "width": 900_000, "height": 600_000 },
        })).expect("a text node");
        host.apply(canvas, vec![Operation::AddNode { page_id: page_id.clone(), parent_id: None, node }], [Intent::Content, Intent::Design].into_iter().collect())
            .expect("the text node is added");
    }
    window.app.refresh_active();
    window.settle(host);
    window.draw(host, 1.0);
    app.idle(window, host, 2);
}

/// Selects `node` the way the layer list does, then Enter opens its text.
fn open_text(window: &mut Window, app: &mut Driver, host: &mut Host, node: &str) {
    window.app.set_selected_ids(vec![node.to_owned()]);
    window.app.set_selection(designllm_contracts::Selection { node_ids: vec![node.to_owned()], text_range: None });
    app.idle(window, host, 2);
    app.keys(window, host, vec![key(Key::Enter)]);
}

/// 68c: a text node edited after another, its composition's `Enabled` not seen by the editor.
#[test]
fn text_node_ime_after_another_edit_commits_without_a_conflict_and_without_enabled() {
    let mut host = Host::start();
    let (canvas, mut window, _) = new_table(&mut host);
    let mut app = Driver::at_once();
    add_two_texts(&mut host, &mut window, &mut app, &canvas);

    open_text(&mut window, &mut app, &mut host, "text-a");
    idle_until(&mut app, &mut window, &mut host, 30, "Enter opens the first text", |window| editing(window).as_deref() == Some("text-a"));
    app.frame(&mut window, &mut host, vec![Event::Ime(ImeEvent::Enabled), Event::Ime(ImeEvent::Preedit("ㅎ".into()))]);
    app.frame(&mut window, &mut host, vec![Event::Ime(ImeEvent::Preedit("한".into()))]);
    app.keys(&mut window, &mut host, vec![Event::Ime(ImeEvent::Commit("한".into()))]);
    assert_eq!(conflict(&window), None, "the first commit: notice {:?}", window.message());
    app.keys(&mut window, &mut host, vec![key(Key::Escape)]);
    idle_until(&mut app, &mut window, &mut host, 100, "Escape ends the first text", |window| window.app.inline_text_edit.is_none());

    open_text(&mut window, &mut app, &mut host, "text-b");
    idle_until(&mut app, &mut window, &mut host, 30, "Enter opens the second text", |window| editing(window).as_deref() == Some("text-b"));
    // No `Enabled`.
    app.frame(&mut window, &mut host, vec![Event::Ime(ImeEvent::Preedit("ㅎ".into()))]);
    app.frame(&mut window, &mut host, vec![Event::Ime(ImeEvent::Preedit("글".into()))]);
    app.keys(&mut window, &mut host, vec![Event::Ime(ImeEvent::Commit("글".into()))]);
    assert_eq!(conflict(&window), None, "the commit in the second text: notice {:?}", window.message());
    app.keys(&mut window, &mut host, vec![key(Key::Escape)]);
    idle_until(&mut app, &mut window, &mut host, 100, "Escape ends the second text", |window| window.app.inline_text_edit.is_none());
    assert_eq!(cell_text(&mut host, &canvas, "text-a"), "한", "the first text keeps its syllable once");
    assert_eq!(cell_text(&mut host, &canvas, "text-b"), "글", "the second text keeps its syllable once");
}

/// The egui events a Windows Korean IME gives the window: the window messages as winit 0.30.13 sends them
/// (`platform_impl/windows/event_loop.rs`, `WM_IME_*`) and egui-winit 0.33.3 turns them into egui events. Its
/// `Preedit("", None)` is egui's `Disabled`, its `Commit` is followed by `Disabled`, and its first `Preedit` after a
/// `Disabled` is preceded by `Enabled`. Every composition has its `Enabled`.
mod windows_ime {
    use eframe::egui::{Event, ImeEvent};

    fn ime(event: ImeEvent) -> Event {
        Event::Ime(event)
    }

    /// The frames of typing `syllables` (each the preedits of one syllable, its last the syllable), the last one still
    /// composed. `restarted`: the IME ends the composition between syllables and starts the next one.
    pub(super) fn typing(syllables: &[&[&str]], restarted: bool) -> Vec<Vec<Event>> {
        let mut frames = Vec::new();
        let mut done: Option<&str> = None;
        for syllable in syllables {
            let (first, rest) = syllable.split_first().expect("a syllable has a preedit");
            frames.push(match done {
                // WM_IME_STARTCOMPOSITION, WM_IME_COMPOSITION (GCS_COMPSTR).
                None => vec![ime(ImeEvent::Enabled), ime(ImeEvent::Preedit((*first).into()))],
                // One WM_IME_COMPOSITION (GCS_RESULTSTR | GCS_COMPSTR); restarted: WM_IME_COMPOSITION (GCS_RESULTSTR),
                // WM_IME_ENDCOMPOSITION, WM_IME_STARTCOMPOSITION, WM_IME_COMPOSITION (GCS_COMPSTR).
                Some(done) => {
                    let mut frame = vec![ime(ImeEvent::Disabled), ime(ImeEvent::Commit(done.into())), ime(ImeEvent::Disabled)];
                    if restarted {
                        frame.push(ime(ImeEvent::Disabled));
                    }
                    frame.extend([ime(ImeEvent::Enabled), ime(ImeEvent::Preedit((*first).into()))]);
                    frame
                }
            });
            frames.extend(rest.iter().map(|preedit| vec![ime(ImeEvent::Preedit((*preedit).into()))]));
            done = syllable.last().copied();
        }
        frames
    }

    /// A key that ends the composition of `syllable` (Tab, Escape, an arrow): WM_IME_COMPOSITION (GCS_RESULTSTR), then
    /// WM_IME_ENDCOMPOSITION.
    pub(super) fn end(syllable: &str) -> Vec<Event> {
        vec![ime(ImeEvent::Disabled), ime(ImeEvent::Commit(syllable.into())), ime(ImeEvent::Disabled), ime(ImeEvent::Disabled)]
    }
}

/// Plays `frames`, one frame each, and checks after each that the edit shows no conflict.
fn play(app: &mut Driver, window: &mut Window, host: &mut Host, frames: Vec<Vec<Event>>, what: &str) {
    for (index, events) in frames.into_iter().enumerate() {
        app.frame(window, host, events);
        assert_eq!(conflict(window), None, "{what}, frame {index}: notice {:?}", window.message());
    }
}

/// Idles until `done` holds, checking every frame that no edit shows a conflict.
fn idle_until(app: &mut Driver, window: &mut Window, host: &mut Host, frames: u32, what: &str, done: impl Fn(&Window) -> bool) {
    for _ in 0..frames {
        if done(window) {
            return;
        }
        app.frame(window, host, Vec::new());
        assert_eq!(conflict(window), None, "{what}: notice {:?}", window.message());
    }
    assert!(done(window), "{what}: not done after {frames} frames; editing {:?}, notice {:?}", editing(window), window.message());
}

/// 68c: typing Korean into the cells of a table as Windows delivers it: each syllable's commit comes in one frame with the
/// next syllable's `Enabled` and preedit, and the commit of the last syllable in one frame with the Tab (or Escape) that
/// ends it. One cell gets its whole typing in one frame (the window busy while the render was on its way). No conflict at
/// any frame, every syllable stored once, and Tab moves on.
#[test]
fn table_cell_windows_korean_ime_order_types_each_cell_and_tab_moves_on_without_a_conflict() {
    let words: [&[&[&str]]; 4] = [&[&["ㅎ", "하", "한"], &["ㄱ", "그", "글"]], &[&["ㅈ", "자"]], &[&["ㄷ", "다"], &["ㄹ", "라"]], &[&["ㅁ", "마"]]];
    for slow in [false, true] {
        for restarted in [false, true] {
            let case = format!("slow {slow}, restarted {restarted}");
            let mut host = Host::start();
            let (canvas, mut window, table) = new_table(&mut host);
            let mut app = if slow { Driver::slow() } else { Driver::at_once() };
            app.idle(&mut window, &mut host, if slow { 20 } else { 1 });
            let cells: Vec<String> = table.rows.iter().flat_map(|row| row.cell_ids.clone()).collect();
            let at = centre(&window, &cells[0]);
            app.click(&mut window, &mut host, at);
            idle_until(&mut app, &mut window, &mut host, 30, &format!("{case}: the click opens cell 0"), |window| editing(window).as_deref() == Some(cells[0].as_str()));

            for (index, word) in words.iter().enumerate() {
                let last = word.last().and_then(|syllable| syllable.last()).copied().expect("a syllable");
                let leave = if index + 1 < words.len() { key(Key::Tab) } else { key(Key::Escape) };
                let mut frames = windows_ime::typing(word, restarted);
                let mut end = windows_ime::end(last);
                end.push(leave);
                frames.push(end);
                // Cell 2 gets its whole typing in one frame.
                if index == 2 {
                    frames = vec![frames.concat()];
                }
                play(&mut app, &mut window, &mut host, frames, &format!("{case}: cell {index}"));
                if index + 1 < words.len() {
                    let next = cells[index + 1].as_str();
                    idle_until(&mut app, &mut window, &mut host, 120, &format!("{case}: Tab after cell {index}"), |window| editing(window).as_deref() == Some(next));
                }
            }
            idle_until(&mut app, &mut window, &mut host, 200, &format!("{case}: Escape ends the edit"), |window| window.app.inline_text_edit.is_none());
            let stored: Vec<String> = cells.iter().map(|cell| cell_text(&mut host, &canvas, cell)).collect();
            assert_eq!(stored, ["한글", "자", "다라", "마"], "{case}: each cell keeps what was typed in it, once");
        }
    }
}

/// 68c: one frame that holds a commit and the start of the next composition, in the orders the IME and the UI test input
/// can give: the commit is the composition's that made it, the next composition only the later commits'. No conflict, the
/// next preedit is shown, every syllable stored once.
#[test]
fn table_cell_a_commit_and_the_next_composition_in_one_frame_raise_no_conflict() {
    let ime = Event::Ime;
    let orders: [Vec<Event>; 3] = [
        vec![ime(ImeEvent::Commit("한".into())), ime(ImeEvent::Preedit("ㄱ".into()))],
        vec![ime(ImeEvent::Commit("한".into())), ime(ImeEvent::Disabled), ime(ImeEvent::Enabled), ime(ImeEvent::Preedit("ㄱ".into()))],
        vec![ime(ImeEvent::Disabled), ime(ImeEvent::Commit("한".into())), ime(ImeEvent::Disabled), ime(ImeEvent::Enabled), ime(ImeEvent::Preedit("ㄱ".into()))],
    ];
    for slow in [false, true] {
        for (order, frame) in orders.iter().enumerate() {
            let case = format!("slow {slow}, order {order}");
            let mut host = Host::start();
            let (canvas, mut window, table) = new_table(&mut host);
            let mut app = if slow { Driver::slow() } else { Driver::at_once() };
            app.idle(&mut window, &mut host, if slow { 20 } else { 1 });
            let cell = table.rows[0].cell_ids[0].clone();
            let at = centre(&window, &cell);
            app.click(&mut window, &mut host, at);
            idle_until(&mut app, &mut window, &mut host, 30, &format!("{case}: the click opens the cell"), |window| editing(window).as_deref() == Some(cell.as_str()));
            play(&mut app, &mut window, &mut host, vec![
                vec![ime(ImeEvent::Enabled), ime(ImeEvent::Preedit("ㅎ".into()))],
                vec![ime(ImeEvent::Preedit("한".into()))],
                frame.clone(),
            ], &case);
            assert_eq!(window.app.text_draft, "한ㄱ", "{case}: the commit and the next preedit are shown");
            play(&mut app, &mut window, &mut host, vec![vec![ime(ImeEvent::Preedit("글".into()))], vec![ime(ImeEvent::Commit("글".into()))]], &case);
            app.idle(&mut window, &mut host, 2);
            play(&mut app, &mut window, &mut host, vec![vec![key(Key::Escape)]], &case);
            idle_until(&mut app, &mut window, &mut host, 200, &format!("{case}: Escape ends the edit"), |window| window.app.inline_text_edit.is_none());
            assert_eq!(cell_text(&mut host, &canvas, &cell), "한글", "{case}: both syllables stored once");
        }
    }
}

/// 68c: a text node typed in as Windows delivers it, then a second text node: no conflict, each syllable stored once.
#[test]
fn text_node_windows_korean_ime_order_commits_without_a_conflict() {
    let mut host = Host::start();
    let (canvas, mut window, _) = new_table(&mut host);
    let mut app = Driver::at_once();
    add_two_texts(&mut host, &mut window, &mut app, &canvas);
    for (node, word, stored) in [
        ("text-a", &[&["ㅎ", "하", "한"][..], &["ㄱ", "그", "글"][..]][..], "한글"),
        ("text-b", &[&["ㅈ", "자"][..]][..], "자"),
    ] {
        open_text(&mut window, &mut app, &mut host, node);
        idle_until(&mut app, &mut window, &mut host, 30, &format!("Enter opens {node}"), |window| editing(window).as_deref() == Some(node));
        let last = word.last().and_then(|syllable| syllable.last()).copied().expect("a syllable");
        let mut frames = windows_ime::typing(word, false);
        frames.push(windows_ime::end(last));
        play(&mut app, &mut window, &mut host, frames, node);
        app.idle(&mut window, &mut host, 2);
        play(&mut app, &mut window, &mut host, vec![vec![key(Key::Escape)]], node);
        idle_until(&mut app, &mut window, &mut host, 100, &format!("Escape ends {node}"), |window| window.app.inline_text_edit.is_none());
        assert_eq!(cell_text(&mut host, &canvas, node), stored, "{node} keeps what was typed, once");
    }
}

/// 68p 2: a Tab in one frame with a preedit ([Preedit, Tab]), the composition still open after it, while the cell waits for
/// the render of its last change (slow) or not (at once), the composition opened in an earlier frame or in that one. The
/// Tab is not lost: once the composition is committed it moves to the next cell, once, and the syllable stays in the cell
/// it was typed in.
#[test]
fn table_cell_a_tab_in_a_frame_with_a_preedit_moves_on_once_the_composition_is_committed() {
    for slow in [true, false] {
        for opened_before in [true, false] {
            let case = format!("slow {slow}, composition opened before the Tab's frame {opened_before}");
            let mut host = Host::start();
            let (canvas, mut window, table) = new_table(&mut host);
            let mut app = if slow { Driver::slow() } else { Driver::at_once() };
            app.idle(&mut window, &mut host, if slow { 20 } else { 1 });
            let cells: Vec<String> = table.rows.iter().flat_map(|row| row.cell_ids.clone()).collect();
            let at = centre(&window, &cells[0]);
            app.click(&mut window, &mut host, at);
            idle_until(&mut app, &mut window, &mut host, 30, &format!("{case}: the click opens cell 0"), |window| editing(window).as_deref() == Some(cells[0].as_str()));
            app.frame(&mut window, &mut host, vec![Event::Text("a".into())]);
            app.idle(&mut window, &mut host, 2);
            if opened_before {
                app.frame(&mut window, &mut host, vec![Event::Ime(ImeEvent::Enabled), Event::Ime(ImeEvent::Preedit("ㅎ".into()))]);
            }
            let readiness = window.app.inline_text_edit.as_ref().map(|session| window.app.inline_table_cell_readiness(session));
            let expected = if slow { super::InlineCellReadiness::WaitingForRender } else { super::InlineCellReadiness::Ready };
            assert_eq!(readiness, Some(expected), "{case}: the cell's state before the Tab's frame");
            play(&mut app, &mut window, &mut host, vec![vec![Event::Ime(ImeEvent::Preedit("한".into())), key(Key::Tab)]], &case);
            assert_eq!(editing(&window).as_deref(), Some(cells[0].as_str()), "{case}: the Tab waits for the open composition");
            let shown = window.app.text_draft.clone();
            play(&mut app, &mut window, &mut host, vec![windows_ime::end("한")], &case);
            idle_until(&mut app, &mut window, &mut host, 120, &format!("{case}: the Tab moves to cell 1 after the commit"), |window| editing(window).as_deref() == Some(cells[1].as_str()));
            app.idle(&mut window, &mut host, 10);
            assert_eq!(editing(&window).as_deref(), Some(cells[1].as_str()), "{case}: the Tab moves one cell only");
            app.keys(&mut window, &mut host, vec![key(Key::Escape)]);
            idle_until(&mut app, &mut window, &mut host, 200, &format!("{case}: Escape ends the edit"), |window| window.app.inline_text_edit.is_none());
            let stored: Vec<String> = cells.iter().map(|cell| cell_text(&mut host, &canvas, cell)).collect();
            assert_eq!(stored, ["a한", "", "", ""], "{case}: the syllable is stored once, in the cell it was typed in");
            assert_eq!(shown, "a한", "{case}: the preedit was shown in cell 0 while the Tab waited");
        }
    }
}

/// 68p 8b: a Tab kept through a composition that then closes outside the frame's input (the editor's IME state reset, as a
/// failed change or the close cleanup does) leaves the cell of its own edit in that frame: it is not handed to the next
/// frame, where the edit may have ended and the board would take it (`select.next`).
#[test]
fn table_cell_a_tab_kept_through_a_composition_closed_elsewhere_moves_its_own_edit_and_reaches_nothing_else() {
    let mut host = Host::start();
    let (_canvas, mut window, table) = new_table(&mut host);
    let mut app = Driver::at_once();
    let cells: Vec<String> = table.rows.iter().flat_map(|row| row.cell_ids.clone()).collect();
    let at = centre(&window, &cells[0]);
    app.click(&mut window, &mut host, at);
    idle_until(&mut app, &mut window, &mut host, 30, "the click opens cell 0", |window| editing(window).as_deref() == Some(cells[0].as_str()));
    app.frame(&mut window, &mut host, vec![Event::Ime(ImeEvent::Enabled), Event::Ime(ImeEvent::Preedit("ㅎ".into()))]);
    app.frame(&mut window, &mut host, vec![Event::Ime(ImeEvent::Preedit("한".into())), key(Key::Tab)]);
    assert_eq!(editing(&window).as_deref(), Some(cells[0].as_str()), "the Tab waits for the open composition");
    window.app.text_adapter = crate::grapheme::GraphemeAdapter::default();
    if let Some(session) = window.app.inline_text_edit.as_mut() {
        session.composing = false;
    }
    app.frame(&mut window, &mut host, Vec::new());
    let leaving = window.app.inline_text_edit.as_ref().is_none_or(|session| session.finish_requested);
    assert!(leaving, "the kept Tab ends cell 0's edit in the frame it is let go in, not in a later frame's input");
    idle_until(&mut app, &mut window, &mut host, 60, "the kept Tab moves to cell 1", |window| editing(window).as_deref() == Some(cells[1].as_str()));
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

/// A click with Shift held (the press and the release carry the modifier).
fn shift_click(app: &mut Driver, window: &mut Window, host: &mut Host, cell: &str) {
    let at = centre(window, cell);
    let shift = Modifiers::SHIFT;
    app.modifiers = shift;
    app.frame(window, host, vec![Event::PointerMoved(at), Event::PointerButton { pos: at, button: egui::PointerButton::Primary, pressed: true, modifiers: shift }]);
    app.frame(window, host, vec![Event::PointerButton { pos: at, button: egui::PointerButton::Primary, pressed: false, modifiers: shift }]);
    app.modifiers = Modifiers::NONE;
    app.idle(window, host, 3);
    app.time += 0.5;
}

/// A new table's first row is a header, and a header cell cannot span into the body: the merges here turn it off first.
fn header_off(window: &mut Window, host: &mut Host, canvas: &str) {
    let id = table_node(host, canvas).id;
    window.app.invoke("canvas.table.set_header", json!({ "table_id": id, "enabled": false })).expect("the header is off");
    window.settle(host);
    window.draw(host, 1.0);
}

#[test]
fn table_merge_shift_clicked_cells_merge_types_into_the_merged_cell_and_unmerge_and_undo_step_back() {
    let mut host = Host::start();
    let (canvas, mut window, table) = new_table(&mut host);
    header_off(&mut window, &mut host, &canvas);
    let mut app = Driver::at_once();
    let (c10, c11) = (table.rows[1].cell_ids[0].clone(), table.rows[1].cell_ids[1].clone());

    // The first click opens c10; Shift+click on c11 adds it to the selection (the edit ends on the first). The header row
    // keeps its own cell style, so a merge over it is refused (TABLE_SPAN_HIDDEN_DATA); the body row is merged.
    let at = centre(&window, &c10);
    app.click(&mut window, &mut host, at);
    assert_eq!(editing(&window).as_deref(), Some(c10.as_str()));
    shift_click(&mut app, &mut window, &mut host, &c11);
    let mut selected = window.app.selected_nodes.clone();
    selected.sort();
    let mut expected = vec![c10.clone(), c11.clone()];
    expected.sort();
    assert_eq!(selected, expected, "Shift+click adds each cell to the selection; notice: {:?}", window.message());

    // 셀 병합 (the panel and the menu run this): one merged 2 × 2 cell anchored at c00.
    let action = window.app.canvas_table_span_command(false);
    assert!(action.is_ok(), "the 2 × 2 selection is a merge: {action:?}");
    window.app.run_span_action(action);
    app.idle(&mut window, &mut host, 4);
    let spans = table_of(&mut host, &canvas).cell_spans;
    assert_eq!(spans.len(), 1, "one merged region");
    assert_eq!(spans.get(&c10), Some(&designllm_contracts::CanvasTableSpan { row_span: 1, column_span: 2 }));

    // Typing goes into the merged cell through its anchor.
    let at = centre(&window, &c10);
    app.click(&mut window, &mut host, at);
    assert_eq!(editing(&window).as_deref(), Some(c10.as_str()), "the merged cell opens at its anchor");
    app.keys(&mut window, &mut host, vec![Event::Text("가나".into())]);
    app.keys(&mut window, &mut host, vec![key(Key::Escape)]);
    app.idle(&mut window, &mut host, 4);
    assert_eq!(cell_text(&mut host, &canvas, &c10), "가나", "the typed text is stored in the merged cell");

    // 병합 해제 by a cell of the region, then one undo brings the merge back.
    let table_id = table_node(&mut host, &canvas).id;
    window.app.invoke("table.unmerge_cell", json!({ "table_id": table_id, "row": 1, "column": 1 })).expect("the region unmerges");
    app.idle(&mut window, &mut host, 4);
    assert!(table_of(&mut host, &canvas).cell_spans.is_empty(), "unmerge removes the span");
    window.app.invoke("edit.undo", json!({})).expect("undo the unmerge");
    app.idle(&mut window, &mut host, 4);
    assert_eq!(table_of(&mut host, &canvas).cell_spans.len(), 1, "undo restores the merged region");
}

#[test]
fn table_merge_a_diagonal_selection_and_a_single_cell_are_refused_with_a_reason() {
    let mut host = Host::start();
    let (canvas, mut window, table) = new_table(&mut host);
    header_off(&mut window, &mut host, &canvas);
    let mut app = Driver::at_once();
    let (c00, c01, c10, c11) = (
        table.rows[0].cell_ids[0].clone(),
        table.rows[0].cell_ids[1].clone(),
        table.rows[1].cell_ids[0].clone(),
        table.rows[1].cell_ids[1].clone(),
    );
    let at = centre(&window, &c00);
    app.click(&mut window, &mut host, at);
    shift_click(&mut app, &mut window, &mut host, &c11);
    let diagonal = window.app.canvas_table_span_command(false);
    assert!(diagonal.as_ref().err().is_some_and(|message| message.contains("직사각형")), "diagonal: {diagonal:?}");
    assert!(window.app.canvas_table_span_operation().is_none(), "the planner refuses it too");

    shift_click(&mut app, &mut window, &mut host, &c01);
    shift_click(&mut app, &mut window, &mut host, &c10);
    assert!(window.app.canvas_table_span_operation().is_some_and(|(unmerge, _)| !unmerge), "all four make a 2 × 2 merge");
    // The first row keeps the header's cell style: the check the command runs refuses it before it is offered.
    let rectangle = window.app.canvas_table_span_command(false);
    assert!(rectangle.as_ref().err().is_some_and(|message| message.contains("서식")), "header style under the merge: {rectangle:?}");
    // A single selected cell cannot merge.
    window.app.set_selected_ids(vec![c00.clone()]);
    let single = window.app.canvas_table_span_command(false);
    assert!(single.as_ref().err().is_some_and(|message| message.contains("두 칸")), "single: {single:?}");
}

#[test]
fn table_merge_moves_the_covered_text_to_the_anchor_and_one_undo_restores_each_merge() {
    let mut host = Host::start();
    let (canvas, mut window, _table) = new_table(&mut host);
    header_off(&mut window, &mut host, &canvas);
    // The first row keeps the header's cell style, so the merges use the body rows, which share one style: a third body row.
    let table_id = table_node(&mut host, &canvas).id;
    window.app.invoke("canvas.table.add_row", json!({ "table_id": table_id, "index": 2 })).expect("a third row is added");
    window.settle(&mut host);
    window.draw(&mut host, 1.0);
    let mut app = Driver::at_once();
    let ids: Vec<Vec<String>> = table_of(&mut host, &canvas).rows.iter().map(|row| row.cell_ids.clone()).collect();
    for (cell, text) in [(&ids[1][0], "가"), (&ids[1][1], "나"), (&ids[2][0], "다"), (&ids[2][1], "라")] {
        set_cell_text(&mut host, &mut window, &canvas, cell, text);
    }
    app.idle(&mut window, &mut host, 2);

    // 2 × 2 over the body rows: the four texts become one anchor text, one per line; the others are emptied.
    window.app.set_selected_ids(vec![ids[1][0].clone(), ids[1][1].clone(), ids[2][0].clone(), ids[2][1].clone()]);
    window.app.run_span_action(window.app.canvas_table_span_command(false));
    app.idle(&mut window, &mut host, 4);
    assert_eq!(table_of(&mut host, &canvas).cell_spans.get(&ids[1][0]), Some(&designllm_contracts::CanvasTableSpan { row_span: 2, column_span: 2 }), "the 2 × 2 merge is made; notice: {:?}", window.message());
    assert_eq!(cell_text(&mut host, &canvas, &ids[1][0]), "가\n나\n다\n라", "the texts join the anchor");
    assert_eq!(cell_text(&mut host, &canvas, &ids[1][1]), "", "the covered cells are emptied");
    window.app.invoke("edit.undo", json!({})).expect("undo the 2 × 2 merge");
    app.idle(&mut window, &mut host, 4);
    assert!(table_of(&mut host, &canvas).cell_spans.is_empty(), "one undo removes the merge");
    assert_eq!(cell_text(&mut host, &canvas, &ids[1][0]), "가", "one undo restores the anchor text");
    assert_eq!(cell_text(&mut host, &canvas, &ids[2][1]), "라", "one undo restores the covered texts");

    // 1 × 2 on the last row: the two texts join the first cell, and one undo restores both.
    window.app.set_selected_ids(vec![ids[2][0].clone(), ids[2][1].clone()]);
    window.app.run_span_action(window.app.canvas_table_span_command(false));
    app.idle(&mut window, &mut host, 4);
    assert_eq!(cell_text(&mut host, &canvas, &ids[2][0]), "다\n라", "the 1 × 2 merge joins the row's texts");
    assert_eq!(cell_text(&mut host, &canvas, &ids[2][1]), "");
    window.app.invoke("table.unmerge_cell", json!({ "table_id": table_id, "row": 2, "column": 1 })).expect("unmerge");
    app.idle(&mut window, &mut host, 4);
    assert_eq!(cell_text(&mut host, &canvas, &ids[2][0]), "다\n라", "unmerge keeps the joined text at the anchor");
    window.app.invoke("edit.undo", json!({})).expect("undo the unmerge");
    app.idle(&mut window, &mut host, 4);
    assert!(table_of(&mut host, &canvas).cell_spans.get(&ids[2][0]).is_some(), "one undo restores the 1 × 2 merge");
    window.app.invoke("edit.undo", json!({})).expect("undo the 1 × 2 merge");
    app.idle(&mut window, &mut host, 4);
    assert!(table_of(&mut host, &canvas).cell_spans.is_empty());
    assert_eq!(cell_text(&mut host, &canvas, &ids[2][0]), "다", "one undo restores the body text");
    assert_eq!(cell_text(&mut host, &canvas, &ids[2][1]), "라", "one undo restores the covered body text");
}

/// A right click (press and release in two frames), then a pause.
fn right_click(app: &mut Driver, window: &mut Window, host: &mut Host, at: Pos2) {
    let button = |pressed| Event::PointerButton { pos: at, button: egui::PointerButton::Secondary, pressed, modifiers: Modifiers::NONE };
    app.frame(window, host, vec![Event::PointerMoved(at), button(true)]);
    app.frame(window, host, vec![button(false)]);
    app.idle(window, host, 2);
    app.time += 0.5;
}

/// The centre of the open table menu's item `index` (0: 셀 병합, 1: 병합 해제), as the menu laid it out.
fn menu_item(window: &Window, index: usize) -> Pos2 {
    window.app.table_merge.menu_items.expect("the table menu is open")[index].center()
}

fn changes_sent_since(app: &Driver, since: usize) -> usize {
    app.sent[since..].iter().filter(|method| *method == "changes.apply").count()
}

/// Why the open menu's item `index` is disabled (`None`: it is offered).
fn menu_reason(window: &Window, index: usize) -> Option<String> {
    window.app.table_menu_commands().expect("the table menu is open")[index].as_ref().err().cloned()
}

/// Frames until `done` holds (at most `frames`); whether it did.
fn until(app: &mut Driver, window: &mut Window, host: &mut Host, frames: u32, done: impl Fn(&Window) -> bool) -> bool {
    for _ in 0..frames {
        if done(window) {
            return true;
        }
        app.frame(window, host, Vec::new());
    }
    done(window)
}

fn tables(host: &mut Host, canvas: &str) -> Vec<Node> {
    let artifact = host.artifact(canvas);
    let ArtifactBody::Canvas(document) = &artifact.body else { panic!("a canvas") };
    document.pages[0].nodes.iter().filter(|node| node.ext.table.is_some()).cloned().collect()
}

fn add_node(host: &mut Host, window: &mut Window, canvas: &str, node: Node) {
    let page_id = match &host.artifact(canvas).body { ArtifactBody::Canvas(document) => document.pages[0].id.clone(), _ => unreachable!() };
    host.apply(canvas, vec![Operation::AddNode { page_id, parent_id: None, node }], [Intent::Content, Intent::Design].into_iter().collect())
        .expect("the node is added");
    window.app.refresh_active();
    window.settle(host);
}

#[test]
fn table_merge_right_click_selects_the_pressed_cell_and_the_menu_merges_and_unmerges_only_on_a_table() {
    let mut host = Host::start();
    let (canvas, mut window, table) = new_table(&mut host);
    header_off(&mut window, &mut host, &canvas);
    let mut app = Driver::at_once();
    let (c10, c11) = (table.rows[1].cell_ids[0].clone(), table.rows[1].cell_ids[1].clone());

    // Off the table the right click opens no menu and leaves the selection.
    let page = window.app.page_rect.expect("page 0 is drawn");
    let selection = window.app.selected_nodes.clone();
    right_click(&mut app, &mut window, &mut host, Pos2::new(page.left() + 6.0, page.bottom() - 6.0));
    assert!(window.app.table_merge.menu_items.is_none(), "no menu off a table");
    assert_eq!(window.app.selected_nodes, selection, "a right click off a table leaves the selection");

    // On a cell that is not selected the press selects it first. One cell has nothing to merge: 셀 병합 is disabled and a
    // click on it sends nothing.
    let at = centre(&window, &c11);
    right_click(&mut app, &mut window, &mut host, at);
    assert_eq!(window.app.selected_nodes, vec![c11.clone()], "the right press selects the pressed cell; notice: {:?}", window.message());
    assert!(menu_reason(&window, 0).is_some_and(|reason| reason.contains("두 칸")));
    let sent = app.sent.len();
    let at = menu_item(&window, 0);
    app.click(&mut window, &mut host, at);
    assert_eq!(changes_sent_since(&app, sent), 0, "a disabled 셀 병합 sends no change");
    assert!(table_of(&mut host, &canvas).cell_spans.is_empty());

    // Two selected cells (the click opens c10, Shift+click adds c11). A right click inside the selected range keeps it, and
    // 셀 병합 merges it.
    let at = centre(&window, &c10);
    app.click(&mut window, &mut host, at);
    shift_click(&mut app, &mut window, &mut host, &c11);
    let at = centre(&window, &c11);
    right_click(&mut app, &mut window, &mut host, at);
    let mut selected = window.app.selected_nodes.clone();
    selected.sort();
    let mut both = vec![c10.clone(), c11.clone()];
    both.sort();
    assert_eq!(selected, both, "a right click inside the selected range keeps it");
    let sent = app.sent.len();
    let at = menu_item(&window, 0);
    app.click(&mut window, &mut host, at);
    app.idle(&mut window, &mut host, 4);
    assert_eq!(changes_sent_since(&app, sent), 1, "셀 병합 sends one change; notice: {:?}", window.message());
    assert!(window.app.table_merge.menu_items.is_none(), "the menu closes after its choice");
    assert_eq!(table_of(&mut host, &canvas).cell_spans.get(&c10), Some(&designllm_contracts::CanvasTableSpan { row_span: 1, column_span: 2 }));

    // On the merged cell: 셀 병합 is disabled, 병합 해제 splits it, and one undo merges it again.
    let at = centre(&window, &c10);
    right_click(&mut app, &mut window, &mut host, at);
    assert!(menu_reason(&window, 0).is_some(), "a merged cell offers no merge");
    let at = menu_item(&window, 1);
    app.click(&mut window, &mut host, at);
    app.idle(&mut window, &mut host, 4);
    assert!(table_of(&mut host, &canvas).cell_spans.is_empty(), "병합 해제 removes the span; notice: {:?}", window.message());
    window.app.invoke("edit.undo", json!({})).expect("undo the unmerge");
    app.idle(&mut window, &mut host, 4);
    assert_eq!(table_of(&mut host, &canvas).cell_spans.len(), 1, "one undo restores the merged cell");
}

#[test]
fn table_merge_while_a_cell_is_edited_the_window_stores_the_edit_first_and_an_agent_is_refused() {
    let mut host = Host::start();
    let (canvas, mut window, table) = new_table(&mut host);
    header_off(&mut window, &mut host, &canvas);
    let mut app = Driver::at_once();
    let (c10, c11) = (table.rows[1].cell_ids[0].clone(), table.rows[1].cell_ids[1].clone());
    let table_id = table_node(&mut host, &canvas).id;
    set_cell_text(&mut host, &mut window, &canvas, &c11, "나");
    app.idle(&mut window, &mut host, 2);
    let at = centre(&window, &c10);
    app.click(&mut window, &mut host, at);
    assert_eq!(editing(&window).as_deref(), Some(c10.as_str()));
    app.keys(&mut window, &mut host, vec![Event::Text("가".into())]);
    assert_eq!(editing(&window).as_deref(), Some(c10.as_str()), "the edit is still open");

    // An agent's merge is refused while the edit is open (the edited text is not stored yet).
    let merge = json!({ "table_id": table_id, "row": 1, "column": 0, "rows": 1, "columns": 2 });
    let run = designllm_contracts::UiRun {
        run_id: "run-merge".into(), view_id: "view-agent".into(), expected: None, command_id: "table.merge_cells".into(),
        args: merge.clone(), actor: "agent".into(), permission: designllm_contracts::UiPermission::Change,
        allowed_intents: [Intent::Content].into_iter().collect(), status: designllm_contracts::UiRunStatus::Queued,
        timeout_ms: 10_000, result: None, error: None,
    };
    let refusal = window.app.ui_run_refusal(&run).expect("an agent's merge waits for the edit");
    assert_eq!(refusal.code, designllm_contracts::ErrorCode::TargetBusy, "{refusal:?}");

    // The window's merge stores the edit, then merges with the stored text.
    window.app.invoke("table.merge_cells", merge).expect("the merge waits for the edit");
    app.idle(&mut window, &mut host, 8);
    assert!(editing(&window).is_none(), "the merge ends the edit");
    assert_eq!(cell_text(&mut host, &canvas, &c10), "가\n나", "the edited text is in the merged cell; notice: {:?}", window.message());
    assert_eq!(cell_text(&mut host, &canvas, &c11), "");
    assert_eq!(table_of(&mut host, &canvas).cell_spans.get(&c10), Some(&designllm_contracts::CanvasTableSpan { row_span: 1, column_span: 2 }));

    // Editing the merged cell, its menu's 병합 해제 stores the edit and then unmerges.
    let at = centre(&window, &c10);
    app.click(&mut window, &mut host, at);
    assert_eq!(editing(&window).as_deref(), Some(c10.as_str()), "the merged cell opens");
    app.keys(&mut window, &mut host, vec![Event::Text("다".into())]);
    let draft = window.app.text_draft.clone();
    assert!(draft.contains('다'), "{draft:?}");
    let at = centre(&window, &c10);
    right_click(&mut app, &mut window, &mut host, at);
    assert_eq!(editing(&window).as_deref(), Some(c10.as_str()), "the right click leaves the edit open");
    let at = menu_item(&window, 1);
    app.click(&mut window, &mut host, at);
    app.idle(&mut window, &mut host, 8);
    assert!(editing(&window).is_none(), "the unmerge ends the edit");
    assert!(table_of(&mut host, &canvas).cell_spans.is_empty(), "the merged cell is split; notice: {:?}", window.message());
    assert_eq!(cell_text(&mut host, &canvas, &c10), draft, "the edited text is stored before the unmerge");
}

#[test]
fn table_merge_on_a_locked_table_the_menu_offers_nothing_and_the_command_is_refused() {
    let mut host = Host::start();
    let (canvas, mut window, table) = new_table(&mut host);
    header_off(&mut window, &mut host, &canvas);
    let mut app = Driver::at_once();
    let table_id = table_node(&mut host, &canvas).id;
    window.app.select_ids(vec![table_id.clone()]);
    app.idle(&mut window, &mut host, 2);
    window.app.invoke("edit.lock", json!({})).expect("the table is locked");
    app.idle(&mut window, &mut host, 4);
    assert_eq!(table_node(&mut host, &canvas).ext.locked, Some(true));

    let selection = window.app.selected_nodes.clone();
    let at = centre(&window, &table.rows[1].cell_ids[1]);
    right_click(&mut app, &mut window, &mut host, at);
    assert_eq!(window.app.selected_nodes, selection, "a locked table's cell is not selected");
    for unmerge in [false, true] {
        assert!(menu_reason(&window, usize::from(unmerge)).is_some_and(|reason| reason.contains("잠긴")), "unmerge {unmerge}");
    }
    let sent = app.sent.len();
    let at = menu_item(&window, 0);
    app.click(&mut window, &mut host, at);
    assert_eq!(changes_sent_since(&app, sent), 0, "a disabled item sends nothing");
    let refused = window.app.invoke("table.merge_cells", json!({ "table_id": table_id, "row": 1, "column": 0, "rows": 1, "columns": 2 }));
    assert!(refused.err().is_some_and(|reason| reason.contains("잠긴")), "the command refuses a locked table");
    assert!(table_of(&mut host, &canvas).cell_spans.is_empty());
}

#[test]
fn table_merge_a_right_click_outside_the_edited_cell_stores_the_edit_selects_the_pressed_cell_and_the_menu_names_it() {
    let mut host = Host::start();
    let (canvas, mut window, table) = new_table(&mut host);
    header_off(&mut window, &mut host, &canvas);
    let mut app = Driver::at_once();
    let (c00, c10) = (table.rows[0].cell_ids[0].clone(), table.rows[1].cell_ids[0].clone());
    let first = table_node(&mut host, &canvas);
    let merge_body = |id: &str| json!({ "table_id": id, "row": 1, "column": 0, "rows": 1, "columns": 2 });
    window.app.invoke("table.merge_cells", merge_body(&first.id)).expect("the first table's body row is merged");
    app.idle(&mut window, &mut host, 4);

    // A second table below the first, its body row merged too.
    window.app.invoke("canvas.table.add", json!({ "rows": 2, "columns": 2 })).expect("a second table");
    app.idle(&mut window, &mut host, 4);
    let second = tables(&mut host, &canvas).into_iter().find(|node| node.id != first.id).expect("the second table");
    let below = designllm_contracts::Rect { y: first.bounds.y + first.bounds.height + 40 * designllm_contracts::EMU_PER_CSS_PX, ..second.bounds };
    host.apply(&canvas, vec![Operation::SetGeometry { node_id: second.id.clone(), bounds: below }], [Intent::Design].into_iter().collect())
        .expect("the second table moves down");
    window.app.refresh_active();
    window.settle(&mut host);
    app.idle(&mut window, &mut host, 4);
    window.app.invoke("canvas.table.set_header", json!({ "table_id": second.id, "enabled": false })).expect("its header is off");
    app.idle(&mut window, &mut host, 4);
    window.app.invoke("table.merge_cells", merge_body(&second.id)).expect("the second table's body row is merged");
    app.idle(&mut window, &mut host, 4);
    let second_anchor = second.ext.table.as_ref().expect("a table").rows[1].cell_ids[0].clone();
    assert_eq!(table_of_id(&mut host, &canvas, &second.id).cell_spans.len(), 1, "notice: {:?}", window.message());

    // Editing the first table's merged cell, a right click on another cell stores the edit and selects that cell. Its menu
    // is about that cell: 병합 해제 is disabled and does not split the edited merged cell.
    window.app.select_ids(vec![first.id.clone()]);
    app.idle(&mut window, &mut host, 2);
    let at = centre(&window, &c10);
    app.click(&mut window, &mut host, at);
    assert_eq!(editing(&window).as_deref(), Some(c10.as_str()));
    app.keys(&mut window, &mut host, vec![Event::Text("가".into())]);
    let at = centre(&window, &c00);
    right_click(&mut app, &mut window, &mut host, at);
    app.idle(&mut window, &mut host, 4);
    assert!(editing(&window).is_none(), "the right click outside the edited cell ends the edit");
    assert_eq!(cell_text(&mut host, &canvas, &c10), "가", "the edit is stored");
    assert_eq!(window.app.selected_nodes, vec![c00.clone()], "the pressed cell is selected; notice: {:?}", window.message());
    assert!(menu_reason(&window, 1).is_some(), "c00 is no merged cell");
    let sent = app.sent.len();
    let at = menu_item(&window, 1);
    app.click(&mut window, &mut host, at);
    assert_eq!(changes_sent_since(&app, sent), 0);
    assert_eq!(table_of_id(&mut host, &canvas, &first.id).cell_spans.len(), 1, "the edited merged cell stays merged");

    // Editing a cell of the first table, a right click on the second table's merged cell: 병합 해제 splits the second table's.
    let at = centre(&window, &c00);
    app.click(&mut window, &mut host, at);
    assert_eq!(editing(&window).as_deref(), Some(c00.as_str()));
    app.keys(&mut window, &mut host, vec![Event::Text("나".into())]);
    let at = centre(&window, &second_anchor);
    right_click(&mut app, &mut window, &mut host, at);
    app.idle(&mut window, &mut host, 4);
    assert_eq!(window.app.selected_nodes, vec![second_anchor.clone()], "the second table's merged cell is selected");
    assert_eq!(menu_reason(&window, 1), None, "the second table's merged cell can be split");
    let at = menu_item(&window, 1);
    app.click(&mut window, &mut host, at);
    app.idle(&mut window, &mut host, 4);
    assert!(table_of_id(&mut host, &canvas, &second.id).cell_spans.is_empty(), "the second table is split; notice: {:?}", window.message());
    assert_eq!(table_of_id(&mut host, &canvas, &first.id).cell_spans.len(), 1, "the first table keeps its merged cell");
    assert_eq!(cell_text(&mut host, &canvas, &c00), "나", "the edit is stored");
}

fn table_of_id(host: &mut Host, canvas: &str, id: &str) -> CanvasTable {
    tables(host, canvas).into_iter().find(|node| node.id == id).and_then(|node| node.ext.table).expect("the table")
}

#[test]
fn table_merge_waiting_for_the_edit_is_dropped_when_the_window_goes_home_or_another_change_comes_first() {
    for case in ["home", "change"] {
        let mut host = Host::start();
        let (canvas, mut window, table) = new_table(&mut host);
        header_off(&mut window, &mut host, &canvas);
        let mut app = Driver { call_frames: 2, ..Driver::slow() };
        let (c00, c10) = (table.rows[0].cell_ids[0].clone(), table.rows[1].cell_ids[0].clone());
        let table_id = table_node(&mut host, &canvas).id;
        let at = centre(&window, &c10);
        app.click(&mut window, &mut host, at);
        assert!(until(&mut app, &mut window, &mut host, 40, |window| editing(window).as_deref() == Some(c10.as_str())), "{case}: the cell opens");
        app.keys(&mut window, &mut host, vec![Event::Text("가".into())]);
        window.app.invoke("table.merge_cells", json!({ "table_id": table_id, "row": 1, "column": 0, "rows": 1, "columns": 2 }))
            .expect("the merge waits for the edit");
        // The edit is stored and closed; the merge still waits for the window to read the stored revision.
        assert!(until(&mut app, &mut window, &mut host, 60, |window| window.app.inline_text_edit.is_none()), "{case}: the edit ends");
        assert!(window.app.span_waits_for_edit(), "{case}: the merge has not run yet");
        let expected = match case {
            "home" => {
                window.app.invoke("nav.home", json!({})).expect("home");
                "편집 화면을 떠나"
            }
            _ => {
                host.apply(&canvas, vec![Operation::SetText { node_id: c00.clone(), text: "밖".into() }], [Intent::Content].into_iter().collect())
                    .expect("another change");
                "다른 변경"
            }
        };
        app.idle(&mut window, &mut host, 30);
        assert!(!window.app.span_waits_for_edit(), "{case}");
        assert!(table_of(&mut host, &canvas).cell_spans.is_empty(), "{case}: the waiting merge is dropped");
        assert_eq!(cell_text(&mut host, &canvas, &c10), "가", "{case}: the edit is stored");
        let notice = window.message().unwrap_or_default();
        assert!(notice.contains(expected), "{case}: the reason is shown: {notice}");
    }
}

#[test]
fn table_merge_menu_only_for_the_topmost_table_reselects_a_mixed_selection_and_closes_for_another_popup() {
    let mut host = Host::start();
    let (canvas, mut window, table) = new_table(&mut host);
    header_off(&mut window, &mut host, &canvas);
    let mut app = Driver::at_once();
    let (c01, c11) = (table.rows[0].cell_ids[1].clone(), table.rows[1].cell_ids[1].clone());
    let bounds = table_node(&mut host, &canvas).bounds;
    // A shape over the table's first row.
    let shape: Node = serde_json::from_value(json!({
        "id": "shape-1", "kind": "rectangle",
        "bounds": { "x": bounds.x, "y": bounds.y, "width": bounds.width, "height": 36 * designllm_contracts::EMU_PER_CSS_PX },
    })).expect("a shape");
    add_node(&mut host, &mut window, &canvas, shape);
    app.idle(&mut window, &mut host, 4);

    let at = centre(&window, &c01);
    right_click(&mut app, &mut window, &mut host, at);
    assert!(window.app.table_merge.menu_items.is_none(), "the shape is on top there: no table menu");

    // The cell selected with another object: the right press selects the cell alone.
    window.app.select_ids(vec!["shape-1".into(), c11.clone()]);
    app.idle(&mut window, &mut host, 2);
    let at = centre(&window, &c11);
    right_click(&mut app, &mut window, &mut host, at);
    assert_eq!(window.app.selected_nodes, vec![c11.clone()], "the pressed cell alone is selected");
    assert!(window.app.table_merge.menu_items.is_some(), "the table menu opens on the cell");

    // Another popup opening closes the menu; where its item was, nothing is sent.
    let item = menu_item(&window, 0);
    egui::Popup::open_id(&window.ctx, egui::Id::new("another-popup"));
    app.idle(&mut window, &mut host, 2);
    assert!(window.app.table_merge.menu_items.is_none(), "the menu is closed");
    assert!(window.app.table_menu_commands().is_none(), "its commands are dropped with it");
    let sent = app.sent.len();
    app.click(&mut window, &mut host, item);
    assert_eq!(changes_sent_since(&app, sent), 0);
}

/// Height (EMU) of `node`'s frame on page 0, as the core laid it out and the board drew it.
fn frame_height(window: &Window, node: &str) -> i64 {
    let page = window.app.page_textures.get(&0).expect("page 0 is drawn");
    page.frames.get(node).unwrap_or_else(|| panic!("the render has a frame for {node}")).height
}

#[test]
fn table_row_grows_with_its_text_past_its_minimum_and_a_merged_anchor_keeps_both_lines_in_view() {
    let mut host = Host::start();
    let (canvas, mut window, _table) = new_table(&mut host);
    header_off(&mut window, &mut host, &canvas);
    let mut app = Driver::at_once();
    let ids: Vec<Vec<String>> = table_of(&mut host, &canvas).rows.iter().map(|row| row.cell_ids.clone()).collect();
    let (first, second) = (ids[1][0].clone(), ids[1][1].clone());
    let minimum = frame_height(&window, &first);

    // Two lines in one cell make its row taller than the minimum.
    set_cell_text(&mut host, &mut window, &canvas, &first, "가\n나");
    app.idle(&mut window, &mut host, 2);
    let two_lines = frame_height(&window, &first);
    assert!(two_lines > minimum, "two lines grow the row ({two_lines} > {minimum}); notice: {:?}", window.message());

    // One line again gives the minimum back, not less.
    set_cell_text(&mut host, &mut window, &canvas, &first, "가");
    app.idle(&mut window, &mut host, 2);
    let one_line = frame_height(&window, &first);
    assert!(one_line >= minimum && one_line < two_lines, "one line returns to at least the minimum ({one_line}, minimum {minimum})");

    // A merged anchor with two lines (the covered cell's text joins it) keeps both lines inside the merged frame.
    set_cell_text(&mut host, &mut window, &canvas, &first, "가\n나");
    app.idle(&mut window, &mut host, 2);
    window.app.set_selected_ids(vec![first.clone(), second.clone()]);
    window.app.run_span_action(window.app.canvas_table_span_command(false));
    app.idle(&mut window, &mut host, 4);
    assert_eq!(table_of(&mut host, &canvas).cell_spans.get(&first), Some(&designllm_contracts::CanvasTableSpan { row_span: 1, column_span: 2 }), "the 1 × 2 merge is made");
    assert!(frame_height(&window, &first) >= two_lines, "the merged anchor is at least two lines tall ({} >= {two_lines})", frame_height(&window, &first));
}

// The right-click menus of nodes, layer rows and page rows (68 item 16), through `App::update` with the UI-test recording on:
// the last frame's layer rows and controls (`ui.layout`) say where to press, and an item is pressed as a person presses it.

/// A window on a new canvas whose frames record their layer rows and controls.
fn menu_window(host: &mut Host, title: &str) -> (String, Window) {
    let canvas = host.create("canvas", title);
    let mut window = Window::open_with_previews(host, &canvas);
    window.app.screen = super::Screen::Editor;
    window.app.ui_test = Some(super::ui_test_input::UiTestInput::default());
    window.draw(host, 1.0);
    (canvas, window)
}

fn rectangle(id: &str, x: i64, children: Vec<Node>) -> Node {
    let kind = if children.is_empty() { "rectangle" } else { "group" };
    let mut node: Node = serde_json::from_value(json!({
        "id": id, "kind": kind, "bounds": { "x": x, "y": 1_000_000, "width": 1_500_000, "height": 1_000_000 },
    })).expect("a node");
    node.children = children;
    node
}

fn page_ids(host: &mut Host, canvas: &str) -> Vec<String> {
    let ArtifactBody::Canvas(document) = host.artifact(canvas).body else { panic!("a canvas") };
    document.pages.iter().map(|page| page.id.clone()).collect()
}

fn pages_json(host: &mut Host, canvas: &str) -> serde_json::Value {
    let ArtifactBody::Canvas(document) = host.artifact(canvas).body else { panic!("a canvas") };
    serde_json::to_value(&document.pages).expect("the pages")
}

fn has_node(host: &mut Host, canvas: &str, id: &str) -> bool {
    let artifact = host.artifact(canvas);
    super::doc_tree::find_node(&artifact, id).is_some()
}

/// The centre of the item labelled `label` (its command's label) in the menu opened at `at`: of the controls the last frame
/// drew with that label, the one nearest `at` in the menu's column (a menu opens beside its anchor), so a panel's control of
/// the same label is not taken.
fn control(window: &Window, label: &str, at: Pos2) -> Option<Pos2> {
    window.app.ui_test.as_ref()?.last_controls().into_iter()
        .filter(|(shown, rect)| shown == label && rect.expand(8.0).x_range().contains(at.x))
        .min_by(|(_, a), (_, b)| a.distance_to_pos(at).total_cmp(&b.distance_to_pos(at)))
        .map(|(_, rect)| rect.center())
}

fn row(window: &Window, node: Option<&str>, page: Option<u32>) -> Pos2 {
    window
        .app
        .ui_test
        .as_ref()
        .and_then(|test| test.last_row_centre(node, page))
        .unwrap_or_else(|| panic!("the layer row of {node:?} / page {page:?} is drawn"))
}

fn sorted(mut ids: Vec<String>) -> Vec<String> {
    ids.sort();
    ids
}

#[test]
fn node_menu_on_the_board_and_on_layer_rows_selects_keeps_a_selection_and_runs_its_items() {
    let mut host = Host::start();
    let (canvas, mut window) = menu_window(&mut host, "우클릭 메뉴");
    add_node(&mut host, &mut window, &canvas, rectangle("r1", 500_000, vec![]));
    add_node(&mut host, &mut window, &canvas, rectangle("r2", 2_500_000, vec![]));
    add_node(&mut host, &mut window, &canvas, rectangle("g1", 4_500_000, vec![rectangle("k1", 4_500_000, vec![])]));
    let mut app = Driver::at_once();
    app.idle(&mut window, &mut host, 3);

    // The board: a right click on a node that is not selected selects it and opens its menu.
    let at = centre(&window, "r1");
    right_click(&mut app, &mut window, &mut host, at);
    assert_eq!(window.app.selected_nodes, vec!["r1".to_owned()], "the right press selects the node");
    for label in ["복제", "복사", "붙여넣기", "삭제"] {
        assert!(control(&window, label, at).is_some(), "the node menu shows {label}");
    }
    app.keys(&mut window, &mut host, vec![key(Key::Escape)]);
    assert!(control(&window, "복제", at).is_none(), "Escape closes the menu");

    // Two selected nodes: a right click on one keeps both, the menu is for both (no 복제), and 삭제 removes both.
    window.app.select_ids(vec!["r1".into(), "r2".into()]);
    app.idle(&mut window, &mut host, 2);
    let at = centre(&window, "r2");
    right_click(&mut app, &mut window, &mut host, at);
    assert_eq!(sorted(window.app.selected_nodes.clone()), ["r1", "r2"], "a right click inside the selection keeps it");
    assert!(control(&window, "복제", at).is_none(), "복제 is for one node");
    let item = control(&window, "삭제", at).expect("the menu shows 삭제");
    app.click(&mut window, &mut host, item);
    app.idle(&mut window, &mut host, 3);
    assert!(!has_node(&mut host, &canvas, "r1") && !has_node(&mut host, &canvas, "r2"), "삭제 removes both; notice: {:?}", window.message());
    window.app.invoke("edit.undo", json!({})).expect("undo the removal");
    app.idle(&mut window, &mut host, 4);
    assert!(has_node(&mut host, &canvas, "r1") && has_node(&mut host, &canvas, "r2"), "one undo brings both back");

    // A layer row two levels down (a group's child): its right click selects it, and 삭제 removes it.
    window.app.select_ids(vec!["g1".into()]);
    app.idle(&mut window, &mut host, 2);
    let at = row(&window, Some("k1"), None);
    right_click(&mut app, &mut window, &mut host, at);
    assert_eq!(window.app.selected_nodes, vec!["k1".to_owned()], "the right press on the row selects its node");
    let item = control(&window, "삭제", at).expect("the row's menu shows 삭제");
    app.click(&mut window, &mut host, item);
    app.idle(&mut window, &mut host, 3);
    assert!(!has_node(&mut host, &canvas, "k1"), "삭제 from the row's menu removes the child; notice: {:?}", window.message());
    assert!(has_node(&mut host, &canvas, "g1"));

    // A locked node: its row's menu offers 잠금 해제, which unlocks it.
    window.app.select_ids(vec!["r2".into()]);
    window.app.invoke("edit.lock", json!({})).expect("lock r2");
    app.idle(&mut window, &mut host, 3);
    window.app.select_ids(Vec::new());
    app.idle(&mut window, &mut host, 2);
    let at = row(&window, Some("r2"), None);
    right_click(&mut app, &mut window, &mut host, at);
    assert_eq!(window.app.selected_nodes, vec!["r2".to_owned()], "a locked node's row selects it");
    let item = control(&window, "잠금 해제", at).expect("a locked node's menu offers 잠금 해제");
    app.click(&mut window, &mut host, item);
    app.idle(&mut window, &mut host, 3);
    let artifact = host.artifact(&canvas);
    let locked = super::doc_tree::find_node(&artifact, "r2").and_then(|node| node.ext.locked);
    assert_ne!(locked, Some(true), "잠금 해제 unlocks it; notice: {:?}", window.message());
}

#[test]
fn table_members_take_the_table_menu_from_the_board_and_from_their_layer_rows() {
    let mut host = Host::start();
    let (canvas, mut window, table) = new_table(&mut host);
    header_off(&mut window, &mut host, &canvas);
    window.app.ui_test = Some(super::ui_test_input::UiTestInput::default());
    let mut app = Driver::at_once();
    app.idle(&mut window, &mut host, 2);
    let table_id = table_node(&mut host, &canvas).id;
    let cell = table.rows[1].cell_ids[1].clone();

    // The board: a cell's right click opens the table menu, not the node menu.
    let at = centre(&window, &cell);
    right_click(&mut app, &mut window, &mut host, at);
    assert!(window.app.table_merge.menu_items.is_some(), "the table menu opens on the cell");
    assert!(control(&window, "복제", at).is_none(), "no node menu on a table cell");
    // A right click on another cell (one the open menu does not cover) while the menu is open opens it there; egui would
    // close it on that click.
    let menu = window.ctx.memory(|memory| memory.area_rect(egui::Id::new("table-cell-menu"))).expect("the menu's area");
    let other = table.rows.iter().flat_map(|row| row.cell_ids.iter()).find(|id| **id != cell && !menu.contains(centre(&window, id)))
        .expect("a cell beside the menu").clone();
    let at = centre(&window, &other);
    right_click(&mut app, &mut window, &mut host, at);
    assert_eq!(window.app.selected_nodes, vec![other.clone()], "the second right press selects its cell");
    assert!(window.app.table_merge.menu_items.is_some(), "the menu opens again on the other cell");
    app.keys(&mut window, &mut host, vec![key(Key::Escape)]);
    assert!(window.app.table_merge.menu_items.is_none(), "Escape closes the table menu");

    // The table's layer row: the table menu for the table, not the node menu.
    window.app.select_ids(Vec::new());
    app.idle(&mut window, &mut host, 2);
    let at = row(&window, Some(&table_id), None);
    right_click(&mut app, &mut window, &mut host, at);
    assert_eq!(window.app.selected_nodes, vec![table_id.clone()], "the row's right press selects the table");
    assert!(window.app.table_merge.menu_items.is_some(), "the table menu opens from the table's row");
    assert!(control(&window, "복제", at).is_none() && control(&window, "셀 병합", at).is_some(), "the table menu, not the node menu");
    app.keys(&mut window, &mut host, vec![key(Key::Escape)]);
    assert!(window.app.table_merge.menu_items.is_none(), "Escape closes it");

    // A cell's layer row (shown under the selected table): the cell is selected as the board's press does, and the menu
    // says why one cell cannot merge.
    let at = row(&window, Some(&cell), None);
    right_click(&mut app, &mut window, &mut host, at);
    assert_eq!(window.app.selected_nodes, vec![cell.clone()], "the cell row's right press selects the cell");
    assert!(menu_reason(&window, 0).is_some_and(|reason| reason.contains("두 칸")), "one cell has nothing to merge");
    assert!(control(&window, "복제", at).is_none());
}

#[test]
fn figure_diagram_rows_open_no_node_menu() {
    use super::figure_tools::{connection, new_diagram_panel, node_addition, panel_additions};
    let mut host = Host::start();
    let figure_id = host.create_figure();
    let (figure, _) = host.figure(&figure_id);
    host.apply(&figure_id, panel_additions(&figure, new_diagram_panel(&figure)), [Intent::Content, Intent::Design].into_iter().collect())
        .expect("a diagram panel");
    for _ in 0..2 {
        let (figure, _) = host.figure(&figure_id);
        let operation = node_addition(&figure, &figure.panels[0].id, designllm_contracts::NodeKind::Rectangle).expect("room for a node");
        host.apply(&figure_id, vec![operation], [Intent::Content].into_iter().collect()).expect("a box");
    }
    let (figure, _) = host.figure(&figure_id);
    let designllm_contracts::PanelBody::Diagram(diagram) = &figure.panels[0].body else { panic!("a diagram") };
    let boxes: Vec<String> = diagram.nodes.iter().map(|node| node.id.clone()).collect();
    let edge = connection(&figure.panels[0].id, &boxes[0], &boxes[1]);
    let Operation::ConnectDiagram { edge: designllm_contracts::Edge { id: edge_id, .. }, .. } = &edge else { unreachable!() };
    let edge_id = edge_id.clone();
    host.apply(&figure_id, vec![edge], [Intent::Content].into_iter().collect()).expect("a connector");
    let mut window = Window::open_with_previews(&mut host, &figure_id);
    window.app.screen = super::Screen::Editor;
    window.app.ui_test = Some(super::ui_test_input::UiTestInput::default());
    let mut app = Driver::at_once();
    app.idle(&mut window, &mut host, 4);

    for id in [boxes[0].as_str(), edge_id.as_str()] {
        let at = row(&window, Some(id), None);
        right_click(&mut app, &mut window, &mut host, at);
        assert!(!egui::Popup::is_any_open(&window.ctx), "no node menu on the figure row {id}");
        assert!(control(&window, "복제", at).is_none());
    }
}

#[test]
fn page_row_menu_removes_the_page_it_opened_for_and_one_undo_restores_the_page_and_its_nodes() {
    let mut host = Host::start();
    let (canvas, mut window) = menu_window(&mut host, "페이지 메뉴");
    window.app.invoke("add.page", json!({})).expect("a second page");
    window.settle(&mut host);
    let pages = page_ids(&mut host, &canvas);
    assert_eq!(pages.len(), 2);
    host.apply(
        &canvas,
        vec![Operation::AddNode { page_id: pages[1].clone(), parent_id: None, node: rectangle("p2-node", 500_000, vec![]) }],
        [Intent::Content, Intent::Design].into_iter().collect(),
    )
    .expect("a node on page 2");
    window.app.refresh_active();
    window.settle(&mut host);
    let mut app = Driver::at_once();
    app.idle(&mut window, &mut host, 3);
    let before = pages_json(&mut host, &canvas);

    // 페이지 삭제 on page 2's row removes page 2 with its node; one undo restores both as they were.
    let at = row(&window, None, Some(1));
    right_click(&mut app, &mut window, &mut host, at);
    assert!(control(&window, "페이지 추가", at).is_some(), "the page menu shows 페이지 추가");
    let item = control(&window, "페이지 삭제", at).expect("the page menu shows 페이지 삭제");
    app.click(&mut window, &mut host, item);
    app.idle(&mut window, &mut host, 3);
    assert_eq!(page_ids(&mut host, &canvas), vec![pages[0].clone()], "page 2 is removed; notice: {:?}", window.message());
    assert!(!has_node(&mut host, &canvas, "p2-node"));
    window.app.invoke("edit.undo", json!({})).expect("undo the removal");
    app.idle(&mut window, &mut host, 4);
    assert_eq!(pages_json(&mut host, &canvas), before, "one undo restores the page, its place and its node exactly");

    // The menu keeps its page by id: the pages are reordered while it is open, and 페이지 삭제 still removes that page.
    let at = row(&window, None, Some(1));
    right_click(&mut app, &mut window, &mut host, at);
    assert!(control(&window, "페이지 삭제", at).is_some(), "the page menu is open on page 2");
    host.apply(&canvas, vec![Operation::ReorderPages { page_ids: vec![pages[1].clone(), pages[0].clone()] }], [Intent::Content, Intent::Design].into_iter().collect())
        .expect("the pages are reordered");
    window.app.refresh_active();
    window.settle(&mut host);
    app.idle(&mut window, &mut host, 2);
    assert_eq!(page_ids(&mut host, &canvas), vec![pages[1].clone(), pages[0].clone()]);
    let item = control(&window, "페이지 삭제", at).expect("the menu stays open on its row");
    app.click(&mut window, &mut host, item);
    app.idle(&mut window, &mut host, 3);
    assert_eq!(page_ids(&mut host, &canvas), vec![pages[0].clone()], "the page the menu opened for is removed (now first); notice: {:?}", window.message());
}
