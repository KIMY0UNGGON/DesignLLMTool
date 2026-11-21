//! Task 69: the synthetic input of a UI-test window, end to end over a real workspace. Runs are queued at the core as an
//! agent's `ui.command.run`, delivered by the window's own poll, and played through `App::raw_input_hook` and `App::update`
//! (never `draw_frame` alone), so what they change is what a person's input changes.

use std::time::{Duration, Instant};

use designllm_contracts::{ArtifactBody, ErrorCode, UiLayout, EMU_PER_CSS_PX, UI_INPUT_COMMAND, UI_LAYOUT_COMMAND};
use eframe::egui::{self, Event, Key, Modifiers, Pos2, Rect};
use serde_json::{json, Value};

use super::{
    test_support::{primary_button, screen, Host, Window},
    ui_test_input::{UiTestInput, UI_PERF_COMMAND},
};

/// A window on a canvas with the rectangles `r1`, `r2` and the text box `t1` ("가"), drawn through `update`. `test` turns the synthetic input on
/// before the window first publishes its registry.
struct Bench {
    host: Host,
    window: Window,
    canvas: String,
    clock: f64,
}

impl Bench {
    fn new(test: bool) -> Self {
        let mut host = Host::start();
        let canvas = canvas(&mut host);
        Self::open(host, canvas, test)
    }

    /// A window on `artifact` of `host`, drawn through `update`.
    fn open(mut host: Host, artifact: String, test: bool) -> Self {
        let mut window = Window::open_with_previews(&mut host, &artifact);
        if test {
            window.app.ui_test = Some(UiTestInput::default());
        }
        let mut bench = Self { host, window, canvas: artifact, clock: 1.0 };
        for _ in 0..4 {
            bench.frame();
        }
        bench
    }

    /// One frame as eframe runs it: the raw input hook, then `App::update`; then the host answers what the frame sent.
    fn frame(&mut self) {
        self.frame_with(Vec::new(), true);
    }

    /// `frame` with `events` as the window's input. Without `answer` the host does not answer yet: what the frame sent waits for
    /// the next answered frame, as with a slow host.
    fn frame_with(&mut self, events: Vec<Event>, answer: bool) {
        self.clock += 0.02;
        let mut input = screen(1.0, 1280.0, 800.0);
        input.time = Some(self.clock);
        input.events = events;
        let ctx = self.window.ctx.clone();
        eframe::App::raw_input_hook(&mut self.window.app, &ctx, &mut input);
        let mut frame = eframe::Frame::_new_kittest();
        let _ = ctx.run(input, |ctx| eframe::App::update(&mut self.window.app, ctx, &mut frame));
        if answer {
            self.window.settle(&mut self.host);
        }
    }

    /// One answered frame with `events`, then one with none.
    fn keys(&mut self, events: Vec<Event>) {
        self.frame_with(events, true);
        self.frame();
    }

    /// A click at `at` (pressed and released in one frame), then one frame with no input.
    fn click_at(&mut self, at: Pos2) {
        self.keys(vec![Event::PointerMoved(at), primary_button(at, true), primary_button(at, false)]);
    }

    /// Runs `command` as the window's own (`DesktopApp::invoke`), then draws.
    fn step(&mut self, command: &str, args: Value) {
        self.window.step(&mut self.host, command, args).unwrap_or_else(|error| panic!("{command}: {error}"));
        self.frame();
    }

    /// Queues `command` for the window's view as the agent `agent`, and lets the window's next poll take it.
    fn queue(&mut self, command: &str, args: Value, permission: &str, intents: &[&str]) -> Result<String, designllm_contracts::DltError> {
        self.window.app.publish_ui_if_due();
        self.window.settle(&mut self.host);
        let view_id = self.window.app.ui.view_id.clone();
        let queued = self.host.call(
            "ui.command.run",
            json!({
                "view_id": view_id, "command_id": command, "actor": "agent", "permission": permission,
                "allowed_intents": intents, "args": args, "timeout_ms": 60_000,
            }),
        )?;
        self.window.app.last_event_poll = Instant::now() - Duration::from_secs(3);
        Ok(queued["run_id"].as_str().expect("a run id").to_owned())
    }

    /// Draws frames until the run ends; returns its record.
    fn finish(&mut self, run_id: &str) -> Value {
        for _ in 0..600 {
            self.frame();
            let run = self.host.call("ui.command.get", json!({ "run_id": run_id })).expect("the run's record");
            if !matches!(run["status"].as_str(), Some("queued" | "delivered")) {
                return run;
            }
        }
        panic!("the run {run_id} did not end");
    }

    fn run(&mut self, command: &str, args: Value, permission: &str, intents: &[&str]) -> Value {
        let run_id = self.queue(command, args, permission, intents).expect("the run is queued");
        self.finish(&run_id)
    }

    fn input(&mut self, events: Value) -> Value {
        let run = self.run(UI_INPUT_COMMAND, json!({ "events": events }), "agent_start", &["content", "design", "data"]);
        assert_eq!(run["status"], "completed", "{run}");
        run["result"].clone()
    }

    fn layout(&mut self) -> UiLayout {
        let run = self.run(UI_LAYOUT_COMMAND, json!({}), "read", &[]);
        assert_eq!(run["status"], "completed", "{run}");
        serde_json::from_value(run["result"].clone()).expect("a layout")
    }

    fn revision(&mut self) -> u64 {
        self.host.artifact(&self.canvas).revision
    }

    fn bounds(&mut self, node_id: &str) -> designllm_contracts::Rect {
        let artifact = self.host.artifact(&self.canvas);
        super::doc_tree::find_node(&artifact, node_id).expect("the node").bounds
    }

    fn text(&mut self, node_id: &str) -> String {
        let ArtifactBody::Canvas(document) = self.host.artifact(&self.canvas).body else { panic!("a canvas") };
        document.pages.iter().flat_map(|page| page.nodes.iter()).find(|node| node.id == node_id).map(|node| node.text.clone()).unwrap_or_default()
    }

    /// The actors of the `artifact.changed` events of the canvas, oldest first.
    fn change_actors(&mut self) -> Vec<String> {
        let batch = self.host.call("events.poll", json!({ "after_seq": 0, "limit": 1000 })).expect("events");
        batch["events"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|event| event["event"] == "artifact.changed" && event["artifact_id"] == self.canvas.as_str())
            .map(|event| event["detail"]["actor"].as_str().unwrap_or_default().to_owned())
            .collect()
    }
}

fn centre(rect: designllm_contracts::UiPointRect) -> [f32; 2] {
    [rect.x + rect.width / 2.0, rect.y + rect.height / 2.0]
}

fn node_rect(layout: &UiLayout, id: &str) -> designllm_contracts::UiPointRect {
    layout.nodes.iter().find(|node| node.node_id == id).unwrap_or_else(|| panic!("{id} is in the layout")).rect
}

#[test]
fn a_window_not_started_for_tests_lists_neither_command_and_refuses_a_run_that_reaches_it() {
    let mut bench = Bench::new(false);
    let view_id = bench.window.app.ui.view_id.clone();
    let listing = bench.host.call("ui.command.list", json!({ "view_id": view_id })).expect("the registry");
    let ids: Vec<&str> = listing["commands"].as_array().expect("commands").iter().filter_map(|command| command["id"].as_str()).collect();
    assert!(!ids.contains(&UI_INPUT_COMMAND) && !ids.contains(&UI_LAYOUT_COMMAND), "off by default");
    let state = bench.host.call("ui.state.read", json!({ "view_id": view_id })).expect("the state");
    assert!(state["state"]["detail"].get("ui_test_input").is_none());
    let refused = bench.queue(UI_INPUT_COMMAND, json!({ "events": [] }), "agent_start", &["content", "design", "data"]).expect_err("refused");
    assert_eq!(refused.code, ErrorCode::NotFound);
    // A run that reaches the window anyway (a stale registry) is refused by the window too.
    let revision = bench.revision();
    let run = designllm_contracts::UiRun {
        run_id: designllm_contracts::new_id(), view_id, expected: None, command_id: UI_INPUT_COMMAND.into(),
        args: json!({ "events": [{ "type": "key", "key": "Delete" }] }), actor: "agent".into(),
        permission: designllm_contracts::UiPermission::AgentStart,
        allowed_intents: [designllm_contracts::Intent::Content, designllm_contracts::Intent::Design, designllm_contracts::Intent::Data].into_iter().collect(),
        status: designllm_contracts::UiRunStatus::Delivered, timeout_ms: 5_000, result: None, error: None,
    };
    bench.window.app.execute_ui_test_run(run);
    bench.window.settle(&mut bench.host);
    let (_, params, _) = bench.window.calls.iter().rev().find(|(method, _, _)| method == "ui.command.complete").expect("the window answers the run");
    assert_eq!(params["error"]["code"], "NOT_FOUND", "{params}");
    assert!(bench.window.app.ui_test.is_none());
    bench.frame();
    assert_eq!(bench.revision(), revision, "nothing was played");
}

#[test]
fn a_test_window_lists_both_commands_and_says_so_in_its_state() {
    let mut bench = Bench::new(true);
    let view_id = bench.window.app.ui.view_id.clone();
    let listing = bench.host.call("ui.command.list", json!({ "view_id": view_id })).expect("the registry");
    let find = |id: &str| listing["commands"].as_array().expect("commands").iter().find(|command| command["id"] == id).cloned().unwrap_or_else(|| panic!("{id} listed"));
    assert_eq!(find(UI_INPUT_COMMAND)["permission"], "agent_start");
    assert_eq!(find(UI_INPUT_COMMAND)["intents"], json!(["design", "content", "data"]));
    assert_eq!(find(UI_LAYOUT_COMMAND)["permission"], "read");
    assert_eq!(find(UI_PERF_COMMAND)["permission"], "read");
    // The timing reads back what the window sent, and starts again on reset.
    let read = bench.run(UI_PERF_COMMAND, json!({ "reset": true }), "read", &[]);
    assert_eq!(read["status"], "completed", "{read}");
    assert!(read["result"]["methods"]["ui.state.publish"]["sent"].as_u64().is_some_and(|sent| sent > 0), "{read}");
    let again = bench.run(UI_PERF_COMMAND, json!({}), "read", &[]);
    assert!(again["result"]["frames"].as_u64() < read["result"]["frames"].as_u64(), "the counts started again: {again}");
    let state = bench.host.call("ui.state.read", json!({ "view_id": view_id })).expect("the state");
    assert_eq!(state["state"]["detail"]["ui_test_input"], true);
    // The window refuses a run without every intent or the strictest class (the core already did; the window checks again).
    assert_eq!(bench.queue(UI_INPUT_COMMAND, json!({}), "file_write", &["content", "design", "data"]).expect_err("class").code, ErrorCode::InvalidArgument);
    assert_eq!(bench.queue(UI_INPUT_COMMAND, json!({}), "agent_start", &["content", "design"]).expect_err("intents").code, ErrorCode::ContentLocked);
    // Bad events are refused before anything is played.
    let run = bench.run(UI_INPUT_COMMAND, json!({ "events": [{ "type": "key", "key": "NoSuchKey" }] }), "agent_start", &["content", "design", "data"]);
    assert_eq!(run["status"], "failed");
    assert_eq!(run["error"]["code"], "INVALID_ARGUMENT", "{run}");
}

#[test]
fn at_each_forced_scale_the_layout_points_are_where_a_click_selects() {
    // U02: the launch sets the zoom factor (`DESIGNLLM_UI_TEST_SCALE`, lib.rs); coordinates stay in points at every scale.
    let mut bench = Bench::new(true);
    for (scale, node) in [(1.5, "r2"), (2.0, "r1"), (1.0, "r2")] {
        bench.window.ctx.set_zoom_factor(scale);
        for _ in 0..3 {
            bench.frame();
        }
        let layout = bench.layout();
        assert_eq!(layout.pixels_per_point, scale);
        let [x, y] = centre(node_rect(&layout, node));
        bench.input(json!([{ "type": "click", "x": x, "y": y }]));
        assert_eq!(bench.window.app.selected_nodes, vec![node.to_string()], "scale {scale}");
    }
}

#[test]
fn layout_rectangles_are_where_the_window_drew_and_clicks_there_select_like_a_person() {
    let mut bench = Bench::new(true);
    let layout = bench.layout();
    assert!(layout.frame > 0 && layout.pixels_per_point == 1.0);
    for id in ["top_bar", "sidebar", "properties", "board"] {
        assert!(layout.panels.iter().any(|panel| panel.id == id), "{id} panel: {:?}", layout.panels);
    }
    // The node rectangles are the frames the board draws and hit-tests.
    let board = bench.window.app.board_rect.expect("the board is drawn");
    let view = bench.window.app.view_ref();
    let drawn: Vec<(String, Rect)> = bench.window.app.node_frames(board.min + view.pan, view.zoom);
    for id in ["r1", "r2", "t1"] {
        let rect = node_rect(&layout, id);
        let frame = drawn.iter().find(|(node, _)| node == id).expect("drawn").1;
        assert_eq!((rect.x, rect.y, rect.width, rect.height), (frame.min.x, frame.min.y, frame.width(), frame.height()), "{id}");
    }
    assert!(layout.pages.iter().any(|page| page.page_index == 0 && page.visible));

    let [x, y] = centre(node_rect(&layout, "r2"));
    let revision = bench.revision();
    let result = bench.input(json!([{ "type": "click", "x": x, "y": y }]));
    assert_eq!(bench.window.app.selected_nodes, vec!["r2".to_string()], "the click on r2's rectangle selects r2");
    assert_eq!(result["state"]["selection"]["node_ids"], json!(["r2"]), "{result}");
    assert_eq!(result["settled"], true);
    let steps = result["steps"].as_array().expect("steps");
    assert_eq!(steps.len(), 1);
    assert!(steps[0]["last_frame"].as_u64() > steps[0]["first_frame"].as_u64(), "a click takes a move, a press and a release frame");
    assert_eq!(bench.revision(), revision, "a selection is no change");

    // A layer row of t1, as the sidebar drew it, selects t1.
    let layout = bench.layout();
    let row = layout.layer_rows.iter().find(|row| row.node_id.as_deref() == Some("t1") && row.visible).expect("t1 has a visible layer row");
    let [x, y] = centre(row.rect);
    bench.input(json!([{ "type": "click", "x": x, "y": y }]));
    assert_eq!(bench.window.app.selected_nodes, vec!["t1".to_string()]);

    // The panel's own close button is a control of nav.toggle_right; a click there closes the panel.
    let layout = bench.layout();
    let properties = layout.panels.iter().find(|panel| panel.id == "properties").expect("the properties panel").rect;
    let close = layout
        .controls
        .iter()
        .find(|control| control.command_id.as_deref() == Some("nav.toggle_right") && control.rect.x >= properties.x)
        .expect("the panel's close button");
    let [x, y] = centre(close.rect);
    bench.input(json!([{ "type": "click", "x": x, "y": y }]));
    assert!(!bench.window.app.view_ref().right_open, "the panel closed");
}

#[test]
fn a_drag_through_update_moves_the_node_in_one_change_made_by_the_agent() {
    let mut bench = Bench::new(true);
    let layout = bench.layout();
    let [x, y] = centre(node_rect(&layout, "r1"));
    let before = bench.bounds("r1");
    let revision = bench.revision();
    let actors = bench.change_actors().len();
    let result = bench.input(json!([
        { "type": "click", "x": x, "y": y, "after_frames": 2 },
        { "type": "drag", "from": [x, y], "to": [x + 40.0, y + 20.0], "steps": 6 },
    ]));
    let after = bench.bounds("r1");
    assert_ne!((after.x, after.y), (before.x, before.y), "the drag moves r1");
    assert_eq!((after.width, after.height), (before.width, before.height), "a move keeps the size");
    assert_eq!(bench.revision(), revision + 1, "one change, one undo step");
    let new_actors = bench.change_actors().split_off(actors);
    assert_eq!(new_actors, vec!["agent".to_string()], "the change is the agent's, not the window's own `desktop`");
    let steps = result["steps"].as_array().expect("steps");
    assert_eq!(steps.len(), 2);
    assert!(steps[1]["first_frame"].as_u64() > steps[0]["last_frame"].as_u64());
    assert_eq!(result["settled"], true);
    assert!(!bench.window.app.ui_test.as_ref().expect("test mode").busy(), "the run ended: the window's own edits are its own again");
}

#[test]
fn keys_and_an_ime_composition_type_into_the_inline_editor_as_typing_does() {
    let mut bench = Bench::new(true);
    let layout = bench.layout();
    let row = layout.layer_rows.iter().find(|row| row.node_id.as_deref() == Some("t1") && row.visible).expect("t1 has a layer row");
    let [x, y] = centre(row.rect);
    let calls = bench.window.calls.len();
    let revision = bench.revision();
    bench.input(json!([
        { "type": "click", "x": x, "y": y, "after_frames": 2 },
        { "type": "key", "key": "Enter", "after_frames": 2 },
        { "type": "ime_enabled" },
        { "type": "ime_preedit", "text": "ㅎ", "after_frames": 2 },
        { "type": "ime_commit", "text": "하", "after_frames": 3 },
        { "type": "ime_disabled", "after_frames": 2 },
        { "type": "key", "key": "Escape", "after_frames": 3 },
    ]));
    assert_eq!(bench.text("t1"), "가하", "the committed syllable is inserted once, at the caret");
    assert_eq!(bench.revision(), revision + 1, "one IME commit, one change");
    // The edit the run started is the agent's (81): its reservation and its commit name `<run actor>:inline:<view>`.
    let sent = &bench.window.calls[calls..];
    let owner = sent.iter().find(|(method, _, _)| method == "targets.reserve").map(|(_, params, _)| params["owner"].clone()).expect("the IME reserves the node");
    let actor = sent.iter().find(|(method, _, _)| method == "changes.apply").map(|(_, params, _)| params["actor"].clone()).expect("the commit");
    assert!(owner.as_str().is_some_and(|owner| owner.starts_with("agent:inline:")), "{owner}");
    assert_eq!(owner, actor, "the commit is made by the reservation's owner");
    assert!(bench.change_actors().last().is_some_and(|last| last.starts_with("agent:inline:")));
    for _ in 0..10 {
        bench.frame();
    }
    assert_eq!((bench.text("t1"), bench.revision()), ("가하".to_string(), revision + 1), "Escape commits nothing more");
}

#[test]
fn a_wheel_with_ctrl_and_a_middle_drag_reach_the_board() {
    let mut bench = Bench::new(true);
    let layout = bench.layout();
    let board = layout.panels.iter().find(|panel| panel.id == "board").expect("the board").rect;
    let [x, y] = centre(board);
    let pan = bench.window.app.view_ref().pan;
    let revision = bench.revision();
    bench.input(json!([{ "type": "drag", "from": [x, board.y + board.height - 10.0], "to": [x + 60.0, board.y + board.height - 40.0], "button": "middle" }]));
    assert_ne!(bench.window.app.view_ref().pan, pan, "a middle drag pans");
    assert_eq!(bench.revision(), revision, "and changes nothing");
    let zoom = bench.window.app.view_ref().zoom;
    bench.input(json!([{ "type": "wheel", "x": x, "y": y, "dy": 120.0, "modifiers": { "ctrl": true } }]));
    assert_ne!(bench.window.app.view_ref().zoom, zoom, "Ctrl + wheel zooms the board");
}

#[test]
fn a_table_reports_one_rectangle_per_cell_inside_its_frame() {
    let mut bench = Bench::new(true);
    bench.window.step(&mut bench.host, "canvas.table.add", json!({ "rows": 2, "columns": 3 })).expect("a table is added");
    for _ in 0..4 {
        bench.frame();
    }
    let layout = bench.layout();
    let table = layout.cells.first().expect("the table's cells are in the layout").table_id.clone();
    let frame = node_rect(&layout, &table);
    let cells: Vec<_> = layout.cells.iter().filter(|cell| cell.table_id == table).collect();
    let places: std::collections::BTreeSet<(u32, u32)> = cells.iter().map(|cell| (cell.row, cell.column)).collect();
    assert_eq!((cells.len(), places.len()), (6, 6), "one rectangle per cell: {cells:?}");
    for cell in cells {
        let rect = cell.rect;
        assert!(
            rect.x >= frame.x - 0.5 && rect.y >= frame.y - 0.5 && rect.x + rect.width <= frame.x + frame.width + 0.5 && rect.y + rect.height <= frame.y + frame.height + 0.5,
            "{cell:?} lies in the table {frame:?}"
        );
        assert_eq!(rect, node_rect(&layout, &cell.cell_id), "a cell is the frame of its cell node");
    }
}

/// A canvas whose first page holds two rectangles and a text box.
fn canvas(host: &mut Host) -> String {
    let id = host.create("canvas", "시험용 입력");
    let ArtifactBody::Canvas(document) = host.artifact(&id).body else { panic!("a canvas") };
    let page_id = document.pages[0].id.clone();
    let nodes = [
        json!({ "id": "r1", "kind": "rectangle", "bounds": { "x": 1_000_000, "y": 1_000_000, "width": 3_000_000, "height": 2_000_000 } }),
        json!({ "id": "r2", "kind": "rectangle", "bounds": { "x": 5_000_000, "y": 1_000_000, "width": 3_000_000, "height": 2_000_000 } }),
        json!({ "id": "t1", "kind": "text", "text": "가", "bounds": { "x": 1_000_000, "y": 4_000_000, "width": 3_000_000, "height": 600_000 } }),
    ];
    let operations = nodes
        .into_iter()
        .map(|node| designllm_contracts::Operation::AddNode {
            page_id: page_id.clone(),
            parent_id: None,
            node: serde_json::from_value(node).expect("a node parses"),
        })
        .collect();
    host.apply(&id, operations, [designllm_contracts::Intent::Content, designllm_contracts::Intent::Design].into_iter().collect()).expect("the nodes are added");
    id
}

/// Item 8 through real approvals: the model-run confirmation an agent's `home.create` opens refuses a synthetic run before it
/// plays, and its buttons are reported as human only; an agent's proposal row in the AI tab ends a run that presses its
/// apply button, or activates it from the keyboard while it has the focus.
#[test]
fn real_approvals_refuse_synthetic_presses_and_keys() {
    let mut bench = Bench::new(true);
    bench.window.app.ai_model = "claude-opus-5-5".into();
    let all = ["content", "design", "data"];
    // An agent's proposal on the canvas: the AI tab lists it with its apply button.
    let revision = bench.revision();
    let workspace = bench.host.workspace_id.clone();
    let change = super::test_support::change_set_arguments(
        &workspace,
        &bench.canvas,
        revision,
        &[designllm_contracts::Intent::Content],
        vec![designllm_contracts::Operation::SetText { node_id: "t1".into(), text: "제안".into() }],
    );
    bench.host.call("proposal.create", json!({ "change_set": change })).expect("a proposal");
    for _ in 0..20 {
        bench.frame();
    }
    let layout = bench.layout();
    let [x, y] = centre(layout.controls.iter().find(|control| control.label == "AI" && control.visible).expect("the AI tab").rect);
    bench.input(json!([{ "type": "click", "x": x, "y": y, "after_frames": 4 }]));
    let layout = bench.layout();
    let apply = layout.controls.iter().find(|control| control.label == "적용" && control.human_only).expect("the apply button, human only").clone();
    assert!(!layout.controls.iter().any(|control| control.label == "적용" && !control.human_only), "reported once, marked");
    let [ax, ay] = centre(apply.rect);
    let run = bench.run(UI_INPUT_COMMAND, json!({ "events": [{ "type": "click", "x": ax, "y": ay }] }), "agent_start", &all);
    assert_eq!(run["status"], "failed", "{run}");
    assert_eq!(run["error"]["diagnostics"][0]["code"], "HUMAN_ONLY");
    assert_eq!(bench.text("t1"), "가", "the proposal is not applied");
    // Focused (as Tab would leave it), the button is not activated by a synthetic Enter or Space either.
    let id = bench.window.app.ui_test.as_ref().expect("test mode").human_only_ids().into_iter().find(|(label, _)| label == "적용").expect("its id").1;
    for key in ["Enter", "Space"] {
        bench.window.ctx.memory_mut(|memory| memory.request_focus(id));
        bench.frame();
        let run = bench.run(UI_INPUT_COMMAND, json!({ "events": [{ "type": "key", "key": key }] }), "agent_start", &all);
        assert_eq!(run["error"]["diagnostics"][0]["code"], "HUMAN_ONLY", "{key}: {run}");
    }
    assert_eq!(bench.text("t1"), "가");

    // An agent asks for a model run from the home screen: the confirmation is open, so no synthetic run starts.
    bench.window.step(&mut bench.host, "nav.home", json!({})).expect("home");
    for _ in 0..4 {
        bench.frame();
    }
    let asked = bench.queue("home.create", json!({ "tile": "blank", "prompt": "모델 실행 요청" }), "agent_start", &[]).expect("queued");
    for _ in 0..6 {
        bench.frame();
    }
    let asked_run = bench.host.call("ui.command.get", json!({ "run_id": asked })).expect("the run");
    assert!(bench.window.app.has_home_confirmation(), "the real confirmation is on screen: {asked_run}");
    let layout = bench.layout();
    assert!(layout.controls.iter().any(|control| control.label == "모델 실행" && control.human_only), "its button is reported, human only");
    let run = bench.run(UI_INPUT_COMMAND, json!({ "events": [{ "type": "key", "key": "Enter" }] }), "agent_start", &all);
    assert_eq!(run["status"], "failed", "{run}");
    assert_eq!(run["error"]["diagnostics"][0]["code"], "HUMAN_ONLY", "{run}");
    assert!(bench.window.app.has_home_confirmation(), "still waiting for a person");
    let pending = bench.host.call("ui.command.get", json!({ "run_id": asked })).expect("the run");
    assert_eq!(pending["status"], "delivered", "the model run is not approved");
}

/// Task 69 r3: a UI-test window starts no model run but the mock runtime, from the home prompt (Enter), the AI tab
/// (Ctrl+Enter) or a figure's AI request, by synthetic input as by a person.
#[test]
fn a_test_window_starts_no_real_model_run() {
    let mut bench = Bench::new(true);
    // A model is chosen, so that nothing but the test window's rule stops the run.
    bench.window.app.ai_model = "claude-opus-5-5".into();
    let all = ["content", "design", "data"];
    let sent = |bench: &Bench, from: usize, method: &str| bench.window.calls[from..].iter().any(|(sent, _, _)| sent == method);
    // The AI tab: type a request and press Ctrl+Enter.
    let layout = bench.layout();
    let [x, y] = centre(layout.controls.iter().find(|control| control.label == "AI" && control.visible).expect("the AI tab").rect);
    bench.input(json!([{ "type": "click", "x": x, "y": y, "after_frames": 4 }]));
    let layout = bench.layout();
    let field = layout.controls.iter().find(|control| control.label == "요청" && control.visible).expect("the request field").rect;
    let [fx, fy] = centre(field);
    let calls = bench.window.calls.len();
    bench.run(UI_INPUT_COMMAND, json!({ "events": [
        { "type": "click", "x": fx, "y": fy, "after_frames": 2 },
        { "type": "text", "text": "상자를 파랗게", "after_frames": 2 },
        { "type": "key", "key": "Enter", "modifiers": { "ctrl": true }, "after_frames": 4 },
    ] }), "agent_start", &all);
    assert!(!sent(&bench, calls, "job.start"), "no job is started");
    assert_eq!(bench.window.message().as_deref(), Some(super::ui_test_input::REAL_RUN_REFUSED));

    // The home prompt: type and press Enter.
    bench.window.step(&mut bench.host, "nav.home", json!({})).expect("home");
    for _ in 0..4 {
        bench.frame();
    }
    let layout = bench.layout();
    let prompt = layout.controls.iter().find(|control| control.label == "무엇을 만들까요?").expect("the home prompt").rect;
    let [px, py] = centre(prompt);
    let calls = bench.window.calls.len();
    bench.run(UI_INPUT_COMMAND, json!({ "events": [
        { "type": "click", "x": px, "y": py, "after_frames": 2 },
        { "type": "text", "text": "포스터", "after_frames": 2 },
        { "type": "key", "key": "Enter", "after_frames": 4 },
    ] }), "agent_start", &all);
    assert!(!sent(&bench, calls, "job.start") && !sent(&bench, calls, "artifact.create"), "nothing is created or started");
    assert_eq!(bench.window.message().as_deref(), Some(super::ui_test_input::REAL_RUN_REFUSED));

    // A figure's AI request from its AI tab, in a window of its own (the refused home request above is a recoverable draft).
    let mut bench = Bench::new(true);
    bench.window.app.ai_model = "claude-opus-5-5".into();
    let figure = bench.host.create_figure();
    let summary = bench.host.call("workspace.list", json!({})).expect("the list")["artifacts"]
        .as_array()
        .and_then(|all| all.iter().find(|item| item["id"] == figure.as_str()).cloned())
        .expect("the figure is listed");
    bench.window.app.activate(serde_json::from_value(summary).expect("a summary"));
    for _ in 0..6 {
        bench.frame();
    }
    let layout = bench.layout();
    let [x, y] = centre(layout.controls.iter().find(|control| control.label == "AI" && control.visible).expect("the AI tab").rect);
    bench.input(json!([{ "type": "click", "x": x, "y": y, "after_frames": 4 }]));
    let layout = bench.layout();
    let field = layout.controls.iter().find(|control| control.label == "요청" && control.visible).expect("the request field").rect;
    let [fx, fy] = centre(field);
    let calls = bench.window.calls.len();
    bench.run(UI_INPUT_COMMAND, json!({ "events": [
        { "type": "click", "x": fx, "y": fy, "after_frames": 2 },
        { "type": "text", "text": "그림을 다듬어", "after_frames": 2 },
        { "type": "key", "key": "Enter", "modifiers": { "ctrl": true }, "after_frames": 4 },
    ] }), "agent_start", &all);
    let figure_calls: Vec<String> = bench.window.calls[calls..].iter().map(|(method, _, _)| method.clone()).filter(|method| method != "events.poll" && method != "ui.state.publish").collect();
    assert!(!sent(&bench, calls, "job.start"), "no figure job is started: {figure_calls:?}");
    assert_eq!(bench.window.message().as_deref(), Some(super::ui_test_input::REAL_RUN_REFUSED), "{figure_calls:?}");
    // The mock runtime is not refused.
    assert_eq!(bench.window.app.ui_test_run_refusal("job.start", &json!({ "runtime": "mock" })), None);
    assert!(bench.window.app.ui_test_run_refusal("job.start", &json!({ "runtime": "claude" })).is_some());
}

/// U09: a queued dialog answer stands in for the image picker while the run plays; its core request carries the agent path
/// mark; an answer the agent path rules refuse fails the run before anything is read.
#[test]
fn a_dialog_answer_inserts_an_image_and_a_refused_answer_changes_nothing() {
    let mut bench = Bench::new(true);
    let all = ["content", "design", "data"];
    let dir = std::env::temp_dir().join(format!("designllm-ui-dialog-{}", designllm_contracts::new_id()));
    std::fs::create_dir_all(&dir).expect("a folder");
    let png = dir.join("picture.png");
    image::save_buffer_with_format(&png, &[255u8; 16], 2, 2, image::ColorType::Rgba8, image::ImageFormat::Png).expect("a png");
    let layout = bench.layout();
    let [x, y] = centre(node_rect(&layout, "r2"));
    let nodes = |bench: &mut Bench| {
        let ArtifactBody::Canvas(document) = bench.host.artifact(&bench.canvas).body else { panic!("a canvas") };
        document.pages[0].nodes.len()
    };
    let before = nodes(&mut bench);
    let calls = bench.window.calls.len();
    let events = json!([
        { "type": "click", "x": x, "y": y, "after_frames": 3 },
        { "type": "key", "key": "K", "modifiers": { "ctrl": true, "shift": true }, "after_frames": 3 },
    ]);
    let run = bench.run(UI_INPUT_COMMAND, json!({ "events": events, "dialog_answers": [png.to_string_lossy()] }), "agent_start", &all);
    assert_eq!(run["status"], "completed", "{run}");
    assert_eq!(run["result"]["dialog_answers_left"], 0, "the picker took the answer");
    let import = bench.window.calls[calls..].iter().find(|(method, _, _)| method == "asset.import").expect("the picture is imported").1.clone();
    assert_eq!(import["path"], png.to_string_lossy().as_ref());
    assert_eq!(import[designllm_contracts::local_path::AGENT_PATH_PARAM], true, "the agent path rules apply");
    assert_eq!(nodes(&mut bench), before + 1, "the picture is on the page");

    let revision = bench.revision();
    // Neither an open file nor a save target (task 68r): a folder that does not exist, a UNC path.
    for bad in [r"\\server\share\picture.png".to_owned(), dir.join("no-folder").join("picture.png").to_string_lossy().into_owned()] {
        let run = bench.run(UI_INPUT_COMMAND, json!({ "events": events, "dialog_answers": [bad] }), "agent_start", &all);
        assert_eq!(run["status"], "failed", "{run}");
        assert_eq!(run["error"]["code"], "INVALID_ARGUMENT", "{run}");
    }
    assert_eq!(bench.revision(), revision, "a refused answer changes nothing");
    // A save target (a new file in an existing folder) passes the start check. The picture dialog that takes it applies the
    // open rule again: the file does not exist, so the dialog is cancelled and nothing is imported.
    let saved = dir.join("saved.png");
    let imports = |bench: &mut Bench| bench.window.calls.iter().filter(|(method, _, _)| method == "asset.import").count();
    let imported = imports(&mut bench);
    let run = bench.run(UI_INPUT_COMMAND, json!({ "events": events, "dialog_answers": [saved.to_string_lossy()] }), "agent_start", &all);
    assert_eq!(run["status"], "completed", "{run}");
    assert_eq!(run["result"]["dialog_answers_left"], 0, "the picker took the answer");
    assert_eq!(imports(&mut bench), imported, "the open dialog refuses a path that is not an existing file");
    assert_eq!(bench.revision(), revision, "a cancelled picker changes nothing");
    // After the run the picker is the native one again (refused in tests): the shortcut inserts nothing.
    assert_eq!(crate::dialogs::set_test_answers(None), 0);
    std::fs::remove_dir_all(dir).expect("cleaned");
}

/// U07 through the window: an agent change to the node arrives while the IME composes in it; the committed syllable is kept
/// once and the agent's change does not overwrite the composition.
#[test]
fn an_agent_change_during_a_composition_keeps_the_typed_syllable_once() {
    let mut bench = Bench::new(true);
    let layout = bench.layout();
    let row = layout.layer_rows.iter().find(|row| row.node_id.as_deref() == Some("t1") && row.visible).expect("t1 has a layer row");
    let [x, y] = centre(row.rect);
    bench.input(json!([
        { "type": "click", "x": x, "y": y, "after_frames": 2 },
        { "type": "key", "key": "Enter", "after_frames": 2 },
        { "type": "ime_enabled" },
        { "type": "ime_preedit", "text": "ㅎ", "after_frames": 2 },
    ]));
    // The agent writes the node while the composition holds it.
    let revision = bench.revision();
    let workspace_id = bench.host.workspace_id.clone();
    let change = json!({
        "request_id": designllm_contracts::new_id(), "actor": "mcp:other:agent", "workspace_id": workspace_id,
        "artifact_id": bench.canvas, "base_revision": revision, "allowed_intents": ["content"],
        "operations": [{ "op": "set_text", "node_id": "t1", "text": "에이전트" }],
    });
    let agent = bench.host.call("changes.apply", change);
    bench.input(json!([
        { "type": "ime_commit", "text": "하", "after_frames": 3 },
        { "type": "key", "key": "Escape", "after_frames": 3 },
    ]));
    for _ in 0..10 {
        bench.frame();
    }
    let text = bench.text("t1");
    assert_eq!(text.matches('하').count(), 1, "the syllable is kept once: {text:?} (agent change: {agent:?})");
    assert!(agent.is_err(), "the composition's reservation keeps the agent off the node: {agent:?}");
    assert_eq!(text, "가하");
}

/// U06 (68-13) through the window: a double click on a layer row opens its name field, and IME text there renames that
/// row's node only.
#[test]
fn a_double_click_on_a_layer_row_renames_that_node_with_ime_text() {
    let mut bench = Bench::new(true);
    let layout = bench.layout();
    let row = |layout: &UiLayout, id: &str| layout.layer_rows.iter().find(|row| row.node_id.as_deref() == Some(id) && row.visible).expect("a layer row").rect;
    let [tx, ty] = centre(row(&layout, "t1"));
    let [rx, ry] = centre(row(&layout, "r1"));
    bench.input(json!([
        { "type": "click", "x": tx, "y": ty, "after_frames": 3 },
        { "type": "click", "x": rx, "y": ry, "count": 2, "after_frames": 4 },
    ]));
    assert!(bench.window.app.layer_rename.is_some(), "the double click opens the name field");
    bench.input(json!([
        { "type": "key", "key": "A", "modifiers": { "ctrl": true }, "after_frames": 2 },
        { "type": "ime_enabled" },
        { "type": "ime_preedit", "text": "테", "after_frames": 2 },
        { "type": "ime_commit", "text": "테스트", "after_frames": 3 },
        { "type": "key", "key": "Enter", "after_frames": 4 },
    ]));
    let name = |bench: &mut Bench, id: &str| {
        let ArtifactBody::Canvas(document) = bench.host.artifact(&bench.canvas).body else { panic!("a canvas") };
        document.pages[0].nodes.iter().find(|node| node.id == id).and_then(|node| node.ext.name.clone())
    };
    assert_eq!(name(&mut bench, "r1").as_deref(), Some("테스트"));
    assert_ne!(name(&mut bench, "t1").as_deref(), Some("테스트"));
}



/// U07 order: an inline edit stays open after an IME commit and Escape (also after ime_disabled), so the next work does not
/// open and the window state keeps changing. Kept as a product follow-up.
#[test]
fn escape_after_an_ime_commit_ends_the_inline_edit() {
    let mut bench = Bench::new(true);
    let layout = bench.layout();
    let row = layout.layer_rows.iter().find(|row| row.node_id.as_deref() == Some("t1") && row.visible).expect("t1 has a layer row");
    let [x, y] = centre(row.rect);
    bench.input(json!([
        { "type": "click", "x": x, "y": y, "after_frames": 2 },
        { "type": "key", "key": "Enter", "after_frames": 2 },
        { "type": "ime_enabled" },
        { "type": "ime_preedit", "text": "ㅎ", "after_frames": 2 },
        { "type": "ime_commit", "text": "하", "after_frames": 3 },
        { "type": "ime_disabled", "after_frames": 2 },
        { "type": "key", "key": "Escape", "after_frames": 6 },
    ]));
    for _ in 0..10 {
        bench.frame();
    }
    assert!(bench.window.app.inline_text_edit.is_none(), "Escape ends the inline edit");
}

fn key_event(key: Key, modifiers: Modifiers, repeat: bool) -> Event {
    Event::Key { key, physical_key: Some(key), pressed: true, repeat, modifiers }
}

fn key(key: Key) -> Event {
    key_event(key, Modifiers::NONE, false)
}

fn arrow(key: Key, shift: bool) -> Event {
    key_event(key, if shift { Modifiers::SHIFT } else { Modifiers::NONE }, false)
}

/// A key the system repeats while it is held.
fn repeated(key: Key) -> Event {
    key_event(key, Modifiers::NONE, true)
}

fn point(rect: designllm_contracts::UiPointRect) -> Pos2 {
    let [x, y] = centre(rect);
    Pos2::new(x, y)
}

fn lock(bench: &mut Bench, id: &str) {
    let canvas = bench.canvas.clone();
    let operation = designllm_contracts::Operation::SetNodeAppearance { node_ids: vec![id.into()], image_fit: None, locked: Some(true) };
    bench.host.apply(&canvas, vec![operation], [designllm_contracts::Intent::Design].into_iter().collect()).expect("the node is locked");
    bench.window.app.refresh_active();
    for _ in 0..3 {
        bench.frame();
    }
}

/// Task 68 item 15 through the window: an arrow key moves the selected node 1 px (9,525 EMU) on a canvas, Shift+arrow 10 px, one
/// change a press (so one undo step each); while a text edit has the keys, the arrows move no node.
#[test]
fn arrow_keys_move_the_selected_node_one_px_and_ten_with_shift() {
    let mut bench = Bench::new(true);
    let layout = bench.layout();
    let [x, y] = centre(node_rect(&layout, "r1"));
    bench.input(json!([{ "type": "click", "x": x, "y": y, "after_frames": 2 }]));
    assert_eq!(bench.window.app.selected_nodes, vec!["r1".to_string()]);
    let start = bench.bounds("r1");
    let revision = bench.revision();
    bench.input(json!([{ "type": "key", "key": "ArrowRight", "after_frames": 2 }]));
    let one = bench.bounds("r1");
    assert_eq!((one.x - start.x, one.y), (EMU_PER_CSS_PX, start.y), "one press moves right by 1 px");
    assert_eq!(bench.revision(), revision + 1, "one press is one change");
    bench.input(json!([{ "type": "key", "key": "ArrowRight", "modifiers": { "shift": true }, "after_frames": 2 }]));
    let ten = bench.bounds("r1");
    assert_eq!(ten.x - one.x, 10 * EMU_PER_CSS_PX, "Shift moves 10 px");
    bench.input(json!([{ "type": "key", "key": "ArrowUp", "after_frames": 2 }]));
    let up = bench.bounds("r1");
    assert_eq!((up.x, up.y), (ten.x, ten.y - EMU_PER_CSS_PX), "Up moves up by 1 px");
    assert_eq!(bench.revision(), revision + 3, "each press is its own change");
    // Undo runs one change back per press: the first Up comes back first.
    let run = bench.run("edit.undo", json!({}), "change", &["content", "design", "data"]);
    assert_eq!(run["status"], "completed", "{run}");
    assert_eq!(bench.bounds("r1"), ten, "one undo step takes back one press");
    // While a text edit has the keys, the arrows move no node (the text keeps them).
    let [tx, ty] = centre(layout.layer_rows.iter().find(|row| row.node_id.as_deref() == Some("t1") && row.visible).expect("t1 has a layer row").rect);
    bench.input(json!([
        { "type": "click", "x": tx, "y": ty, "after_frames": 2 },
        { "type": "key", "key": "Enter", "after_frames": 2 },
    ]));
    assert!(bench.window.app.inline_text_edit.is_some(), "Enter starts the text edit");
    let text_revision = bench.revision();
    let text_bounds = bench.bounds("t1");
    bench.input(json!([{ "type": "key", "key": "ArrowRight", "after_frames": 2 }]));
    assert_eq!(bench.revision(), text_revision, "the arrow keys move no node while text is edited");
    assert_eq!(bench.bounds("t1"), text_bounds);
}

/// A slide's unit is px too: a press moves 9,525 EMU and Shift 95,250.
#[test]
fn arrow_keys_move_a_slide_node_one_px_and_ten_with_shift() {
    let mut host = Host::start();
    let slides = host.create("slides", "방향키");
    let ArtifactBody::Slides(document) = host.artifact(&slides).body else { panic!("slides") };
    let node = serde_json::from_value(json!({
        "id": "s1", "kind": "text", "text": "가", "bounds": { "x": 1_000_000, "y": 1_000_000, "width": 2_000_000, "height": 600_000 }
    }))
    .expect("a node parses");
    let operation = designllm_contracts::Operation::AddNode { page_id: document.pages[0].id.clone(), parent_id: None, node };
    host.apply(&slides, vec![operation], [designllm_contracts::Intent::Content].into_iter().collect()).expect("the node is added");
    let mut bench = Bench::open(host, slides, false);
    bench.step("select.node", json!({ "node_id": "s1" }));
    let start = bench.bounds("s1");
    bench.keys(vec![arrow(Key::ArrowRight, false)]);
    assert_eq!(bench.bounds("s1").x - start.x, EMU_PER_CSS_PX, "one press moves 1 px");
    bench.keys(vec![arrow(Key::ArrowDown, true)]);
    let moved = bench.bounds("s1");
    assert_eq!((moved.x - start.x, moved.y - start.y), (EMU_PER_CSS_PX, 10 * EMU_PER_CSS_PX), "Shift+Down moves 10 px down");
    assert_eq!(bench.window.message(), None);
}

/// The arrows are the selection's only on the editor screen with nothing else open: on the home screen, with a dialog or a
/// menu open, or while a button or a field has the keyboard, no node moves. A focused widget keeps the arrow: egui moves the
/// button's focus, the field its caret.
#[test]
fn arrow_keys_move_nothing_on_the_home_screen_or_with_a_dialog_a_menu_or_a_focused_widget() {
    let mut bench = Bench::new(true);
    bench.step("select.node", json!({ "node_id": "r1" }));
    let start = bench.bounds("r1");
    let revision = bench.revision();
    let unmoved = |bench: &mut Bench, case: &str| {
        assert_eq!(bench.revision(), revision, "{case}: no change is sent");
        assert_eq!(bench.bounds("r1"), start, "{case}: the node stays");
        assert_eq!(bench.window.message(), None, "{case}: no notice");
    };
    // The home screen, with the document's selection kept behind it.
    bench.window.app.screen = super::Screen::Home;
    bench.frame();
    bench.keys(vec![arrow(Key::ArrowRight, false)]);
    unmoved(&mut bench, "home");
    bench.window.app.screen = super::Screen::Editor;
    bench.frame();
    // A dialog: the chart form's window.
    bench.window.app.figure_form = Some(super::FigureForm::Chart);
    bench.frame();
    bench.keys(vec![arrow(Key::ArrowRight, false)]);
    unmoved(&mut bench, "dialog");
    bench.window.app.figure_form = None;
    bench.frame();
    // A menu: the zoom menu.
    egui::Popup::open_id(&bench.window.ctx, egui::Id::new("zoom-menu"));
    bench.frame();
    assert!(egui::Popup::is_any_open(&bench.window.ctx), "the zoom menu is open");
    bench.keys(vec![arrow(Key::ArrowRight, false)]);
    unmoved(&mut bench, "menu");
    egui::Popup::close_all(&bench.window.ctx);
    bench.frame();
    // A widget Tab gave the keyboard to.
    bench.keys(vec![key(Key::Tab)]);
    assert!(bench.window.ctx.memory(|memory| memory.focused()).is_some(), "Tab gives a widget the keyboard");
    bench.window.step(&mut bench.host, "select.node", json!({ "node_id": "r1" })).expect("r1 is selected again");
    bench.keys(vec![arrow(Key::ArrowRight, false)]);
    assert!(bench.window.ctx.memory(|memory| memory.focused()).is_some(), "the arrow is egui's: a widget keeps the keyboard");
    unmoved(&mut bench, "focused widget");
    bench.keys(vec![key(Key::Escape)]);
    // A properties field with the keyboard: its X field.
    bench.step("select.node", json!({ "node_id": "r1" }));
    bench.frame();
    let field = egui::Id::new(("node-field", "r1", "x".to_owned()));
    assert!(bench.window.ctx.read_response(field).is_some(), "the properties panel shows the X field");
    // The field lies below the 800-point window's fold here, so the keyboard is given to it as a click on it would.
    bench.window.ctx.memory_mut(|memory| memory.request_focus(field));
    bench.frame();
    assert!(bench.window.ctx.wants_keyboard_input(), "the X field has the keyboard");
    bench.keys(vec![arrow(Key::ArrowLeft, false)]);
    assert!(bench.window.ctx.memory(|memory| memory.has_focus(field)), "the field keeps the keyboard");
    unmoved(&mut bench, "properties field");
}

/// Of a selection with a locked node only the unlocked ones move; a selection of locked nodes only moves nothing, and the notice
/// says so once for that selection, not on every press.
#[test]
fn arrow_keys_leave_locked_nodes_and_say_so_once_when_only_locked_ones_are_selected() {
    let mut bench = Bench::new(true);
    lock(&mut bench, "r2");
    bench.step("select.many", json!({ "node_ids": ["r1", "r2"] }));
    let (r1, r2, revision) = (bench.bounds("r1"), bench.bounds("r2"), bench.revision());
    bench.keys(vec![arrow(Key::ArrowRight, false)]);
    assert_eq!(bench.bounds("r1").x - r1.x, EMU_PER_CSS_PX, "the unlocked node moves");
    assert_eq!(bench.bounds("r2"), r2, "the locked node stays");
    assert_eq!(bench.revision(), revision + 1, "one change");
    assert_eq!(bench.window.message(), None, "a selection with an unlocked node moves without a notice");
    bench.step("select.node", json!({ "node_id": "r2" }));
    let revision = bench.revision();
    bench.keys(vec![arrow(Key::ArrowRight, false)]);
    assert_eq!(bench.window.message().as_deref(), Some(super::nudge_keys::LOCKED_NUDGE), "the first press says why");
    bench.window.app.notices.clear();
    for _ in 0..3 {
        bench.keys(vec![arrow(Key::ArrowLeft, false)]);
    }
    assert_eq!(bench.window.message(), None, "the notice is not repeated for the same selection");
    assert_eq!(bench.revision(), revision, "no change is sent for a locked node");
    assert_eq!(bench.bounds("r2"), r2);
}

/// A held key repeats faster than the host answers: presses made while the last change and its reads are on their way go out
/// together once the window shows that change, so none is lost and none is sent against an old revision.
#[test]
fn held_arrow_keys_wait_for_the_last_change_and_send_no_conflicting_change() {
    let mut bench = Bench::new(true);
    bench.step("select.node", json!({ "node_id": "r1" }));
    let (start, revision) = (bench.bounds("r1"), bench.revision());
    bench.frame_with(vec![arrow(Key::ArrowRight, false)], false);
    for _ in 0..3 {
        bench.frame_with(vec![repeated(Key::ArrowRight)], false);
    }
    for _ in 0..4 {
        bench.frame();
    }
    assert_eq!(bench.bounds("r1").x - start.x, 4 * EMU_PER_CSS_PX, "every press arrives");
    assert_eq!(bench.revision(), revision + 2, "the first press, then the three made while it was on its way");
    assert_eq!(bench.window.message(), None, "no refusal");
    assert!(bench.window.app.held_edits.is_empty(), "no change is held back");
}

/// Esc in an inline text edit ends the edit and keeps the selection. An edit the editor cannot end on Esc (here a conflict) keeps
/// its draft on Esc, and the selection stays its node, as `select.clear` keeps it during an edit; the conflict notice decides.
#[test]
fn escape_ends_an_inline_edit_and_keeps_the_draft_of_an_edit_in_conflict() {
    let mut bench = Bench::new(true);
    bench.step("select.node", json!({ "node_id": "t1" }));
    bench.keys(vec![key(Key::Enter)]);
    assert!(bench.window.app.inline_text_edit.is_some(), "Enter starts the text edit");
    bench.keys(vec![key(Key::Escape)]);
    for _ in 0..4 {
        bench.frame();
    }
    assert!(bench.window.app.inline_text_edit.is_none(), "Escape ends the edit");
    assert_eq!(bench.window.app.selected_nodes, vec!["t1".to_string()], "and keeps the selection");
    bench.keys(vec![key(Key::Enter)]);
    bench.keys(vec![Event::Text("나".into())]);
    let draft = bench.window.app.text_draft.clone();
    assert!(draft.contains('나'), "the typed text is in the draft: {draft:?}");
    bench.window.app.inline_text_edit.as_mut().expect("the edit opens again").conflict = Some("시험 충돌".into());
    bench.frame();
    bench.keys(vec![key(Key::Escape)]);
    assert!(bench.window.app.inline_text_edit.as_ref().is_some_and(|edit| edit.conflict.is_some()), "the edit in conflict stays");
    assert_eq!(bench.window.app.text_draft, draft, "with its draft");
    assert_eq!(bench.window.app.selected_nodes, vec!["t1".to_string()], "and the selection stays its node");
}

/// A layer row's double click is two clicks on its own area within the system's double-click time with nothing between: a third
/// click right after, a click on the row's lock icon or on the board between, and a slow second click open no name field.
#[test]
fn a_layer_row_double_click_needs_two_clicks_on_it_with_no_click_between() {
    let mut bench = Bench::new(true);
    lock(&mut bench, "r2");
    let rows = bench.window.app.ui_layout().layer_rows;
    let row = |id: &str| rows.iter().find(|row| row.node_id.as_deref() == Some(id) && row.visible).unwrap_or_else(|| panic!("{id} has a layer row")).rect;
    let (r1, r2) = (row("r1"), row("r2"));
    // The lock icon sits 8 points in from the row's right end.
    let lock_icon = Pos2::new(r2.x + r2.width - 16.0, r2.y + r2.height / 2.0);
    let page = bench.window.app.ui_layout().pages[0].rect;
    let board = Pos2::new(page.x + page.width - 12.0, page.y + page.height - 12.0);
    let gap = super::sidebar::double_click_secs();
    let renaming = |bench: &Bench| bench.window.app.layer_rename.is_some();
    bench.click_at(point(r1));
    bench.click_at(point(r1));
    assert!(renaming(&bench), "two clicks on the row open its name field");
    bench.window.app.layer_rename = None;
    bench.click_at(point(r1));
    assert!(!renaming(&bench), "a third click is a single click");
    bench.clock += gap + 0.5;
    bench.click_at(point(r2));
    bench.click_at(lock_icon);
    assert!(bench.window.app.selected_nodes == vec!["r2".to_string()], "the lock icon acts on its row");
    bench.click_at(point(r2));
    assert!(!renaming(&bench), "a click on the row's icon between the two makes no double click");
    bench.clock += gap + 0.5;
    bench.click_at(point(r1));
    bench.click_at(board);
    bench.click_at(point(r1));
    assert!(!renaming(&bench), "a click on the board between the two makes no double click");
    bench.clock += gap + 0.5;
    bench.click_at(point(r1));
    bench.clock += gap + 0.05;
    bench.click_at(point(r1));
    assert!(!renaming(&bench), "a second click after the double-click time is a single click");
    bench.clock += gap + 0.5;
    bench.click_at(point(r1));
    bench.click_at(point(r1));
    assert!(renaming(&bench), "two quick clicks still open it");
}

/// 68f item 1: Ctrl + wheel over a node (a region the board's widgets lay over it) zooms the board about the pointer,
/// as over empty board. egui hovers only the topmost interactive widget under the pointer, so a region over the board
/// takes the board's hover.
#[test]
fn ctrl_wheel_over_a_node_zooms_the_board_about_the_pointer() {
    let mut bench = Bench::new(true);
    let layout = bench.layout();
    let [x, y] = centre(node_rect(&layout, "r1"));
    let zoom = bench.window.app.view_ref().zoom;
    bench.input(json!([{ "type": "wheel", "x": x, "y": y, "dy": 120.0, "modifiers": { "ctrl": true } }]));
    assert_ne!(bench.window.app.view_ref().zoom, zoom, "Ctrl + wheel over the node r1 zooms the board");
}

/// 68f item 1: Ctrl + wheel over the board zooms the board, not the app (egui's zoom factor, the UI scale, stays); over a
/// panel it does neither, and the plain wheel over a panel leaves the board where it is. The plain wheel over a node pans the
/// board, as over empty board (design tools do).
#[test]
fn the_wheel_moves_the_board_only_over_the_board_and_never_the_ui_scale() {
    let mut bench = Bench::new(true);
    let layout = bench.layout();
    let scale = bench.window.ctx.zoom_factor();
    let [x, y] = centre(node_rect(&layout, "r1"));
    let zoom = bench.window.app.view_ref().zoom;
    bench.input(json!([{ "type": "wheel", "x": x, "y": y, "dy": 120.0, "modifiers": { "ctrl": true } }]));
    assert_ne!(bench.window.app.view_ref().zoom, zoom, "Ctrl + wheel over the board zooms it");
    assert_eq!(bench.window.ctx.zoom_factor(), scale, "the UI scale stays");
    let pan = bench.window.app.view_ref().pan;
    bench.input(json!([{ "type": "wheel", "x": x, "y": y, "dy": -60.0 }]));
    assert_ne!(bench.window.app.view_ref().pan, pan, "the wheel over a node pans the board");
    // egui spreads a wheel step over the next frames (smooth scrolling): it ends before the panels are tried.
    for _ in 0..40 {
        bench.frame();
    }
    let panels: Vec<_> = layout.panels.iter().filter(|panel| matches!(panel.id.as_str(), "sidebar" | "properties")).cloned().collect();
    assert!(!panels.is_empty(), "a panel is open: {:?}", layout.panels);
    for panel in panels {
        let [x, y] = centre(panel.rect);
        let (zoom, pan) = (bench.window.app.view_ref().zoom, bench.window.app.view_ref().pan);
        bench.input(json!([{ "type": "wheel", "x": x, "y": y, "dy": 120.0, "modifiers": { "ctrl": true } }, { "type": "wheel", "x": x, "y": y, "dy": -60.0 }]));
        assert_eq!(bench.window.app.view_ref().zoom, zoom, "Ctrl + wheel over the {} panel leaves the board's zoom", panel.id);
        assert_eq!(bench.window.app.view_ref().pan, pan, "the wheel over the {} panel leaves the board", panel.id);
        assert_eq!(bench.window.ctx.zoom_factor(), scale, "the UI scale stays");
    }
}

/// The canvas templates a person starts from (home.use_template).
const CANVAS_TEMPLATES: [&str; 8] = ["mobile_app", "login", "dashboard", "landing_page", "social_post", "poster_a3", "invitation", "banner"];

/// The id of the node that holds `id` directly, on the open canvas.
fn container_of(bench: &Bench, id: &str) -> Option<String> {
    fn walk(nodes: &[designllm_contracts::Node], id: &str, parent: Option<&str>) -> Option<Option<String>> {
        nodes.iter().find_map(|node| {
            if node.id == id {
                Some(parent.map(str::to_owned))
            } else {
                walk(&node.children, id, Some(&node.id))
            }
        })
    }
    let artifact = bench.window.app.artifact.as_ref()?;
    let ArtifactBody::Canvas(document) = &artifact.body else { return None };
    document.pages.iter().find_map(|page| walk(&page.nodes, id, None)).flatten()
}

/// Whether `id` holds other nodes, on the open canvas.
fn has_children(bench: &Bench, id: &str) -> bool {
    bench.window.app.artifact.as_ref().and_then(|artifact| super::doc_tree::find_node(artifact, id)).is_some_and(|node| !node.children.is_empty())
}

/// 68f item 2, sweep: every visible object of every canvas template that holds no other object, clicked in turn without
/// clearing the selection (a click on another object while one is selected), selects that object or the container that holds it
/// directly; nothing else. Not clicked: an object whose centre another object drawn after it covers (the click is that one's).
/// Where the selection's floating toolbar lies over the object (it flips below a selection near the page title) the toolbar
/// takes the click, as it would a person's: the selection is cleared first, so the click reaches the board.
/// The clicks come 60 ms apart: egui counts two clicks within 0.3 s as a double click wherever they land, and the board
/// takes one only where the click before it landed (`board_double_click`), so a quick click on another object selects it.
#[test]
fn a_click_on_every_visible_object_of_each_canvas_template_selects_it_or_its_container() {
    let mut bench = Bench::new(true);
    let mut wrong: Vec<String> = Vec::new();
    let mut clicked = 0;
    for template in CANVAS_TEMPLATES {
        bench.step("home.use_template", json!({ "template": template }));
        for _ in 0..10 {
            bench.frame();
        }
        let layout = bench.layout();
        let contains = |rect: designllm_contracts::UiPointRect, [x, y]: [f32; 2]| x > rect.x && x < rect.x + rect.width && y > rect.y && y < rect.y + rect.height;
        for (index, node) in layout.nodes.iter().enumerate() {
            let at = centre(node.rect);
            let covered = layout.nodes[index + 1..].iter().any(|later| later.visible && contains(later.rect, at));
            if !node.visible || covered || has_children(&bench, &node.node_id) {
                continue;
            }
            if bench.window.ctx.layer_id_at(Pos2::from(at)) != Some(egui::LayerId::background()) {
                bench.step("select.clear", json!({}));
            }
            bench.click_at(Pos2::from(at));
            bench.frame();
            clicked += 1;
            let selected = bench.window.app.selected_nodes.clone();
            let container = container_of(&bench, &node.node_id);
            if selected != [node.node_id.clone()] && container.as_ref().is_none_or(|container| selected != [container.clone()]) {
                wrong.push(format!("{template}: {} (container {container:?}) -> {selected:?}", node.node_id));
            }
        }
    }
    assert!(clicked > 40, "the templates hold objects to click ({clicked})");
    assert!(wrong.is_empty(), "clicks that selected nothing or another object: {wrong:#?}");
}

/// 68f item 2, early clicks: a click on a template's objects in the first frames after it opens (the fit and the zoom
/// settle still run), before the board is still.
#[test]
fn a_click_right_after_a_canvas_template_opens_selects_the_object() {
    let mut bench = Bench::new(true);
    let mut empty: Vec<String> = Vec::new();
    for template in CANVAS_TEMPLATES {
        bench.step("home.use_template", json!({ "template": template }));
        let layout = bench.layout();
        let nodes: Vec<_> = layout.nodes.iter().filter(|node| node.visible).take(5).cloned().collect();
        for node in nodes {
            bench.click_at(Pos2::from(centre(node.rect)));
            if bench.window.app.selected_nodes.is_empty() {
                empty.push(format!("{template}: {} (page {})", node.node_id, node.page_index));
            }
            bench.step("select.clear", json!({}));
        }
    }
    assert!(empty.is_empty(), "clicks that selected nothing: {empty:#?}");
}

/// 68f item 2, slow host: the click's reply waits one frame (what the frame sent is answered later), with a pan before it
/// so the page transform moves between the click and its reply.
#[test]
fn a_click_whose_reply_is_late_after_a_pan_still_selects_the_object() {
    let mut bench = Bench::new(true);
    let mut empty: Vec<String> = Vec::new();
    for template in CANVAS_TEMPLATES {
        bench.step("home.use_template", json!({ "template": template }));
        for _ in 0..10 {
            bench.frame();
        }
        let layout = bench.layout();
        let board = layout.panels.iter().find(|panel| panel.id == "board").expect("the board").rect;
        let nodes: Vec<_> = layout.nodes.iter().filter(|node| node.visible).take(6).cloned().collect();
        for node in nodes {
            let at = Pos2::from(centre(node.rect));
            // A small middle-button pan first (the transform moves), then the click, whose reply is late.
            let from = Pos2::new(board.x + board.width / 2.0, board.y + board.height - 10.0);
            bench.frame_with(vec![Event::PointerMoved(from), Event::PointerButton { pos: from, button: egui::PointerButton::Middle, pressed: true, modifiers: Modifiers::NONE }], true);
            bench.frame_with(vec![Event::PointerMoved(from + egui::vec2(0.0, -30.0)), Event::PointerButton { pos: from, button: egui::PointerButton::Middle, pressed: false, modifiers: Modifiers::NONE }], true);
            bench.frame_with(vec![Event::PointerMoved(at), Event::PointerButton { pos: at, button: egui::PointerButton::Primary, pressed: true, modifiers: Modifiers::NONE }, Event::PointerButton { pos: at, button: egui::PointerButton::Primary, pressed: false, modifiers: Modifiers::NONE }], false);
            bench.frame();
            bench.frame();
            if bench.window.app.selected_nodes.is_empty() {
                empty.push(format!("{template}: {} (page {})", node.node_id, node.page_index));
            }
            bench.step("select.clear", json!({}));
        }
    }
    assert!(empty.is_empty(), "clicks that selected nothing: {empty:#?}");
}

/// 68f item 2, the zoom race: a click whose reply comes after the board zoomed (the page transform moved, and the page is
/// drawn again at the new zoom's dpi) selects what was clicked. The reply is about the revision on screen, so it answers the
/// click; asking again with the click's old screen point would hit what the zoomed board shows there now.
#[test]
fn a_click_answered_after_the_board_zoomed_selects_what_was_clicked() {
    let mut bench = Bench::new(true);
    bench.step("home.use_template", json!({ "template": "dashboard" }));
    for _ in 0..10 {
        bench.frame();
    }
    let layout = bench.layout();
    let board = layout.panels.iter().find(|panel| panel.id == "board").expect("the board").rect;
    let board_rect = Rect::from_min_size(Pos2::new(board.x, board.y), egui::vec2(board.width, board.height));
    let node = layout.nodes.iter().filter(|node| node.visible && node.node_id.ends_with(".value")).last().expect("a metric value").clone();
    let at = Pos2::from(centre(node.rect));
    bench.click_at(at);
    let clicked = bench.window.app.selected_nodes.clone();
    assert!(!clicked.is_empty(), "a click on {} selects it", node.node_id);
    bench.step("select.clear", json!({}));
    for _ in 0..4 {
        bench.frame();
    }
    let hits_before = bench.window.calls.iter().filter(|(method, _, _)| method == "render.hit_test").count();
    // The click goes out; before its reply the board zooms threefold about its top left corner and draws a frame.
    bench.frame_with(vec![Event::PointerMoved(at), primary_button(at, true), primary_button(at, false)], false);
    bench.window.app.zoom_about(board_rect, board_rect.min, 3.0);
    bench.frame_with(Vec::new(), false);
    for _ in 0..6 {
        bench.frame();
    }
    assert_eq!(bench.window.app.selected_nodes, clicked, "the late reply selects what the click was on");
    let hits = bench.window.calls.iter().filter(|(method, _, _)| method == "render.hit_test").count() - hits_before;
    assert_eq!(hits, 1, "the reply is not sent again with the click's old point");
}

/// Whether a `ui.input` run plays or settles in the bench's window.
fn busy(bench: &Bench) -> bool {
    bench.window.app.ui_test.as_ref().is_some_and(UiTestInput::busy)
}

fn save_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("designllm-ui-{name}-{}", designllm_contracts::new_id()));
    std::fs::create_dir_all(&dir).expect("a folder");
    dir
}

/// Task 68r: a save dialog answer marks every core request that writes to it `agent_path`, however late it leaves. Here the run
/// ends (`settle_timeout_ms` 1) and its pin runs out before the export's dry run is answered: the write leaves after both.
#[test]
fn a_save_answer_keeps_the_agent_path_mark_after_the_run_and_its_pin_ended() {
    use designllm_contracts::local_path::AGENT_PATH_PARAM;
    let mut bench = Bench::new(true);
    let dir = save_dir("save-late");
    let out = dir.join("out.png");
    // The export popover, PNG, then `내보내기…` (the scenario 68r-save-dialog-answer.json).
    let control = |bench: &mut Bench, label: &str| {
        let layout = bench.layout();
        centre(layout.controls.iter().find(|control| control.label == label && control.visible).unwrap_or_else(|| panic!("{label}")).rect)
    };
    for label in ["내보내기", "PNG"] {
        let [x, y] = control(&mut bench, label);
        bench.input(json!([{ "type": "click", "x": x, "y": y, "after_frames": 4 }]));
    }
    let [x, y] = control(&mut bench, "내보내기…");
    let calls = bench.window.calls.len();
    let args = json!({
        "events": [{ "type": "click", "x": x, "y": y, "after_frames": 1 }],
        "settle": true, "settle_timeout_ms": 1, "dialog_answers": [out.to_string_lossy()],
    });
    let run_id = bench.queue(UI_INPUT_COMMAND, args, "agent_start", &["content", "design", "data"]).expect("the run is queued");
    for _ in 0..50 {
        if busy(&bench) {
            break;
        }
        bench.frame();
    }
    assert!(busy(&bench), "the run plays");
    // The host answers nothing while the run plays and settles: the dry run waits for it.
    for _ in 0..200 {
        if !busy(&bench) {
            break;
        }
        bench.frame_with(Vec::new(), false);
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(!busy(&bench), "the run ended before the dry run was answered");
    std::thread::sleep(Duration::from_millis(20));
    let run = bench.finish(&run_id);
    assert_eq!(run["status"], "completed", "{run}");
    assert_eq!(run["result"]["dialog_answers_left"], 0, "the save dialog took the answer");
    let exports: Vec<&Value> = bench.window.calls[calls..].iter().filter(|(method, _, _)| method == "artifact.export").map(|(_, params, _)| params).collect();
    assert_eq!(exports.len(), 2, "a dry run, then the write: {exports:?}");
    assert_eq!(exports[0]["dry_run"], true, "{exports:?}");
    assert!(exports[1].get("dry_run").is_none(), "the second request writes: {exports:?}");
    for export in exports {
        assert_eq!(export["target_path"], out.to_string_lossy().as_ref());
        assert_eq!(export[AGENT_PATH_PARAM], true, "the agent's path keeps the agent path rules: {export}");
    }
    assert!(std::fs::read(&out).expect("the file is written").starts_with(b"\x89PNG"), "a PNG");
    std::fs::remove_dir_all(dir).expect("cleaned");
}

/// Task 68r: export losses a person accepts write where the save answer said, and the write keeps the agent path mark; a Save
/// As answer marks its save and the plain saves to the same place after it. No run plays and no pin is left here, and the
/// window's own export elsewhere is not marked.
#[test]
fn a_save_answer_marks_the_write_after_a_loss_confirmation_and_later_saves() {
    use super::test_support::{methods, Harness};
    use designllm_contracts::local_path::AGENT_PATH_PARAM;
    let dir = save_dir("save-losses");
    let out = dir.join("out.pdf");
    let mut harness = Harness::new();
    harness.open_document("canvas", 5);
    harness.app.ui_test = Some(UiTestInput::default());
    harness.sent();
    crate::dialogs::set_test_answers(Some([out.clone()].into_iter().collect()));
    harness.app.invoke_ui("file.export", json!({ "format": "pdf" }));
    let dry = harness.sent();
    assert_eq!(methods(&dry), ["artifact.export"]);
    assert_eq!(dry[0].2["dry_run"], true);
    assert_eq!(dry[0].2[AGENT_PATH_PARAM], true, "the dry run names the agent's path");
    let losses = json!([{ "code": "font", "message": "글꼴 대체", "node_id": null, "blocking": true }]);
    let dry_run = json!({
        "dry_run": true, "artifact_id": "a1", "revision": 5, "format": "pdf", "mime": "application/pdf", "bytes": 10,
        "editability": "vector", "losses": losses, "diagnostics": [],
    });
    harness.answer(&dry, "artifact.export", Ok(dry_run));
    let confirmation = harness.app.export_confirmation.as_mut().expect("the losses wait for a person");
    assert_eq!(confirmation.request.target_path, out.to_string_lossy().as_ref());
    // A person allows the loss and accepts it: the place is still the agent's.
    confirmation.allowed.insert("font".into());
    harness.app.apply_export_losses();
    let write = harness.sent();
    assert_eq!(methods(&write), ["artifact.export"]);
    assert!(write[0].2.get("dry_run").is_none(), "the write");
    assert_eq!(write[0].2["target_path"], out.to_string_lossy().as_ref());
    assert_eq!(write[0].2[AGENT_PATH_PARAM], true, "a loss a person accepts keeps the agent path rules of the place");

    let document = dir.join("doc.json");
    crate::dialogs::set_test_answers(Some([document.clone()].into_iter().collect()));
    harness.app.invoke_ui("file.save_as", json!({}));
    harness.app.invoke_ui("file.save", json!({}));
    let saves = harness.sent();
    assert_eq!(methods(&saves), ["artifact.save", "artifact.save"], "Save As, then a plain save to the same place");
    for save in &saves {
        assert_eq!(save.2["target_path"], document.to_string_lossy().as_ref());
        assert_eq!(save.2["overwrite"], false);
        assert_eq!(save.2[AGENT_PATH_PARAM], true, "{}", save.2);
    }
    assert_eq!(crate::dialogs::set_test_answers(None), 0);

    harness.app.export_path = dir.join("own.pdf").to_string_lossy().into_owned();
    harness.app.export_active();
    let own = harness.sent();
    assert_eq!(methods(&own), ["artifact.export"]);
    assert!(own[0].2.get(AGENT_PATH_PARAM).is_none(), "the window's own export is not marked: {}", own[0].2);
    std::fs::remove_dir_all(dir).expect("cleaned");
}

/// Task 68r: while a run plays in a visible test window, the save dialog a person opens (their own Ctrl+Shift+S) is the native
/// one (refused in tests): it takes none of the run's answers, and nothing is saved.
#[test]
fn a_save_dialog_a_person_opens_during_a_run_takes_no_queued_answer() {
    let mut bench = Bench::new(true);
    let dir = save_dir("save-person");
    let target = dir.join("agent.json");
    let args = json!({ "events": [{ "type": "wait", "frames": 30 }], "dialog_answers": [target.to_string_lossy()] });
    let run_id = bench.queue(UI_INPUT_COMMAND, args, "agent_start", &["content", "design", "data"]).expect("the run is queued");
    for _ in 0..50 {
        if busy(&bench) {
            break;
        }
        bench.frame();
    }
    assert!(busy(&bench), "the run plays");
    let calls = bench.window.calls.len();
    let ctrl_shift = Modifiers { ctrl: true, shift: true, command: true, ..Modifiers::NONE };
    bench.keys(vec![key_event(Key::S, ctrl_shift, false)]);
    let run = bench.finish(&run_id);
    assert_eq!(run["status"], "completed", "{run}");
    assert_eq!(run["result"]["dialog_answers_left"], 1, "the person's dialog took no answer: {run}");
    assert!(!bench.window.calls[calls..].iter().any(|(method, _, _)| method == "artifact.save"), "nothing is saved");
    assert!(!target.exists());
    std::fs::remove_dir_all(dir).expect("cleaned");
}

/// Task 68r: a save answer that names a file which exists does not overwrite it: the save the window sends (overwrite false,
/// marked `agent_path`) is refused ALREADY_EXISTS, and the file keeps its bytes.
#[test]
fn a_save_answer_naming_an_existing_file_is_refused_and_the_file_is_kept() {
    use designllm_contracts::local_path::AGENT_PATH_PARAM;
    let mut bench = Bench::new(true);
    let dir = save_dir("save-existing");
    let existing = dir.join("old.json");
    std::fs::write(&existing, b"keep").expect("a file");
    let calls = bench.window.calls.len();
    let args = json!({
        "events": [{ "type": "key", "key": "S", "modifiers": { "ctrl": true, "shift": true }, "after_frames": 4 }],
        "dialog_answers": [existing.to_string_lossy()],
    });
    let run = bench.run(UI_INPUT_COMMAND, args, "agent_start", &["content", "design", "data"]);
    assert_eq!(run["status"], "completed", "{run}");
    assert_eq!(run["result"]["dialog_answers_left"], 0, "Save As took the answer");
    let (_, save, answer) = bench.window.calls[calls..].iter().find(|(method, _, _)| method == "artifact.save").expect("Save As sends a save");
    assert_eq!(save["target_path"], existing.to_string_lossy().as_ref());
    assert_eq!(save["overwrite"], false);
    assert_eq!(save[AGENT_PATH_PARAM], true, "{save}");
    assert_eq!(*answer, Value::Null, "the host refused the save");
    let mut again = save.clone();
    again["request_id"] = json!(designllm_contracts::new_id());
    let error = bench.host.call("artifact.save", again).expect_err("the same save is refused");
    assert_eq!(error.code, ErrorCode::AlreadyExists, "{error:?}");
    assert_eq!(std::fs::read(&existing).expect("the file"), b"keep", "the file is not overwritten");
    std::fs::remove_dir_all(dir).expect("cleaned");
}
