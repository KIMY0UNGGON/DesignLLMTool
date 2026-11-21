//! The properties panel's path section. The text its fields show reads back to the stored path, text the parser refuses
//! is named in Korean and sends nothing, a valid apply is one content change whose `expected` is the stored path, undo takes
//! it back, and a draft keeps what the user typed while the stored path changes underneath it.

use std::sync::mpsc::{Receiver, Sender};

use designllm_contracts::{ArtifactBody, ArtifactSummary, Node, PathSpec, Rect};
use eframe::egui::{Context, Id};
use serde_json::{json, Value};

use super::{path_data_text, path_from_fields, refresh_path_draft, view_box_text, NodePathDraft};
use crate::{
    app::{figure_flow_tests::Host, test_support::settle_work, tests::screen, DesktopApp},
    rpc::{RpcPayload, RpcReply, RpcWorker, Work},
    theme,
};

const TRIANGLE: &str = "M 0 0 L 24 0 L 12 24 Z";
const VIEW_BOX: &str = "0 0 24 24";

/// A window on `canvas` whose RPC calls are recorded and answered by the host (the same harness as `path_editor_tests`).
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
            if let Work::Call { ticket, method, params, .. } = item {
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

/// The stored path of one node.
fn stored_path(host: &mut Host, canvas: &str, id: &str) -> Option<PathSpec> {
    page_nodes(host, canvas).into_iter().find(|node| node.id == id).and_then(|node| node.ext.path)
}

/// A canvas with one path inserted through the insert form; returns the window and the path's node id.
fn canvas_with_one_path(host: &mut Host, title: &str) -> (Window, String, String) {
    let canvas = host.create("canvas", title);
    let mut window = Window::open(host, &canvas);
    window.app.add_path(&json!({ "d": TRIANGLE, "view_box": VIEW_BOX })).expect("the path is inserted");
    window.settle(host);
    let id = window.change_sets()[0]["operations"][0]["node"]["id"].as_str().expect("the new node's id").to_owned();
    (window, canvas, id)
}

#[test]
fn the_text_the_fields_show_reads_back_to_the_same_path() {
    for (d, view_box) in [
        (TRIANGLE, VIEW_BOX),
        ("M -1.5 2.25 C 0.001 0 10 10.125 4 -0.25 Z", "-1.5 -2 30.25 40"),
        ("M 0,0 L 1000.5 -0.125 A 10 10 0 0 1 20 0 Z", "0 0 20.001 10"),
    ] {
        let stored = path_from_fields(d, view_box).expect("valid path data");
        let shown = path_from_fields(&path_data_text(&stored), &view_box_text(&stored.view_box)).expect("the shown text parses");
        assert_eq!(shown, stored, "{d:?}");
    }
    assert_eq!(view_box_text(&Rect { x: -1_500, y: 0, width: 24_050, height: 24_000 }), "-1.5 0 24.05 24");
}

#[test]
fn refused_text_is_named_in_korean_with_the_insert_forms_wording() {
    for (d, view_box, problem) in [
        ("L 1 2", "0 0 24 24", "M 또는 m으로 시작"),
        ("M 0 0 X 1", "0 0 24 24", "알 수 없는 명령 글자입니다 (7번째 글자)"),
        ("M 0 0 L 1 X", "0 0 24 24", "숫자가 필요합니다 (11번째 글자)"),
        ("M 0 0 L 1", "0 0 24 24", "숫자가 필요합니다 (입력 끝)"),
        ("M 100001 0 L 1 1", "0 0 24 24", "좌표가 허용 범위(±100000)를 벗어났습니다"),
        ("M 0 0 L 1 1", "0 0 0 24", "0보다 커야"),
        ("M 0 0 L 1 1", "0 0 24", "네 개"),
        ("M 0 0 L 1 1", "1e300 0 24 24", "viewBox 값은 ±100000 이하여야 합니다"),
    ] {
        let error = path_from_fields(d, view_box).expect_err("the text is refused");
        assert!(error.contains(problem), "{d:?} / {view_box:?}: {error}");
        assert!(error.chars().any(|c| ('\u{AC00}'..='\u{D7A3}').contains(&c)), "the reason is Korean: {error}");
    }
}

/// Regression (backlog desktop 20): the fields took path data past the insert form's 1,048,576 characters; the parser alone
/// would accept these spaces. They are refused whole now, in the insert form's words; 1,048,576 characters still pass.
#[test]
fn the_fields_refuse_path_data_past_the_insert_forms_limit() {
    let at_limit = format!("M 0 0 L 1 1{}", " ".repeat(1_048_576 - 11));
    assert!(path_from_fields(&at_limit, VIEW_BOX).is_ok(), "1,048,576 characters are taken");
    let long = format!("{at_limit} ");
    let error = path_from_fields(&long, VIEW_BOX).expect_err("too long");
    assert!(error.contains("1,048,576자"), "{error}");
}

#[test]
fn a_draft_keeps_typed_text_and_follows_the_stored_path_only_while_it_describes_the_old_one() {
    let stored = path_from_fields(TRIANGLE, VIEW_BOX).expect("valid path data");
    let moved = path_from_fields("M 0 0 L 5 5 Z", VIEW_BOX).expect("valid path data");
    let mut draft = NodePathDraft::default();
    refresh_path_draft(&mut draft, 1, &stored);
    assert_eq!(draft.d, path_data_text(&stored), "the first frame shows the stored path");

    // A refused apply leaves the revision alone, so the panel does not refresh the draft and the typed text stays.
    draft.d = "M 0 0 L 2 2 Z".into();
    assert_eq!(draft.revision, Some(1));
    assert_eq!(draft.d, "M 0 0 L 2 2 Z");

    // Another change moves the stored path under the typed text: the typed text stays and the base moves.
    refresh_path_draft(&mut draft, 2, &moved);
    assert_eq!(draft.d, "M 0 0 L 2 2 Z", "typed text is kept");
    assert_eq!(draft.base_d, path_data_text(&moved));

    // Text that spells the old path differently still describes it, so it follows the stored path.
    let mut respelled = NodePathDraft::default();
    refresh_path_draft(&mut respelled, 1, &stored);
    respelled.d = "M0 0L24 0L12 24Z".into();
    refresh_path_draft(&mut respelled, 2, &moved);
    assert_eq!(respelled.d, path_data_text(&moved));
    assert_eq!(respelled.view_box, VIEW_BOX, "the view box follows too");
}

#[test]
fn an_apply_is_one_content_change_with_the_stored_path_as_expected_and_undo_takes_it_back() {
    let mut host = Host::start();
    let (mut window, canvas, id) = canvas_with_one_path(&mut host, "편집");
    let original = stored_path(&mut host, &canvas, &id).expect("the inserted path is stored");

    window.app.props_path(&json!({ "node_id": id, "d": "M 0 0 L 48 0 L 24 48 Z", "view_box": "0 0 48 48" })).expect("the edit is accepted");
    window.settle(&mut host);

    let changes = window.change_sets();
    assert_eq!(changes.len(), 2, "the insert, then the edit");
    assert_eq!(changes[1]["allowed_intents"], json!(["content"]));
    let operation = &changes[1]["operations"][0];
    assert_eq!(operation["op"], "set_node_path");
    assert_eq!(operation["node_id"], json!(id));
    assert_eq!(operation["expected"], serde_json::to_value(&original).expect("a path serializes"));
    let edited = PathSpec::from_svg("M 0 0 L 48 0 L 24 48 Z", Rect { x: 0, y: 0, width: 48_000, height: 48_000 }).expect("valid path data");
    assert_eq!(operation["path"], serde_json::to_value(&edited).expect("a path serializes"));
    assert_eq!(stored_path(&mut host, &canvas, &id), Some(edited));

    window.invoke(&mut host, "edit.undo", json!({})).expect("undo runs");
    assert_eq!(stored_path(&mut host, &canvas, &id), Some(original), "undo restores the path from before the edit");
}

#[test]
fn text_the_parser_refuses_sends_no_change_and_leaves_the_path() {
    let mut host = Host::start();
    let (mut window, canvas, id) = canvas_with_one_path(&mut host, "거부");
    let revision = host.artifact(&canvas).revision;
    let before = stored_path(&mut host, &canvas, &id);

    let error = window.app.props_path(&json!({ "node_id": id, "d": "M 0 0 X 1", "view_box": VIEW_BOX })).expect_err("the data is refused");
    assert!(error.contains("알 수 없는 명령 글자입니다"), "{error}");
    let error = window.app.props_path(&json!({ "node_id": id, "d": TRIANGLE, "view_box": "0 0 0 24" })).expect_err("the view box is refused");
    assert!(error.contains("0보다 커야"), "{error}");
    window.settle(&mut host);

    assert_eq!(window.change_sets().len(), 1, "only the insert was sent");
    assert_eq!(host.artifact(&canvas).revision, revision, "the canvas is unchanged");
    assert_eq!(stored_path(&mut host, &canvas, &id), before);
}

#[test]
fn an_unchanged_path_and_a_node_that_is_not_a_path_send_nothing() {
    let mut host = Host::start();
    let (mut window, _canvas, id) = canvas_with_one_path(&mut host, "같음");
    let error = window.app.props_path(&json!({ "node_id": id, "d": TRIANGLE, "view_box": VIEW_BOX })).expect_err("nothing changed");
    assert!(error.contains("바꿀 것이 없습니다"), "{error}");

    window.invoke(&mut host, "add.rect", json!({})).expect("a rectangle is added");
    let rectangle = window.change_sets()[1]["operations"][0]["node"]["id"].as_str().expect("the rectangle's id").to_owned();
    let error = window.app.props_path(&json!({ "node_id": rectangle, "d": TRIANGLE, "view_box": VIEW_BOX })).expect_err("not a path");
    assert!(error.contains("경로 노드에만"), "{error}");
    window.settle(&mut host);
    assert_eq!(window.change_sets().len(), 2, "only the two inserts were sent");
}

#[test]
fn an_engine_refusal_leaves_the_stored_path() {
    let mut host = Host::start();
    let (mut window, canvas, id) = canvas_with_one_path(&mut host, "잠금");
    window.invoke(&mut host, "select.node", json!({ "node_id": id })).expect("the path is selected");
    window.invoke(&mut host, "edit.lock", json!({})).expect("the path is locked");
    let revision = host.artifact(&canvas).revision;
    let before = stored_path(&mut host, &canvas, &id);

    // A locked node is refused by the engine when the change reaches it; the stored path does not move.
    let _ = window.app.props_path(&json!({ "node_id": id, "d": "M 0 0 L 2 2 Z", "view_box": VIEW_BOX }));
    window.settle(&mut host);

    assert_eq!(host.artifact(&canvas).revision, revision);
    assert_eq!(stored_path(&mut host, &canvas, &id), before);
}

#[test]
fn the_path_section_drafts_the_stored_path_of_the_selected_node() {
    let mut host = Host::start();
    let (mut window, _canvas, id) = canvas_with_one_path(&mut host, "구역");
    window.invoke(&mut host, "select.node", json!({ "node_id": id })).expect("the path is selected");
    for _ in 0..2 {
        let _ = window.ctx.run(screen(1.0, 1280.0, 800.0), |ctx| {
            window.app.draw_frame(ctx);
        });
    }
    window.settle(&mut host);

    let key = Id::new(("node-path-draft", id.as_str()));
    let draft = window.ctx.data(|data| data.get_temp::<NodePathDraft>(key)).expect("the path section drew its draft");
    assert_eq!(draft.d, "M 0 0 L 24 0 L 12 24 Z");
    assert_eq!(draft.view_box, VIEW_BOX);
}
