//! The path form's `add.path` handler: SVG path data and a view box become one Path node. The window's change sets go to
//! the real host, so an accepted path is what the host stores, a refused one sends no change set, and undo and redo
//! take the insert back and forth through the core history.

use std::sync::mpsc::{Receiver, Sender};

use designllm_contracts::{ArtifactBody, ArtifactSummary, Node, NodeKind, PathSpec, Rect, PATH_MAX_COMMANDS};
use eframe::egui::Context;
use serde_json::{json, Value};

use crate::{
    app::{
        figure_flow_tests::Host,
        kind_commands::parse_view_box,
        test_support::settle_work,
        tests::screen,
        tools::{path_form_draft_id, path_form_open_id, PathDraft},
        DesktopApp,
    },
    rpc::{RpcPayload, RpcReply, RpcWorker, Work},
    theme,
};

const TRIANGLE: &str = "M 0 0 L 24 0 L 12 24 Z";
/// The view box `0 0 24 24` in PathSpec units.
const VIEW_BOX: Rect = Rect { x: 0, y: 0, width: 24_000, height: 24_000 };

/// A window on `canvas` whose RPC calls are recorded and answered by the host.
struct Window {
    app: DesktopApp,
    replies: Sender<RpcReply>,
    work: Receiver<Work>,
    ctx: Context,
    calls: Vec<(String, Value)>,
}

impl Window {
    fn open(host: &mut Host, canvas: &str) -> Self {
        let (rpc, replies, work) = RpcWorker::detached();
        let ctx = Context::default();
        theme::install(&ctx);
        let mut window = Self { app: DesktopApp::new(rpc, "작업공간".into(), None, None), replies, work, ctx, calls: Vec::new() };
        window.settle(host);
        let list = host.call("workspace.list", json!({})).expect("the work list");
        let summary = list["artifacts"]
            .as_array()
            .and_then(|all| all.iter().find(|item| item["id"] == canvas))
            .cloned()
            .expect("the canvas is in the work list");
        window.app.activate(serde_json::from_value::<ArtifactSummary>(summary).expect("a summary"));
        window.settle(host);
        for _ in 0..3 {
            let _ = window.ctx.run(screen(1.0, 1280.0, 800.0), |ctx| {
                window.app.draw_frame(ctx);
            });
        }
        window.settle(host);
        window
    }

    /// Plays the host's side until the window has nothing left to ask.
    fn settle(&mut self, host: &mut Host) {
        settle_work(&self.work, &mut self.app, &self.ctx, |item| {
            if let Work::Call { ticket, method, params } = item {
                self.calls.push((method.clone(), params.clone()));
                let result = host.call(&method, params).map(RpcPayload::Json);
                self.replies.send(RpcReply { ticket, result }).expect("the window listens");
            }
        });
    }

    /// Runs one command as the window does, then plays the host's side of what it sent.
    fn invoke(&mut self, host: &mut Host, id: &str, args: Value) -> Result<(), String> {
        let result = self.app.invoke(id, args);
        self.settle(host);
        result
    }

    /// The `changes.apply` calls sent so far, in order.
    fn change_sets(&self) -> Vec<Value> {
        self.calls.iter().filter(|(method, _)| method == "changes.apply").map(|(_, params)| params.clone()).collect()
    }
}

/// The first page's nodes as the host stores them now.
fn page_nodes(host: &mut Host, canvas: &str) -> Vec<Node> {
    match host.artifact(canvas).body {
        ArtifactBody::Canvas(document) => document.pages.into_iter().next().expect("a page").nodes,
        _ => panic!("a canvas"),
    }
}

/// The form's entries as the test puts them in, with the view box `0 0 24 24`.
fn draft(d: &str) -> PathDraft {
    PathDraft { d: d.into(), view_box: "0 0 24 24".into(), error: None }
}

/// The form's draft of the document `artifact_id` as the window's memory holds it now.
fn form_draft(window: &Window, artifact_id: &str) -> Option<PathDraft> {
    window.ctx.data(|data| data.get_temp::<PathDraft>(path_form_draft_id(artifact_id)))
}

#[test]
fn a_path_insert_sends_the_parsed_path_as_a_content_change() {
    let mut host = Host::start();
    let canvas = host.create("canvas", "경로");
    let mut window = Window::open(&mut host, &canvas);

    window.app.add_path(&json!({ "d": TRIANGLE, "view_box": "0 0 24 24" })).expect("the path is accepted");
    window.settle(&mut host);

    let changes = window.change_sets();
    assert_eq!(changes.len(), 1, "one change set");
    assert_eq!(changes[0]["allowed_intents"], json!(["content"]));
    let operation = &changes[0]["operations"][0];
    assert_eq!(operation["op"], "add_node");
    assert_eq!(operation["node"]["kind"], "path");
    let expected = PathSpec::from_svg(TRIANGLE, VIEW_BOX).expect("valid path data");
    assert_eq!(operation["node"]["path"], serde_json::to_value(&expected).expect("a path serializes"));

    let id = operation["node"]["id"].as_str().expect("the new node's id").to_owned();
    let stored = page_nodes(&mut host, &canvas);
    let node = stored.iter().find(|node| node.id == id).expect("the host stores the node");
    assert_eq!(node.kind, NodeKind::Path);
    assert_eq!(node.ext.path, Some(expected));
}

#[test]
fn the_box_keeps_the_view_box_proportions() {
    let mut host = Host::start();
    let canvas = host.create("canvas", "비율");
    let mut window = Window::open(&mut host, &canvas);

    window.app.add_path(&json!({ "d": "M 0 0 L 48 0 L 48 24 Z", "view_box": "0 0 48 24" })).expect("the path is accepted");
    window.settle(&mut host);

    let changes = window.change_sets();
    let bounds = &changes[0]["operations"][0]["node"]["bounds"];
    assert_eq!(bounds["x"], json!(381_000), "40 px from the left");
    assert_eq!(bounds["y"], json!(381_000), "40 px from the top");
    assert_eq!(bounds["width"], json!(457_200), "the longer side is 48 px");
    assert_eq!(bounds["height"], json!(228_600), "half the width for a 2:1 view box");
}

#[test]
fn a_refused_path_sends_no_change_set_and_names_the_problem_in_korean() {
    let mut host = Host::start();
    let canvas = host.create("canvas", "거부");
    let mut window = Window::open(&mut host, &canvas);
    let revision = host.artifact(&canvas).revision;
    let too_many = format!("M 0 0{}", " L1 1".repeat(4096));

    for (d, view_box, problem) in [
        ("L 1 2", "0 0 24 24", "M 또는 m으로 시작"),
        ("M 0 0 L 1", "0 0 24 24", "숫자가 필요합니다 (입력 끝)"),
        ("", "0 0 24 24", "경로 데이터가 비어 있습니다"),
        ("M 0 0 L 1 X", "0 0 24 24", "숫자가 필요합니다 (11번째 글자)"),
        ("M 0 0 L 1 1 X", "0 0 24 24", "알 수 없는 명령 글자입니다 (13번째 글자)"),
        ("M 0 0 L 1 1,", "0 0 24 24", "쉼표는 인자 사이에"),
        ("M 0 0 L 1 1 Z 2", "0 0 24 24", "Z 명령 뒤에는"),
        ("M 100001 0 L 1 1", "0 0 24 24", "좌표가 허용 범위(±100000)를 벗어났습니다"),
        (too_many.as_str(), "0 0 24 24", "명령은 4096개까지 쓸 수 있습니다"),
        ("M 0 0 L 1 1", "0 0 0 24", "0보다 커야"),
        ("M 0 0 L 1 1", "0 0 24", "네 개"),
        ("M 0 0 L 1 1", "0 0 24 nan", "네 개"),
        ("M 0 0 L 1 1", "1e300 0 24 24", "viewBox 값은 ±100000 이하여야 합니다"),
    ] {
        let error = window.app.add_path(&json!({ "d": d, "view_box": view_box })).expect_err("the path is refused");
        assert!(error.contains(problem), "{d:?} / {view_box:?}: {error}");
        assert!(error.chars().any(|c| ('\u{AC00}'..='\u{D7A3}').contains(&c)), "the reason is Korean: {error}");
    }
    window.settle(&mut host);

    assert!(window.change_sets().is_empty(), "a refused path sends nothing");
    assert_eq!(host.artifact(&canvas).revision, revision, "the canvas is unchanged");
}

#[test]
fn the_longest_path_is_placed_and_one_command_more_is_refused_before_any_change_set() {
    let mut host = Host::start();
    let canvas = host.create("canvas", "한계");
    let mut window = Window::open(&mut host, &canvas);

    let longest = format!("M 0 0{}", " L1 1".repeat(4095));
    window.app.add_path(&json!({ "d": longest, "view_box": "0 0 24 24" })).expect("4096 commands are accepted");
    window.settle(&mut host);
    assert_eq!(window.change_sets().len(), 1, "the longest path is one change set");
    let stored = page_nodes(&mut host, &canvas);
    let node = stored.iter().find(|node| node.kind == NodeKind::Path).expect("the path is stored");
    assert_eq!(node.ext.path.as_ref().map(|path| path.commands.len()), Some(4096));

    let one_more = format!("M 0 0{}", " L1 1".repeat(4096));
    let error = window.app.add_path(&json!({ "d": one_more, "view_box": "0 0 24 24" })).expect_err("4097 commands are refused");
    assert!(error.contains("4096개까지"), "{error}");
    window.settle(&mut host);
    assert_eq!(window.change_sets().len(), 1, "the refused path sends no change set");
}

#[test]
fn commas_between_arguments_place_the_same_path_as_spaces() {
    let mut host = Host::start();
    let canvas = host.create("canvas", "쉼표");
    let mut window = Window::open(&mut host, &canvas);

    window.app.add_path(&json!({ "d": "M 0,0 L 24,0 L 12,24 Z", "view_box": "0 0 24 24" })).expect("one comma between arguments is accepted");
    window.settle(&mut host);

    let changes = window.change_sets();
    assert_eq!(changes.len(), 1);
    let expected = PathSpec::from_svg(TRIANGLE, VIEW_BOX).expect("valid path data");
    assert_eq!(changes[0]["operations"][0]["node"]["path"], serde_json::to_value(&expected).expect("a path serializes"));
}

#[test]
fn an_inserted_path_undoes_and_redoes_through_the_core_history() {
    let mut host = Host::start();
    let canvas = host.create("canvas", "되돌리기");
    let mut window = Window::open(&mut host, &canvas);
    window.app.add_path(&json!({ "d": TRIANGLE, "view_box": "0 0 24 24" })).expect("the path is accepted");
    window.settle(&mut host);
    let changes = window.change_sets();
    let id = changes[0]["operations"][0]["node"]["id"].as_str().expect("the new node's id").to_owned();
    let has_node = |host: &mut Host| page_nodes(host, &canvas).iter().any(|node| node.id == id);
    assert!(has_node(&mut host));

    window.invoke(&mut host, "edit.undo", json!({})).expect("undo runs");
    assert!(!has_node(&mut host), "undo takes the path back");

    window.invoke(&mut host, "edit.redo", json!({})).expect("redo runs");
    let restored = page_nodes(&mut host, &canvas);
    let node = restored.iter().find(|node| node.id == id).expect("redo restores the node");
    assert_eq!(node.ext.path, Some(PathSpec::from_svg(TRIANGLE, VIEW_BOX).expect("valid path data")));
}

#[test]
fn a_view_box_takes_spaces_or_commas_and_needs_a_positive_size_within_the_limit() {
    assert_eq!(parse_view_box("0 0 24 24"), Ok(Rect { x: 0, y: 0, width: 24_000, height: 24_000 }));
    assert_eq!(parse_view_box(" 0,0 , 24.5 24 "), Ok(Rect { x: 0, y: 0, width: 24_500, height: 24_000 }));
    assert_eq!(parse_view_box("-100000 -100000 100000 100000"), Ok(Rect { x: -100_000_000, y: -100_000_000, width: 100_000_000, height: 100_000_000 }), "the limit is kept");
    assert!(parse_view_box("0 0 24").is_err(), "three numbers");
    assert!(parse_view_box("0 0 24 nan").is_err(), "not a finite number");
    assert!(parse_view_box("0 0 -24 24").is_err(), "negative width");
    assert!(parse_view_box("0 0 0.0001 24").is_err(), "rounds to a zero width");
    assert!(parse_view_box("1e300 0 24 24").is_err(), "saturated before the limit check");
    assert!(parse_view_box("-1e300 0 24 24").is_err(), "a large negative value");
    assert!(parse_view_box("0 0 100000.0006 24").is_err(), "rounds past the limit");
}

#[test]
fn a_path_longer_than_the_old_form_limit_is_placed_whole() {
    let mut host = Host::start();
    let canvas = host.create("canvas", "긴 경로");
    let mut window = Window::open(&mut host, &canvas);
    // 4096 commands of long numbers: about 295 000 characters, past the 262 144 the form used to cut at.
    let segment = "C-123.456789,-123.456789,-123.456789,-123.456789,-123.456789,-123.456789";
    let long = format!("M0 0{}", segment.repeat(PATH_MAX_COMMANDS - 1));
    assert!(long.chars().count() > 262_144);

    window.app.add_path(&json!({ "d": long, "view_box": "0 0 24 24" })).expect("the whole path is accepted");
    window.settle(&mut host);

    let changes = window.change_sets();
    assert_eq!(changes.len(), 1, "one change set");
    let expected = PathSpec::from_svg(&long, VIEW_BOX).expect("valid path data");
    assert_eq!(expected.commands.len(), PATH_MAX_COMMANDS);
    assert_eq!(changes[0]["operations"][0]["node"]["path"], serde_json::to_value(&expected).expect("a path serializes"));
}

#[test]
fn a_path_past_one_million_characters_is_refused_whole_and_not_cut() {
    let mut host = Host::start();
    let canvas = host.create("canvas", "너무 긴 경로");
    let mut window = Window::open(&mut host, &canvas);
    let long = format!("M 0 0 L 1 1{}", " ".repeat(1_048_577 - 11));
    assert_eq!(long.chars().count(), 1_048_577);

    let error = window.app.add_path(&json!({ "d": long, "view_box": "0 0 24 24" })).expect_err("too long");
    assert!(error.contains("1,048,576자"), "{error}");
    window.settle(&mut host);
    assert!(window.change_sets().is_empty(), "nothing is sent");
}

#[test]
fn a_path_on_slides_is_refused_before_any_change_set() {
    let mut host = Host::start();
    let slides = host.create("slides", "슬라이드");
    let mut window = Window::open(&mut host, &slides);

    let error = window.app.add_path(&json!({ "d": TRIANGLE, "view_box": "0 0 24 24" })).expect_err("slides take no path");
    assert!(error.contains("슬라이드에는 경로를 넣을 수 없습니다"), "{error}");
    window.settle(&mut host);
    assert!(window.change_sets().is_empty(), "nothing is sent");
}

#[test]
fn a_placed_path_clears_the_form_text_when_the_host_answers() {
    let mut host = Host::start();
    let canvas = host.create("canvas", "확인");
    let mut window = Window::open(&mut host, &canvas);
    window.ctx.data_mut(|data| data.insert_temp(path_form_draft_id(&canvas), draft(TRIANGLE)));

    window.app.add_path(&json!({ "d": TRIANGLE, "view_box": "0 0 24 24" })).expect("the path is sent");
    assert!(form_draft(&window, &canvas).is_some(), "the text stays in the form until the host answers");
    window.settle(&mut host);

    assert_eq!(page_nodes(&mut host, &canvas).len(), 1, "the host placed it");
    assert!(form_draft(&window, &canvas).is_none(), "the placed text is cleared");
}

#[test]
fn a_refused_path_puts_its_text_back_into_the_form_with_the_reason() {
    let mut host = Host::start();
    let canvas = host.create("canvas", "거절");
    let mut window = Window::open(&mut host, &canvas);
    // Another window changes the canvas, so this window's change set is based on a revision the host has moved past.
    let mut other = Window::open(&mut host, &canvas);
    other.app.add_path(&json!({ "d": "M 0 0 L 2 2", "view_box": "0 0 24 24" })).expect("the other window's path is sent");
    other.settle(&mut host);

    window.ctx.data_mut(|data| data.insert_temp(path_form_draft_id(&canvas), draft(TRIANGLE)));
    window.app.add_path(&json!({ "d": TRIANGLE, "view_box": "0 0 24 24" })).expect("the path is sent");
    window.settle(&mut host);

    assert_eq!(page_nodes(&mut host, &canvas).len(), 1, "only the other window's path is stored");
    let restored = form_draft(&window, &canvas).expect("the text is back in the form");
    assert_eq!(restored.d, TRIANGLE);
    assert!(restored.error.is_some(), "the reason is shown in the form");
    assert_eq!(window.ctx.data(|data| data.get_temp::<bool>(path_form_open_id(&canvas))), Some(true), "the form opens under the path item");
}

#[test]
fn text_typed_after_the_send_stays_whatever_the_host_answers() {
    let mut host = Host::start();
    let canvas = host.create("canvas", "입력");
    let mut window = Window::open(&mut host, &canvas);
    window.ctx.data_mut(|data| data.insert_temp(path_form_draft_id(&canvas), draft(TRIANGLE)));
    window.app.add_path(&json!({ "d": TRIANGLE, "view_box": "0 0 24 24" })).expect("the path is sent");
    // The user types on before the host answers: the placed text is not what is in the form any more, so it stays.
    window.ctx.data_mut(|data| data.insert_temp(path_form_draft_id(&canvas), draft("M 0 0 L 2 2")));
    window.settle(&mut host);

    assert_eq!(page_nodes(&mut host, &canvas).len(), 1, "the host placed the sent path");
    assert_eq!(form_draft(&window, &canvas).map(|kept| kept.d), Some("M 0 0 L 2 2".to_owned()));
}

/// Regression (backlog desktop 20): the form's draft was one for the whole window, so the text typed or sent in one canvas
/// showed in the path form of the next canvas opened, and a refusal put it there. Each document keeps its own draft now.
#[test]
fn the_path_form_of_another_canvas_does_not_show_the_text_of_the_first() {
    let mut host = Host::start();
    let first = host.create("canvas", "첫째");
    let second = host.create("canvas", "둘째");
    let mut window = Window::open(&mut host, &first);
    window.ctx.data_mut(|data| data.insert_temp(path_form_draft_id(&first), draft(TRIANGLE)));
    window.app.add_path(&json!({ "d": TRIANGLE, "view_box": "0 0 24 24" })).expect("the path is sent");
    let list = host.call("workspace.list", json!({})).expect("the work list");
    let summary = list["artifacts"].as_array().and_then(|all| all.iter().find(|item| item["id"] == second)).cloned().expect("listed");
    window.app.activate(serde_json::from_value::<ArtifactSummary>(summary).expect("a summary"));
    window.settle(&mut host);

    assert_eq!(window.app.artifact.as_ref().map(|artifact| artifact.id.as_str()), Some(second.as_str()), "the second canvas is open");
    assert!(form_draft(&window, &second).is_none(), "the second canvas's form is empty");
    assert_ne!(path_form_draft_id(&first), path_form_draft_id(&second));
    assert_ne!(path_form_open_id(&first), path_form_open_id(&second));
}
