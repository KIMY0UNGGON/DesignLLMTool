//! Task 64: defects the on-screen GUI check (60) found, each reproduced on the window's command path with the real host.

use designllm_contracts::{new_id, ArtifactBody, Intent, Node, NodeKind, Operation, PanelBody};
use serde_json::json;

use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc::{Receiver, Sender},
    Arc,
};
use std::time::{Duration, Instant};

use designllm_contracts::{ArtifactSummary, DltError, ErrorCode};
use eframe::egui::{Context, Event, Key, Modifiers, RawInput, ViewportCommand, ViewportEvent, ViewportId};

use super::board::LeftTab;
use super::figure_tools::{new_diagram_panel, node_addition, panel_additions};
use super::host_link::{HOST_LOST, HOST_LOST_CLOSE_NOTE, HOST_LOST_REFUSAL, QUIT, RECONNECT};
use super::link_flow_tests::canvas_with_three_boxes;
use super::test_support::{canvas_with_page, drawn_text, screen, settle_work, Host, Window};
use super::{DesktopApp, Screen};
use crate::rpc::{RpcPayload, RpcReply, RpcWorker, Work};
use crate::theme;

fn scratch(test: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("designllm-desktop-64-{test}-{}", new_id()));
    std::fs::create_dir_all(&dir).expect("a scratch folder");
    dir
}

/// The canvas nodes of page 0, as the host keeps them.
fn canvas_nodes(host: &mut Host, artifact_id: &str) -> Vec<Node> {
    match host.artifact(artifact_id).body {
        ArtifactBody::Canvas(document) => document.pages[0].nodes.clone(),
        _ => panic!("a canvas"),
    }
}

/// A canvas whose only page holds one rectangle `r` with the default fill and stroke.
fn canvas_with_rectangle(host: &mut Host) -> String {
    let canvas = canvas_with_page(host, "사각형");
    let node: Node = serde_json::from_value(json!({
        "id": "r", "kind": "rectangle",
        "bounds": { "x": 952_500, "y": 952_500, "width": 1_905_000, "height": 952_500 },
    }))
    .expect("a rectangle parses");
    host.apply(&canvas, vec![Operation::AddNode { page_id: "page-1".into(), parent_id: None, node }], [Intent::Content].into_iter().collect())
        .expect("the rectangle goes in");
    canvas
}

/// 64-1 (M50): a slide deck offers SVG, which its engine writes for one page; `file.export` used to refuse it as a format
/// the artifact does not support.
#[test]
fn a_slide_deck_exports_one_page_as_svg_through_file_export() {
    let mut host = Host::start();
    let deck = host.create("slides", "덱");
    let mut window = Window::open(&mut host, &deck);
    let target = scratch("slides-svg").join("page.svg");
    window
        .step(&mut host, "file.export", json!({ "format": "svg", "target_path": target.to_string_lossy(), "scope": "page" }))
        .expect("a slide deck exports SVG");
    assert_eq!(window.message().as_deref(), Some("내보냈습니다"), "the export is not refused");
    let text = std::fs::read_to_string(&target).expect("the export wrote the SVG");
    assert!(text.contains("<svg"), "an SVG document");
}

/// 64-2 (M65): in the editor, `nav.focus_request` opens the AI tab and puts the keyboard in its request box. It used to go to
/// the home screen. The selection the user made stays the request's target.
#[test]
fn focus_request_in_the_editor_focuses_the_ai_request_box_and_keeps_the_target() {
    let mut host = Host::start();
    let canvas = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
    let mut window = Window::open(&mut host, &canvas);
    window.step(&mut host, "select.node", json!({ "node_id": "a" })).expect("the box is selected");
    window.step(&mut host, "nav.focus_request", json!({})).expect("the command runs");
    assert_eq!(window.app.screen, Screen::Editor, "the editor stays open");
    assert_eq!(window.app.view_ref().left_tab, LeftTab::Ai, "the AI tab is shown");
    assert!(window.app.view_ref().left_open, "the left sidebar is open");
    assert_eq!(window.app.ai_scope_label(), "선택한 항목 1개", "the selected box is the target");
    window.draw(&mut host, 1.0);
    assert!(window.ctx.memory(|memory| memory.focused()).is_some(), "the request box has the keyboard");
}

/// 64-4 (M07): `props.fill_remove` and `props.stroke_remove` on a canvas shape remove the fill and the stroke. They used to send
/// the colour `none`, which the canvas engine refuses.
#[test]
fn removing_the_fill_and_the_stroke_of_a_canvas_shape_succeeds() {
    let mut host = Host::start();
    let canvas = canvas_with_rectangle(&mut host);
    let mut window = Window::open(&mut host, &canvas);
    window.step(&mut host, "select.node", json!({ "node_id": "r" })).expect("the rectangle is selected");
    window.step(&mut host, "props.fill_remove", json!({})).expect("the fill is removed");
    assert_eq!(window.message(), None, "the engine accepts the removal");
    window.step(&mut host, "props.stroke_remove", json!({})).expect("the stroke is removed");
    assert_eq!(window.message(), None, "the engine accepts the removal");
    let nodes = canvas_nodes(&mut host, &canvas);
    let rectangle = nodes.iter().find(|node| node.id == "r").expect("the rectangle");
    assert_eq!(rectangle.style.fill, "transparent");
    assert_eq!(rectangle.style.stroke_width, 0);
}

/// 64-5 (M10): choosing `그리드` for a frame with no column count makes a grid. The command used to send only the mode, which the
/// engine refuses without `grid_columns`, and the column field shows only once the frame is a grid.
#[test]
fn choosing_grid_for_a_frame_without_columns_makes_a_grid() {
    let mut host = Host::start();
    let canvas = canvas_with_rectangle(&mut host);
    let mut window = Window::open(&mut host, &canvas);
    window.step(&mut host, "select.node", json!({ "node_id": "r" })).expect("the frame is selected");
    window.step(&mut host, "props.layout_mode", json!({ "mode": "grid" })).expect("the mode is sent");
    assert_eq!(window.message(), None, "the engine accepts the grid");
    let nodes = canvas_nodes(&mut host, &canvas);
    let layout = nodes.iter().find(|node| node.id == "r").and_then(|node| node.layout.clone()).expect("the frame has a layout");
    assert_eq!(layout.mode, "grid");
    assert_eq!(layout.grid_columns, Some(2), "the column count the properties panel shows by default");
}

/// 64-6 (M15): `edit.nudge` with several nodes selected moves every one of them. It used to move the first and report success.
#[test]
fn nudging_a_multi_selection_moves_every_selected_node() {
    let mut host = Host::start();
    let canvas = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
    let mut window = Window::open(&mut host, &canvas);
    window.step(&mut host, "select.many", json!({ "node_ids": ["a", "b"] })).expect("two boxes are selected");
    window.step(&mut host, "edit.nudge", json!({ "x": 10, "y": 5 })).expect("the selection is moved");
    assert_eq!(window.message(), None);
    let nodes = canvas_nodes(&mut host, &canvas);
    let at = |id: &str| nodes.iter().find(|node| node.id == id).map(|node| (node.bounds.x, node.bounds.y)).expect("the box");
    let (dx, dy) = (10 * designllm_contracts::EMU_PER_CSS_PX, 5 * designllm_contracts::EMU_PER_CSS_PX);
    assert_eq!(at("a"), (dx, 1_000_000 + dy), "the first box moved");
    assert_eq!(at("b"), (1_000_000 + dx, 1_000_000 + dy), "the second box moved too");
    assert_eq!(at("c"), (3_000_000, 1_000_000), "the box not selected stays");
}

/// A window whose host can stop: while `stopped`, every request is answered as the RPC worker answers when the host pipe is gone
/// (HOST_NOT_RUNNING); `lose_next` answers only that many requests so (an answer lost while the host still runs). The host
/// itself keeps its committed state, as a stopped host keeps it on disk.
struct LinkWindow {
    app: DesktopApp,
    replies: Sender<RpcReply>,
    work: Receiver<Work>,
    ctx: Context,
    stopped: Arc<AtomicBool>,
    lose_next: usize,
    sent: Vec<String>,
    /// The error a stopped host's requests get: HOST_NOT_RUNNING (the pipe is gone), or IO (the pipe is held but answers nothing).
    failure: ErrorCode,
    /// While stopped, `system.hello` is still answered, as the bridge answers it from the handshake without the pipe.
    cached_hello: bool,
}

impl LinkWindow {
    fn open(host: &mut Host, artifact_id: &str) -> Self {
        let (rpc, replies, work) = RpcWorker::detached();
        let ctx = Context::default();
        theme::install(&ctx);
        let app = DesktopApp::new(rpc, "작업공간".into(), None, None);
        let stopped = Arc::new(AtomicBool::new(false));
        let mut window = Self { app, replies, work, ctx, stopped, lose_next: 0, sent: Vec::new(), failure: ErrorCode::HostNotRunning, cached_hello: false };
        window.settle(host);
        let list = host.call("workspace.list", json!({})).expect("the work list");
        let summary = list["artifacts"].as_array().and_then(|all| all.iter().find(|item| item["id"] == artifact_id)).cloned().expect("listed");
        window.app.activate(serde_json::from_value::<ArtifactSummary>(summary).expect("a summary"));
        window.settle(host);
        let stopped = window.stopped.clone();
        window.app.host_link.starter = Some(Arc::new(move || {
            stopped.store(false, Ordering::SeqCst);
            Ok(())
        }));
        window
    }

    fn settle(&mut self, host: &mut Host) {
        let stopped = self.stopped.load(Ordering::SeqCst);
        let (failure, cached_hello) = (self.failure.clone(), self.cached_hello);
        let (replies, lose_next, sent) = (&self.replies, &mut self.lose_next, &mut self.sent);
        settle_work(&self.work, &mut self.app, &self.ctx, |item| {
            if let Work::Call { ticket, method, params, .. } = item {
                sent.push(method.clone());
                let lost = (stopped && !(cached_hello && method == "system.hello")) || *lose_next > 0;
                *lose_next = lose_next.saturating_sub(1);
                let result = if lost {
                    // The bridge marks its transport failures retryable.
                    let mut error = DltError::new(failure.clone(), "no DesignLLMTool host is serving this workspace");
                    error.retryable = true;
                    Err(error)
                } else if method == "host.heartbeat" || (stopped && method == "system.hello") {
                    // The pipe server answers these itself (crates/host conn.rs), not the core.
                    Ok(RpcPayload::Json(json!({})))
                } else {
                    host.call(&method, params).map(RpcPayload::Json)
                };
                replies.send(RpcReply { ticket, result }).expect("the window listens");
            }
        });
    }

    /// The host stops; the next poll of the window finds no host.
    fn stop_host(&mut self, host: &mut Host) {
        self.stopped.store(true, Ordering::SeqCst);
        self.poll(host);
        assert!(self.app.host_link.lost, "the window knows its host is gone");
    }

    fn poll(&mut self, host: &mut Host) {
        self.app.last_event_poll = Instant::now() - Duration::from_secs(3);
        self.app.poll_events();
        self.settle(host);
    }

    fn reconnect(&mut self, host: &mut Host) {
        self.app.reconnect_host();
        let deadline = Instant::now() + Duration::from_secs(5);
        while self.app.host_link.lost && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
            self.app.poll_reconnect();
        }
        assert!(!self.app.host_link.lost, "the host is back");
        self.settle(host);
    }

    fn drawn(&mut self) -> Vec<String> {
        let app = &mut self.app;
        // A new modal measures itself in its first frame and draws in the next.
        let _ = self.ctx.run(screen(1.0, 1280.0, 800.0), |ctx| app.draw_host_link(ctx));
        let output = self.ctx.run(screen(1.0, 1280.0, 800.0), |ctx| app.draw_host_link(ctx));
        drawn_text(&output)
    }
}

/// 64-7: when the host stops, the window says so, refuses changes, and offers `다시 연결` and `종료`. Reconnecting to the restarted
/// host (a new host on the same workspace) shows the same document at the revision the host kept, keeps the user's selection
/// and the AI target, and registers the window's commands with the new host again.
#[test]
fn a_lost_host_is_shown_and_reconnecting_to_a_restarted_host_restores_the_window() {
    let mut host = Host::start();
    let canvas = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
    let mut window = LinkWindow::open(&mut host, &canvas);
    window.app.invoke("select.node", json!({ "node_id": "a" })).expect("the box is selected");
    window.settle(&mut host);
    window.app.publish_ui_if_due();
    window.settle(&mut host);
    let revision = window.app.artifact.as_ref().map(|artifact| artifact.revision).expect("the document is loaded");
    assert!(!window.drawn().iter().any(|text| text == HOST_LOST), "a live host shows nothing");

    window.stop_host(&mut host);
    let shown = window.drawn();
    for words in [HOST_LOST, RECONNECT, QUIT] {
        assert!(shown.iter().any(|text| text == words), "{words} is shown: {shown:?}");
    }
    assert_eq!(window.app.invoke("edit.nudge", json!({ "x": 1, "y": 0 })), Err(HOST_LOST_REFUSAL.into()), "a change is refused with the reason");
    assert_eq!(window.app.invoke("nav.toggle_left", json!({})), Ok(()), "the window itself still answers");
    assert_eq!(window.app.invoke("nav.toggle_left", json!({})), Ok(()));

    host.restart();
    window.reconnect(&mut host);
    assert_eq!(window.app.screen, Screen::Editor);
    assert_eq!(window.app.active.as_ref().map(|active| active.id.as_str()), Some(canvas.as_str()), "the same document");
    assert_eq!(window.app.artifact.as_ref().map(|artifact| artifact.revision), Some(revision), "at the revision the host kept");
    assert_eq!(window.app.selected_nodes, ["a"], "the selection is kept");
    assert_eq!(window.app.ai_target_ids(), ["a"], "the AI target is kept");
    assert!(!window.drawn().iter().any(|text| text == HOST_LOST), "the notice is gone");

    // The new host knows the window's commands and its view: the polls and the changes work again.
    window.app.publish_ui_if_due();
    window.settle(&mut host);
    let listing = host.call("ui.command.list", json!({ "view_id": window.app.ui.view_id })).expect("the window's registry");
    assert!(listing["commands"].as_array().is_some_and(|commands| !commands.is_empty()), "the registry is sent again");
    window.poll(&mut host);
    assert_eq!(window.app.notices.current(Instant::now()).map(|notice| notice.text.clone()), None, "the polls are accepted");
    window.app.invoke("edit.nudge", json!({ "x": 1, "y": 0 })).expect("changes run again");
    window.settle(&mut host);
    let nodes = canvas_nodes(&mut host, &canvas);
    assert_eq!(nodes.iter().find(|node| node.id == "a").map(|node| node.bounds.x), Some(designllm_contracts::EMU_PER_CSS_PX), "the kept selection moves");
}

/// 64-7: a host lost on the home screen comes back without opening the editor.
#[test]
fn reconnecting_from_the_home_screen_stays_on_the_home_screen() {
    let mut host = Host::start();
    let canvas = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
    let mut window = LinkWindow::open(&mut host, &canvas);
    window.app.invoke("nav.home", json!({})).expect("home");
    window.stop_host(&mut host);
    host.restart();
    window.reconnect(&mut host);
    assert_eq!(window.app.screen, Screen::Home);
}

/// 64-7: one answer lost while the host runs is not a lost host (the check asks again and finds it).
#[test]
fn one_lost_answer_is_not_a_lost_host() {
    let mut host = Host::start();
    let canvas = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
    let mut window = LinkWindow::open(&mut host, &canvas);
    window.lose_next = 1;
    window.poll(&mut host);
    assert!(!window.app.host_link.lost, "the check found the host");
    assert!(window.sent.iter().any(|method| method == "host.heartbeat"), "the window checked over the pipe: {:?}", window.sent);
}

/// 64-7: while the host is gone the window sends nothing: only commands that call no host run, shortcuts do nothing, and the
/// polls stop.
#[test]
fn a_lost_host_blocks_host_commands_shortcuts_and_polls() {
    let mut host = Host::start();
    let canvas = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
    let second = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
    let mut window = LinkWindow::open(&mut host, &canvas);
    window.stop_host(&mut host);
    window.sent.clear();
    for (id, args) in [("nav.open_project", json!({ "artifact_id": second })), ("select.node", json!({ "node_id": "a" })), ("layer.find.next", json!({}))] {
        assert_eq!(window.app.invoke(id, args), Err(HOST_LOST_REFUSAL.into()), "{id} needs the host");
    }
    assert_eq!(window.app.active.as_ref().map(|active| active.id.as_str()), Some(canvas.as_str()), "the open document stays");
    let tab = Event::Key { key: Key::Tab, physical_key: Some(Key::Tab), pressed: true, repeat: false, modifiers: Modifiers::NONE };
    let mut input = screen(1.0, 1280.0, 800.0);
    input.events.push(tab);
    let app = &mut window.app;
    let _ = window.ctx.run(input, |ctx| app.handle_shortcuts(ctx));
    window.poll(&mut host);
    window.settle(&mut host);
    assert!(window.sent.is_empty(), "nothing is sent while the host is gone: {:?}", window.sent);
}

/// 64-7: `종료` closes a window with held work while the host is gone; it says what is lost and what stays as a draft.
#[test]
fn quitting_after_a_lost_host_closes_past_held_work() {
    let mut host = Host::start();
    let canvas = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
    let mut window = LinkWindow::open(&mut host, &canvas);
    window.app.close_release_failed = true;
    assert!(window.app.has_close_work());
    window.stop_host(&mut host);
    assert!(window.drawn().iter().any(|text| text == HOST_LOST_CLOSE_NOTE), "the window says what closing keeps");
    window.app.quit_after_host_loss(&window.ctx);
    let mut input = RawInput::default();
    input.viewports.get_mut(&ViewportId::ROOT).expect("root viewport").events.push(ViewportEvent::Close);
    let app = &mut window.app;
    let output = window.ctx.run(input, |ctx| app.guard_held_edit_close(ctx));
    let cancelled = output.viewport_output.get(&ViewportId::ROOT).is_some_and(|viewport| viewport.commands.contains(&ViewportCommand::CancelClose));
    assert!(!cancelled, "the close goes through");
    assert!(!window.app.close_confirmation_open, "no close question is left behind the notice");
}

/// 64-4 (review): a canvas fill or stroke of `transparent` paints nothing, so the properties panel offers to add one and shows no
/// colour `transparent`.
#[test]
fn a_transparent_fill_and_stroke_show_as_none_in_the_properties_panel() {
    let mut host = Host::start();
    let canvas = canvas_with_rectangle(&mut host);
    let patch = designllm_contracts::StylePatch { fill: Some("transparent".into()), stroke: Some("transparent".into()), stroke_width: Some(9525), ..Default::default() };
    host.apply(&canvas, vec![Operation::SetStyle { node_ids: vec!["r".into()], patch }], [Intent::Design].into_iter().collect()).expect("the style");
    let mut window = Window::open(&mut host, &canvas);
    window.step(&mut host, "select.node", json!({ "node_id": "r" })).expect("the rectangle is selected");
    let app = &mut window.app;
    let output = window.ctx.run(screen(1.0, 1280.0, 800.0), |ctx| {
        eframe::egui::CentralPanel::default().show(ctx, |ui| app.draw_properties(ui, &theme::palette(ctx)));
    });
    let shown = drawn_text(&output);
    assert!(!shown.iter().any(|text| text.contains("transparent")), "no colour transparent is shown: {shown:?}");
    assert!(!super::properties::paints("transparent") && !super::properties::paints("none") && super::properties::paints("#112233"));
}

/// 64-6 (review): nodes under different parents move together; each keeps its place relative to its own parent.
#[test]
fn nudging_nodes_under_different_parents_moves_each_of_them() {
    let mut host = Host::start();
    let canvas = canvas_with_page(&mut host, "묶음");
    let group: Node = serde_json::from_value(json!({
        "id": "g", "kind": "group", "bounds": { "x": 0, "y": 0, "width": 2_000_000, "height": 2_000_000 },
        "children": [{ "id": "child", "kind": "rectangle", "bounds": { "x": 100_000, "y": 100_000, "width": 500_000, "height": 500_000 } }],
    }))
    .expect("a group parses");
    let top: Node = serde_json::from_value(json!({
        "id": "top", "kind": "rectangle", "bounds": { "x": 3_000_000, "y": 100_000, "width": 500_000, "height": 500_000 },
    }))
    .expect("a rectangle parses");
    let add = |node| Operation::AddNode { page_id: "page-1".into(), parent_id: None, node };
    host.apply(&canvas, vec![add(group), add(top)], [Intent::Content].into_iter().collect()).expect("the nodes");
    let mut window = Window::open(&mut host, &canvas);
    window.step(&mut host, "select.many", json!({ "node_ids": ["child", "top"] })).expect("both are selected");
    window.step(&mut host, "edit.nudge", json!({ "x": 10, "y": 0 })).expect("the selection is moved");
    assert_eq!(window.message(), None);
    let dx = 10 * designllm_contracts::EMU_PER_CSS_PX;
    let nodes = canvas_nodes(&mut host, &canvas);
    let group = nodes.iter().find(|node| node.id == "g").expect("the group");
    assert_eq!(group.children[0].bounds.x, 100_000 + dx, "the child moved inside its group");
    assert_eq!(nodes.iter().find(|node| node.id == "top").map(|node| node.bounds.x), Some(3_000_000 + dx), "the page node moved");
}

/// 64-5 (review): a stack with a distribution and a hugging width becomes a grid; the grid keeps neither, as the engine requires.
#[test]
fn a_stack_with_justify_and_hug_width_becomes_a_grid() {
    let mut host = Host::start();
    let canvas = canvas_with_page(&mut host, "스택");
    let node: Node = serde_json::from_value(json!({
        "id": "s", "kind": "rectangle", "bounds": { "x": 0, "y": 0, "width": 2_000_000, "height": 1_000_000 },
        "layout": { "mode": "row", "width_mode": "hug", "height_mode": "fixed", "gap": 0, "padding": 0, "align": "start", "justify": "center" },
    }))
    .expect("a stack parses");
    host.apply(&canvas, vec![Operation::AddNode { page_id: "page-1".into(), parent_id: None, node }], [Intent::Content].into_iter().collect())
        .expect("the stack");
    let mut window = Window::open(&mut host, &canvas);
    window.step(&mut host, "select.node", json!({ "node_id": "s" })).expect("the stack is selected");
    window.step(&mut host, "props.layout_mode", json!({ "mode": "grid" })).expect("the mode is sent");
    assert_eq!(window.message(), None, "the engine accepts the grid");
    let nodes = canvas_nodes(&mut host, &canvas);
    let layout = nodes[0].layout.clone().expect("a layout");
    assert_eq!((layout.mode.as_str(), layout.width_mode.as_str(), layout.justify), ("grid", "fixed", None));
}

/// 64-1 (review): the page shown is the page a deck of several pages exports as SVG, and the export menu does not offer `선택`
/// for a deck (its engine writes whole pages only).
#[test]
fn a_slide_deck_exports_the_shown_page_as_svg_and_offers_no_selection_scope() {
    let mut host = Host::start();
    let deck = host.create("slides", "덱");
    let mut window = Window::open(&mut host, &deck);
    window.step(&mut host, "add.slide", json!({})).expect("a second slide");
    window.step(&mut host, "nav.page", json!({ "page_index": 1 })).expect("page 2");
    let target = scratch("slides-svg-2").join("page2.svg");
    window
        .step(&mut host, "file.export", json!({ "format": "svg", "target_path": target.to_string_lossy(), "scope": "page" }))
        .expect("the shown page exports");
    let text = std::fs::read_to_string(&target).expect("the export wrote the SVG");
    assert!(text.contains("data-page-index=\"1\""), "the second page");
    window.app.export_format = "svg".into();
    window.app.selected_nodes = vec!["any".into()];
    assert!(!window.app.export_scopes().iter().any(|(key, _)| *key == "selection"), "a deck offers no selection export");
}

/// 64-7 (review 2): while the host is gone, the window draws its last pages and sends nothing, even after a page or zoom change and
/// an Enter on a selected box; no error notice comes up.
#[test]
fn drawing_while_the_host_is_gone_sends_nothing_and_raises_no_notice() {
    let mut host = Host::start();
    let canvas = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
    let mut window = LinkWindow::open(&mut host, &canvas);
    window.app.invoke("select.node", json!({ "node_id": "a" })).expect("the box is selected");
    window.settle(&mut host);
    window.stop_host(&mut host);
    // Enter is the lost-host dialog's `다시 연결` while a starter is set, and that starts the host and sends its startup requests
    // (those are not sent while the host is gone). This test is about what the window draws and sends with no restart, so the
    // starter goes: Enter is still taken by the dialog, and still starts no text edit.
    window.app.host_link.starter = None;
    window.app.notices.clear_transient();
    window.sent.clear();
    window.app.invoke("view.zoom_in", json!({})).expect("the zoom is the window's own");
    let enter = Event::Key { key: Key::Enter, physical_key: Some(Key::Enter), pressed: true, repeat: false, modifiers: Modifiers::NONE };
    for frame in 0..4 {
        let mut input = screen(1.0, 1280.0, 800.0);
        if frame == 2 {
            input.events.push(enter.clone());
        }
        let app = &mut window.app;
        let _ = window.ctx.run(input, |ctx| {
            app.draw_frame(ctx);
            app.draw_host_link(ctx);
        });
        window.settle(&mut host);
    }
    assert!(window.sent.is_empty(), "nothing is sent while the host is gone: {:?}", window.sent);
    assert!(window.app.inline_text_edit.is_none(), "Enter starts no text edit behind the notice");
    assert_eq!(window.app.notices.current(Instant::now()).map(|notice| notice.text.clone()), None, "no error notice");
}

/// 64-7 (review 2): the window's own close (X, Alt+F4) while the host is gone closes as `종료` does, past the close question.
#[test]
fn closing_the_window_while_the_host_is_gone_goes_past_the_close_question() {
    let mut host = Host::start();
    let canvas = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
    let mut window = LinkWindow::open(&mut host, &canvas);
    window.app.close_release_failed = true;
    window.stop_host(&mut host);
    let mut input = RawInput::default();
    input.viewports.get_mut(&ViewportId::ROOT).expect("root viewport").events.push(ViewportEvent::Close);
    let app = &mut window.app;
    let output = window.ctx.run(input, |ctx| app.guard_held_edit_close(ctx));
    let cancelled = output.viewport_output.get(&ViewportId::ROOT).is_some_and(|viewport| viewport.commands.contains(&ViewportCommand::CancelClose));
    assert!(!cancelled, "the close goes through");
    assert!(!window.app.close_confirmation_open);
}

/// 64-7 (review 2): a host that comes back at a newer revision than the window last read still gets the window's selection
/// (sent after the window reads the new state), and the reservations of the old host are read again.
#[test]
fn reconnecting_to_a_newer_revision_keeps_the_selection_and_rereads_reservations() {
    let mut host = Host::start();
    let canvas = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
    let mut window = LinkWindow::open(&mut host, &canvas);
    window.app.invoke("select.node", json!({ "node_id": "a" })).expect("the box is selected");
    window.settle(&mut host);
    let before = window.app.context.as_ref().map(|context| context.revision).expect("the state");
    window.stop_host(&mut host);
    // Another client's change lands before the host stops for good.
    let patch = designllm_contracts::StylePatch { fill: Some("#112233".into()), ..Default::default() };
    host.apply(&canvas, vec![Operation::SetStyle { node_ids: vec!["c".into()], patch }], [Intent::Design].into_iter().collect()).expect("a change");
    host.restart();
    window.sent.clear();
    window.reconnect(&mut host);
    assert!(window.app.context.as_ref().is_some_and(|context| context.revision > before), "the window reads the newer revision");
    assert_eq!(window.app.selected_nodes, ["a"], "the selection is kept");
    assert!(window.sent.iter().any(|method| method == "targets.list"), "the reservations are read again: {:?}", window.sent);
}

// Task 66a: items 64-3, 64-8 and 64-9 moved to task 66 with their reproductions.

/// 64-3 (M34): a table `canvas.table.add` makes is drawn with its rows. Its empty cells hug their height, which used to be 0, so
/// the table had no height and no cell was drawn.
#[test]
fn a_new_canvas_table_is_laid_out_with_visible_rows() {
    let mut host = Host::start();
    let canvas = canvas_with_page(&mut host, "표");
    let mut window = Window::open(&mut host, &canvas);
    window.step(&mut host, "canvas.table.add", json!({ "rows": 3, "columns": 3 })).expect("the table is added");
    assert_eq!(window.message(), None);
    let nodes = canvas_nodes(&mut host, &canvas);
    let table = nodes.iter().find(|node| node.ext.table.is_some()).expect("the table node");
    let revision = host.artifact(&canvas).revision;
    let snapshot = host
        .call("render.get", json!({ "artifact_id": canvas, "expected_revision": revision, "page_index": 0, "include_node_frames": true,
            "profile": { "dpi": 192, "transparent": false }, "region": table.bounds }))
        .expect("the page renders");
    let frames = &snapshot["scene"]["metadata"]["node_frames"];
    let row_height = 36 * designllm_contracts::EMU_PER_CSS_PX;
    assert_eq!(frames[&table.id]["height"].as_i64(), Some(3 * row_height), "three rows of the cells' height: {}", frames[&table.id]);
    for cell in &table.children {
        assert_eq!(frames[&cell.id]["height"].as_i64(), Some(row_height), "cell {} has its row height", cell.id);
    }
    let preview = serde_json::from_value(snapshot["preview"].clone()).expect("the preview asset");
    let bytes = super::link_flow_tests::bytes_of(&mut host, &preview);
    let png = image::load_from_memory_with_format(&bytes, image::ImageFormat::Png).unwrap().to_rgb8();
    assert_eq!(png.height(), 216, "three 36 px rows at twice CSS resolution");
    for row in 0..3 {
        for col in 0..3 {
            let x = png.width() * (2 * col + 1) / 6;
            let y = 72 * row + 36;
            let fill = if row == 0 { [243, 244, 246] } else { [255, 255, 255] };
            assert_eq!(png.get_pixel(x, y).0, fill);
            if row > 0 {
                assert_eq!(png.get_pixel(x, 72 * row).0, [203, 213, 225], "horizontal border");
            }
            if col > 0 {
                let border_x = table.bounds.width * 2 * i64::from(col) / (3 * designllm_contracts::EMU_PER_CSS_PX);
                assert_eq!(png.get_pixel(border_x as u32, y).0, [203, 213, 225], "vertical border");
            }
        }
    }
}

/// 64-8: the layer search does not carry its words to another document.
#[test]
fn the_layer_search_is_cleared_when_another_document_opens() {
    let mut host = Host::start();
    let first = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
    let second = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
    let mut window = Window::open(&mut host, &first);
    window.step(&mut host, "layer.find", json!({ "text": "가" })).expect("the search opens");
    window.step(&mut host, "layer.find.next", json!({})).expect("find a box in the first canvas");
    assert_eq!(window.app.selected_nodes.len(), 1);
    assert_eq!(window.app.layer_find_query, "가");
    window.step(&mut host, "nav.open_project", json!({ "artifact_id": second })).expect("the other document opens");
    assert_eq!(window.app.active.as_ref().map(|active| active.id.as_str()), Some(second.as_str()));
    assert!(window.app.layer_find_query.is_empty(), "the old words are gone: {:?}", window.app.layer_find_query);
    assert!(!window.app.layer_find_open, "the search box is closed");
    assert!(!window.app.layer_find_focus);
    assert!(window.app.canvas_find_reveal.is_none());
    window.step(&mut host, "layer.find.next", json!({})).expect("an empty search");
    assert!(window.app.selected_nodes.is_empty(), "the same words occur in the second canvas");
}

/// 64-9 (M41): `add.diagram_node` puts the new box where no box of the diagram is. Its slot used to follow the node count only,
/// so a box placed elsewhere (by an agent's change) was covered and the engine refused with LABEL_COLLISION.
#[test]
fn a_new_diagram_node_takes_a_slot_no_other_node_covers() {
    let mut host = Host::start();
    let figure_id = host.create("figure", "그림");
    let (figure, _) = host.figure(&figure_id);
    host.apply(&figure_id, panel_additions(&figure, new_diagram_panel(&figure)), [Intent::Content, Intent::Design].into_iter().collect())
        .expect("an empty diagram panel");
    let (figure, _) = host.figure(&figure_id);
    let panel = figure.panels[0].clone();
    // A labelled box in the second slot: the next box by count would go on top of it.
    let mut operation = node_addition(&figure, &panel.id, NodeKind::Rectangle).expect("room for a box");
    if let Operation::AddDiagramNode { node, .. } = &mut operation {
        node.bounds = crate::figure_layout::diagram_node_bounds(panel.bounds.size(), 1).expect("a second slot");
        node.text = "A".into();
    }
    host.apply(&figure_id, vec![operation], [Intent::Content].into_iter().collect()).expect("the labelled box goes in");

    let mut window = Window::open(&mut host, &figure_id);
    window.step(&mut host, "add.diagram_node", json!({})).expect("the box is sent");
    assert_eq!(window.message(), None, "the engine accepts the new box");
    let (figure, _) = host.figure(&figure_id);
    let PanelBody::Diagram(diagram) = &figure.panels[0].body else { panic!("a diagram panel") };
    assert_eq!(diagram.nodes.len(), 2, "the new box is in the diagram");
    let overlaps = |a: designllm_contracts::Rect, b: designllm_contracts::Rect| {
        a.x < b.x + b.width && b.x < a.x + a.width && a.y < b.y + b.height && b.y < a.y + a.height
    };
    assert!(!overlaps(diagram.nodes[0].bounds, diagram.nodes[1].bounds), "the boxes do not overlap");
    assert_eq!(Some(diagram.nodes[1].bounds), crate::figure_layout::diagram_node_bounds(panel.bounds.size(), 0),
        "the first empty slot is reused after an out-of-order insertion");
}

/// The host integration harness (`designllm-host-harness`, crates/host tests/support), built into the same target folder as
/// this test binary. It serves a real pipe and lets this test executable hold the gui role (`--gui-exe`).
fn host_harness() -> std::path::PathBuf {
    let exe = std::env::current_exe().expect("the test executable");
    let harness = exe.parent().and_then(std::path::Path::parent).expect("target/debug").join("designllm-host-harness.exe");
    assert!(harness.is_file(), "build it first: cargo build -p designllm-host --bin designllm-host-harness ({})", harness.display());
    harness
}

/// Repeats `step` until `done` holds or `limit` passes.
fn wait_until(limit: Duration, mut step: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        if step() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

/// 64-7 (GUI recheck): a window on a real host, idle, finds the host gone after `host.stop` (the pipe closes), with its own
/// two-second poll and nothing else to do.
#[test]
#[ignore = "starts designllm-host-harness from the same target folder; run with --ignored after building it"]
fn an_idle_window_finds_a_real_host_gone_after_host_stop() {
    use std::io::BufRead;
    let harness = host_harness();
    let workspace = scratch("real-host");
    let exe = std::env::current_exe().expect("the test executable");
    let mut child = std::process::Command::new(&harness)
        .arg("--host").arg("--workspace").arg(&workspace).arg("--gui-exe").arg(&exe)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("the harness host starts");
    let stdout = child.stdout.take().expect("the host's stdout");
    let ready = std::io::BufReader::new(stdout).lines().map_while(Result::ok).any(|line| line.starts_with("READY"));
    assert!(ready, "the host serves its pipe");

    let bridge = designllm_bridge::Client::new(&workspace);
    bridge.call("workspace.list", json!({})).expect("another client, as an MCP bridge is");
    let worker = RpcWorker::spawn(&workspace).expect("the window's RPC worker");
    let mut app = DesktopApp::new(worker, "작업공간".into(), None, None);
    let ctx = Context::default();
    theme::install(&ctx);
    assert!(wait_until(Duration::from_secs(10), || {
        app.process_replies(&ctx);
        app.publish_ui_if_due();
        app.pending.is_empty() && app.ui.token.is_some()
    }), "the window reads the host");
    assert!(!app.host_lost());

    designllm_bridge::Client::new(&workspace).call("host.stop", json!({})).expect("the host stops");
    let mut exited = false;
    // The window does what `update` does while idle: replies, then its poll.
    let lost = wait_until(Duration::from_secs(15), || {
        exited |= child.try_wait().ok().flatten().is_some();
        app.process_replies(&ctx);
        app.poll_events();
        if !app.host_lost() {
            app.publish_ui_if_due();
        }
        app.host_lost()
    });

    drop(bridge);
    exited = exited || wait_until(Duration::from_secs(15), || child.try_wait().ok().flatten().is_some());
    assert!(exited, "the host process ends");
    assert!(lost, "the idle window finds its host gone");
}

/// 64-7 (GUI recheck): a host whose pipe answers nothing (IO errors) while the bridge still answers `system.hello` from its
/// handshake is found lost: the check goes over the pipe (`host.heartbeat`), and a transport error counts as no host.
#[test]
fn a_host_that_answers_nothing_over_the_pipe_is_found_lost() {
    let mut host = Host::start();
    let canvas = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
    let mut window = LinkWindow::open(&mut host, &canvas);
    window.failure = ErrorCode::Io;
    window.cached_hello = true;
    window.stop_host(&mut host);
}

/// 64-1 (GUI recheck): SVG writes one page; `scope: all` on a deck of several pages is refused with the reason, as the slide
/// engine refuses several pages for one SVG, instead of writing the shown page only.
#[test]
fn svg_of_every_page_of_a_deck_is_refused_with_the_reason() {
    let mut host = Host::start();
    let deck = host.create("slides", "덱");
    let mut window = Window::open(&mut host, &deck);
    window.step(&mut host, "add.slide", json!({})).expect("a second slide");
    let target = scratch("slides-svg-all").join("all.svg");
    let refused = window.step(&mut host, "file.export", json!({ "format": "svg", "target_path": target.to_string_lossy(), "scope": "all" }));
    assert_eq!(refused, Err(super::save_export::ONE_PAGE_FORMAT.into()));
    assert!(!target.exists(), "nothing is written");
}

/// 64 (GUI recheck): `nav.open_file` opens a native file of this tool (`.designllm`): its kind is read from the file.
#[test]
fn a_native_file_opens_by_the_kind_it_names() {
    let mut host = Host::start();
    let canvas = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
    let mut window = Window::open(&mut host, &canvas);
    let path = scratch("native-open").join("canvas.designllm");
    window.step(&mut host, "file.save", json!({ "target_path": path.to_string_lossy() })).expect("the canvas is saved");
    assert!(path.is_file(), "the native file is written");
    window.step(&mut host, "nav.open_file", json!({ "path": path.to_string_lossy() })).expect("the file opens");
    assert_eq!(window.message(), None, "no refusal: {:?}", window.message());
    assert_eq!(window.app.active.as_ref().map(|active| active.kind), Some(designllm_contracts::ArtifactKind::Canvas));
    assert_ne!(window.app.active.as_ref().map(|active| active.id.as_str()), Some(canvas.as_str()), "a new artifact from the file");
}

/// 64 (GUI recheck): an agent's `add.chart` with `path` reads the table into the figure without the chart dialog; without a
/// path the window's own command opens the dialog as before.
#[test]
fn an_agent_chart_with_a_path_reads_the_table_without_the_dialog() {
    let mut host = Host::start();
    let figure = super::link_flow_tests::drawn_figure(&mut host, "그림", 1);
    let mut window = Window::open(&mut host, &figure);
    let csv = scratch("chart-csv").join("table.csv");
    std::fs::write(&csv, "group,value\nA,1\nB,2\n").expect("the table");
    let run = run_with(&mut window, &mut host, "add.chart", json!({ "path": csv.to_string_lossy() }), "change", &["data"]);
    assert_eq!(run["status"], "completed", "{run}");
    assert!(window.app.figure_form.is_none(), "no dialog is left open");
    let (document, _) = host.figure(&figure);
    assert_eq!(document.datasets.len(), 1, "the table is in the figure");
    window.step(&mut host, "add.chart", json!({})).expect("the window's own chart");
    assert!(window.app.figure_form.is_some(), "the window's own command opens the dialog");
}

/// 64 (GUI recheck): a work item created elsewhere just now (an agent's `artifact_create`) opens: the window reads its work list
/// again when it does not know the id, and the repeated command opens it.
#[test]
fn opening_a_work_item_created_elsewhere_reads_the_list_again() {
    let mut host = Host::start();
    let first = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
    let mut window = Window::open(&mut host, &first);
    let second = host.create("canvas", "방금 만든 것");
    let first_try = window.step(&mut host, "nav.open_project", json!({ "artifact_id": second }));
    assert_eq!(first_try, Err(super::dispatch::LIST_REREAD.into()), "the window says it read its list again");
    window.step(&mut host, "nav.open_project", json!({ "artifact_id": second })).expect("the repeat opens it");
    assert_eq!(window.app.active.as_ref().map(|active| active.id.as_str()), Some(second.as_str()));
}

/// An agent run of `command` with `args`, `permission` and `intents` and the window's state (`expected_state_seq`), as an agent
/// that read the window's state sends it.
fn run_with(window: &mut Window, host: &mut Host, command: &str, args: serde_json::Value, permission: &str, intents: &[&str]) -> serde_json::Value {
    window.app.publish_ui_if_due();
    window.settle(host);
    let view_id = window.app.ui.view_id.clone();
    let listing = host.call("ui.command.list", json!({ "view_id": view_id })).expect("the window's command registry");
    let queued = host
        .call(
            "ui.command.run",
            json!({
                "view_id": view_id, "expected_state_seq": listing["state_seq"], "command_id": command, "actor": "agent",
                "permission": permission, "allowed_intents": intents, "args": args,
            }),
        )
        .expect("the agent's run is queued");
    window.app.last_event_poll = Instant::now() - Duration::from_secs(3);
    window.app.poll_events();
    window.settle(host);
    host.call("ui.command.get", json!({ "run_id": queued["run_id"] })).expect("the run's record")
}
