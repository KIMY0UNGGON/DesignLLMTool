//! D17.6: `layer.find.next` and `layer.find.previous` in a Canvas select the next or previous layer whose name or text
//! holds the search query, open its page and bring it into view, the same through the window and an agent's run.

use designllm_contracts::{ArtifactBody, Intent, Node, Operation, Page, Size};
use serde_json::json;

use super::test_support::{canvas_with_page, Host, HostBackend, Mcp, Window};

/// Page 1: `n1` "Alpha 제목", a rectangle named "Box" (`n2`), `n3` "beta"; page 2: `m1` "alpha 끝" and `m2` "gamma". The
/// layers list each page front first: n3, n2, n1, then m2, m1.
fn canvas_with_matches(host: &mut Host) -> String {
    let canvas = canvas_with_page(host, "찾기");
    let add = |page: &str, mut value: serde_json::Value, x: i64| {
        value["bounds"] = json!({ "x": x, "y": 1_000_000, "width": 1_000_000, "height": 400_000 });
        let node = serde_json::from_value::<Node>(value).expect("a node parses");
        Operation::AddNode { page_id: page.into(), parent_id: None, node }
    };
    let operations = vec![
        Operation::InsertPage { index: 1, page: Page::new("page-2", Size::slide()) },
        add("page-1", json!({ "id": "n1", "kind": "text", "text": "Alpha 제목" }), 500_000),
        add("page-1", json!({ "id": "n2", "kind": "rectangle", "text": "", "name": "Box" }), 2_000_000),
        add("page-1", json!({ "id": "n3", "kind": "text", "text": "beta" }), 3_500_000),
        add("page-2", json!({ "id": "m1", "kind": "text", "text": "alpha 끝" }), 7_000_000),
        add("page-2", json!({ "id": "m2", "kind": "text", "text": "gamma" }), 500_000),
    ];
    host.apply(&canvas, operations, [Intent::Content, Intent::Design].into_iter().collect()).expect("the nodes are added");
    canvas
}

fn found(window: &Window) -> (Vec<String>, u32) {
    (window.app.selected_nodes.clone(), window.app.view_ref().page_index)
}

/// The node's frame lies wholly on the board.
fn on_board(window: &Window, id: &str) -> bool {
    let board = window.app.board_rect.expect("the board has been drawn");
    let view = window.app.view_ref();
    window.app.node_frames(board.min + view.pan, view.zoom).into_iter().any(|(node, frame)| node == id && board.contains_rect(frame))
}

/// With nothing selected, true.
fn selected_on_board(window: &Window) -> bool {
    window.app.selected_nodes.first().is_none_or(|id| on_board(window, id))
}

fn host_selection(window: &Window) -> Vec<String> {
    window.app.context.as_ref().expect("the host's context").selection.node_ids.clone()
}

#[test]
fn canvas_find_selects_each_name_or_text_match_across_pages_and_wraps() {
    let mut host = Host::start();
    let canvas = canvas_with_matches(&mut host);
    let mut window = Window::open(&mut host, &canvas);
    window.draw(&mut host, 1.0);
    let revision = host.artifact(&canvas).revision;
    window.app.layer_find_query = "  ALPHA ".into();
    window.step(&mut host, "view.zoom_page", json!({ "page_index": 0 })).expect("page 1 fills the board");
    window.draw(&mut host, 1.0);
    assert!(!on_board(&window, "m1"), "the match on page 2 starts off the board");

    let steps = [
        ("layer.find.next", "n1", 0),
        ("layer.find.next", "m1", 1),
        ("layer.find.next", "n1", 0),
        ("layer.find.previous", "m1", 1),
        ("layer.find.previous", "n1", 0),
    ];
    for (command, node, page) in steps {
        window.step(&mut host, command, json!({})).unwrap_or_else(|error| panic!("{command}: {error}"));
        window.draw(&mut host, 1.0);
        assert_eq!(found(&window), (vec![node.to_owned()], page), "{command}");
        assert_eq!(host_selection(&window), [node], "the host holds the selection");
        assert!(selected_on_board(&window), "{node} is brought into view");
    }

    window.app.layer_find_query = "box".into();
    window.step(&mut host, "layer.find.next", json!({})).expect("a name match");
    assert_eq!(found(&window), (vec!["n2".to_owned()], 0), "the name of a node without text matches");
    window.step(&mut host, "layer.find.previous", json!({})).expect("the only match");
    assert_eq!(found(&window), (vec!["n2".to_owned()], 0), "a single match stays selected");

    window.step(&mut host, "nav.page", json!({ "page_index": 1 })).expect("page 2");
    window.step(&mut host, "select.node", json!({ "node_id": "m2" })).expect("a node that does not match");
    window.app.layer_find_query = "alpha".into();
    window.step(&mut host, "layer.find.next", json!({})).expect("next from a selection");
    assert_eq!(found(&window), (vec!["m1".to_owned()], 1), "the first match after m2 in layer order, not the first match");

    for query in ["", "   ", "없는 검색어"] {
        window.app.layer_find_query = query.into();
        let calls = window.calls.len();
        for command in ["layer.find.next", "layer.find.previous"] {
            window.step(&mut host, command, json!({})).expect("no match is not an error");
            assert_eq!(found(&window), (vec!["m1".to_owned()], 1), "{query:?}: the selection and the page stay");
        }
        assert_eq!(window.calls.len(), calls, "{query:?}: nothing is sent");
    }
    assert_eq!(host.artifact(&canvas).revision, revision, "finding does not edit the canvas");
}

#[test]
fn canvas_find_hidden_match_releases_the_view_after_one_reveal() {
    let mut host = Host::start();
    let canvas = canvas_with_matches(&mut host);
    let hide = Operation::SetNodeInfo { node_ids: vec!["m1".into()], alt: None, name: None, hidden: Some(true) };
    host.apply(&canvas, vec![hide], [Intent::Content].into_iter().collect()).expect("m1 is hidden");
    let mut window = Window::open_with_previews(&mut host, &canvas);
    window.draw(&mut host, 1.0);
    window.step(&mut host, "view.zoom_page", json!({ "page_index": 0 })).expect("page 1 fills the board");
    window.draw(&mut host, 1.0);
    assert!(!on_board(&window, "m1"));

    window.app.layer_find_query = "끝".into();
    window.step(&mut host, "layer.find.next", json!({})).expect("the hidden layer is found");
    window.draw(&mut host, 1.0);
    assert_eq!(found(&window), (vec!["m1".to_owned()], 1));
    assert!(!window.app.page_textures.get(&1).expect("page 2 was rendered").frames.contains_key("m1"));
    assert!(on_board(&window, "m1"), "the stored bounds were brought into view");

    let manual_pan = window.app.view_ref().pan + eframe::egui::vec2(4096.0, 0.0);
    window.app.view().pan = manual_pan;
    assert!(!on_board(&window, "m1"));
    window.draw(&mut host, 1.0);
    assert_eq!(window.app.view_ref().pan, manual_pan);
    assert!(window.app.canvas_find_reveal.is_none());
}

#[test]
fn canvas_find_through_an_agent_run_matches_the_window() {
    let mut runs = Vec::new();
    for agent in [false, true] {
        let mut host = Host::start();
        let canvas = canvas_with_matches(&mut host);
        let mut window = Window::open(&mut host, &canvas);
        window.draw(&mut host, 1.0);
        // Page 1 fills the board, so that the match on page 2 has to be brought into view.
        window.step(&mut host, "view.zoom_page", json!({ "page_index": 0 })).expect("page 1 fills the board");
        // The zoom settles on a whole render dpi after a delay: settled now, so that the two paths' timing does not count.
        window.app.view().zoom_changed = Some(f64::NEG_INFINITY);
        window.draw(&mut host, 1.0);
        let mut seen = Vec::new();
        let find = ("layer.find", json!({ "text": "alpha" }));
        let next = ("layer.find.next", json!({}));
        let previous = ("layer.find.previous", json!({}));
        for (command, args) in [find, next.clone(), next, previous.clone(), previous] {
            if agent {
                let run = window.agent_run(&mut host, command, args);
                assert_eq!(run["status"], "completed", "{command}: {run}");
            } else {
                window.step(&mut host, command, args).expect(command);
            }
            window.draw(&mut host, 1.0);
            let view = window.app.view_ref();
            seen.push((found(&window), host_selection(&window), selected_on_board(&window), (view.pan.x, view.pan.y, view.zoom)));
        }
        let artifact = host.artifact(&canvas);
        let ArtifactBody::Canvas(document) = artifact.body else { panic!("a canvas") };
        runs.push((seen, artifact.revision, serde_json::to_value(document).expect("the body serializes")));
    }
    let pages: Vec<_> = runs[0].0.iter().map(|step| step.0.clone()).collect();
    let at = |node: &str, page: u32| (vec![node.to_owned()], page);
    assert_eq!(pages, [(Vec::new(), 0), at("n1", 0), at("m1", 1), at("n1", 0), at("m1", 1)]);
    assert!(runs[0].0.iter().all(|step| step.2), "every match is on the board");
    assert_eq!(runs[0], runs[1], "the window and the agent's run end in the same selection, page, pan, zoom and canvas");
}

#[test]
fn canvas_find_next_through_mcp_matches_the_window() {
    let mut host = Host::start();
    let canvas = canvas_with_matches(&mut host);
    let mut gui = Window::open_with_previews(&mut host, &canvas);
    let mut agent = Window::open_with_previews(&mut host, &canvas);
    for window in [&mut gui, &mut agent] {
        window.draw(&mut host, 1.0);
        window.step(&mut host, "view.zoom_page", json!({ "page_index": 0 })).expect("page 1 fills the board");
        window.app.view().zoom_changed = Some(f64::NEG_INFINITY);
        window.step(&mut host, "layer.find", json!({ "text": "끝" })).expect("the query is set");
        window.draw(&mut host, 1.0);
        assert!(!on_board(window, "m1"));
    }
    gui.step(&mut host, "layer.find.next", json!({})).expect("the window finds m1");
    gui.draw(&mut host, 1.0);

    agent.app.publish_ui_if_due();
    agent.settle(&mut host);
    let backend = HostBackend::new(host);
    let mcp = Mcp::connect(backend.clone());
    let queued = mcp.call("ui_command_run", json!({
        "view_id": agent.app.ui.view_id, "command_id": "layer.find.next", "args": {}, "actor": "agent", "permission": "read"
    }));
    agent.app.last_event_poll = std::time::Instant::now() - std::time::Duration::from_secs(3);
    agent.app.poll_events();
    backend.with(|host| {
        agent.settle(host);
        agent.draw(host, 1.0);
    });
    let run = mcp.call("ui_command_get", json!({ "run_id": queued["run_id"] }));
    assert_eq!(run["status"], "completed", "{run}");
    assert_eq!(found(&gui), (vec!["m1".to_owned()], 1));
    assert!(on_board(&gui, "m1"));
    assert_eq!(found(&agent), found(&gui));
    assert_eq!(host_selection(&agent), host_selection(&gui));
    let (gui_view, agent_view) = (gui.app.view_ref(), agent.app.view_ref());
    assert_eq!((agent_view.pan, agent_view.zoom), (gui_view.pan, gui_view.zoom));
}

/// An agent's run reports what the host answered to the selection it sent, not success before the answer.
#[test]
fn an_agent_find_run_fails_when_the_host_refuses_its_selection() {
    let mut host = Host::start();
    let canvas = canvas_with_matches(&mut host);
    let mut window = Window::open(&mut host, &canvas);
    window.draw(&mut host, 1.0);
    window.step(&mut host, "layer.find", json!({ "text": "alpha" })).expect("the query is set");
    window.app.publish_ui_if_due();
    window.settle(&mut host);
    let view_id = window.app.ui.view_id.clone();
    let listing = host.call("ui.command.list", json!({ "view_id": view_id })).expect("the window's registry");
    let queued = host
        .call(
            "ui.command.run",
            json!({
                "view_id": view_id,
                "expected_state_seq": listing["state_seq"],
                "command_id": "layer.find.next",
                "actor": "agent",
                "permission": "read",
                "allowed_intents": [],
                "args": {},
            }),
        )
        .expect("the run is queued");
    // The canvas changes behind the window: the selection it sends names a revision the host no longer has.
    let node = serde_json::from_value::<Node>(json!({
        "id": "z", "kind": "text", "text": "zeta", "bounds": { "x": 0, "y": 0, "width": 100_000, "height": 100_000 }
    }))
    .expect("a node parses");
    host.apply(&canvas, vec![Operation::AddNode { page_id: "page-2".into(), parent_id: None, node }], [Intent::Content].into_iter().collect())
        .expect("the canvas changes");
    window.app.last_event_poll = std::time::Instant::now() - std::time::Duration::from_secs(3);
    window.app.poll_events();
    window.settle(&mut host);

    let run = host.call("ui.command.get", json!({ "run_id": queued["run_id"] })).expect("the run's record");
    assert_ne!(run["status"], "completed", "{run}");
    assert!(run["error"]["message"].as_str().is_some_and(|message| message.contains("selection was made on revision")), "the host's refusal: {run}");
    assert!(!host_selection(&window).contains(&"n1".to_owned()), "the host did not take the selection");
}

/// While a text is edited on the board, the search does not move the page, the view or the selection.
#[test]
fn canvas_find_during_an_inline_text_edit_changes_nothing() {
    let mut host = Host::start();
    let canvas = canvas_with_matches(&mut host);
    let mut window = Window::open(&mut host, &canvas);
    window.draw(&mut host, 1.0);
    window.step(&mut host, "view.zoom_page", json!({ "page_index": 0 })).expect("page 1 fills the board");
    window.step(&mut host, "select.node", json!({ "node_id": "n3" })).expect("the edited text is selected");
    window.draw(&mut host, 1.0);
    assert!(window.app.begin_inline_text_edit(&window.ctx, "n3", 0, None), "n3 opens for editing");
    window.app.layer_find_query = "alpha".into();
    let before = (found(&window), window.app.view_ref().pan, window.app.page_index);

    let result = window.step(&mut host, "layer.find.next", json!({}));

    assert!(result.is_err(), "the search waits for the edit to finish");
    assert_eq!((found(&window), window.app.view_ref().pan, window.app.page_index), before);
    assert!(window.app.inline_text_edit.is_some(), "the edit goes on");
}

/// A match found before the board knows its rectangle is brought into view by the next frame; a reopened document whose
/// selection page is 0 while its view shows page 2 opens page 2 for a match there.
#[test]
fn canvas_find_brings_the_match_into_view_on_the_next_frame_and_keeps_both_page_indices() {
    let mut host = Host::start();
    let canvas = canvas_with_matches(&mut host);
    let mut window = Window::open(&mut host, &canvas);
    window.draw(&mut host, 1.0);
    window.step(&mut host, "view.zoom_page", json!({ "page_index": 0 })).expect("page 1 fills the board");
    window.draw(&mut host, 1.0);
    window.app.board_rect = None;
    window.app.layer_find_query = "alpha".into();
    window.step(&mut host, "layer.find.next", json!({})).expect("n1");
    window.step(&mut host, "layer.find.next", json!({})).expect("m1");
    assert_eq!(found(&window), (vec!["m1".to_owned()], 1));
    window.draw(&mut host, 1.0);
    assert!(on_board(&window, "m1"), "the next frame brings m1 into view");

    window.step(&mut host, "select.clear", json!({})).expect("nothing selected");
    window.app.page_index = 0;
    window.step(&mut host, "layer.find.previous", json!({})).expect("m1 again");
    assert_eq!((window.app.view_ref().page_index, window.app.page_index), (1, 1), "both page indices follow the match");
}
