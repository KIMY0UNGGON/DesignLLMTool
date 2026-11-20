//! The repeat-group commands of the window (2d): the change sets they send, the GUI <-> MCP parity of all five, of their
//! results and of their refusals (`parity_tests.rs` pattern), the agent channel, and the undo. The expansion and the refusal
//! rules are the engine's.

use std::sync::mpsc::{Receiver, Sender};

use designllm_contracts::{
    Artifact, ArtifactBody, ArtifactSummary, CanvasTable, CanvasTableRow, ConditionOp, DltError, Intent, LayoutSpec, Node, NodeExt, NodeKind, Operation,
    PageRole, Rect, RepeatBinding, RepeatSpec, ShowCondition, Size, Style,
};
use eframe::egui::Context;
use serde_json::{json, Value};

use super::{
    repeat_groups::{bind_args, condition_args, create_args, create_operations, ConditionDraft, ConditionForm, Draft, RepeatPanel},
    test_support::{canvas_with_page, change_set_arguments, content_of, drawn_text, screen, settle_work, Host, HostBackend, Mcp, Window},
    DesktopApp,
};
use crate::{
    commands,
    rpc::{RpcPayload, RpcReply, RpcWorker, Work},
    theme,
};

/// The only page of the canvases these tests build.
const PAGE: &str = "page-1";
/// The component page and the repeat group that the tests create.
const COMPONENT: &str = "card-page";
const GROUP: &str = "list";

fn rect(x: i64, y: i64, width: i64, height: i64) -> Rect {
    Rect { x, y, width, height }
}

fn plain(id: &str, kind: NodeKind, bounds: Rect, text: &str, children: Vec<Node>) -> Node {
    Node {
        id: id.into(),
        kind,
        bounds,
        text: text.into(),
        style: Style::default(),
        asset: None,
        children,
        layout: None,
        interactions: Vec::new(),
        variants: Default::default(),
        ext: NodeExt::default(),
    }
}

/// A canvas with a 3-row table, `table` (a header and two data rows, names and roles), and the card `card`: a group
/// with one text child `title`, which the repeat will show. The card is the component of the repeat.
fn canvas_with_table(host: &mut Host, title: &str) -> String {
    let id = canvas_with_page(host, title);
    let cell = |id: &str, text: &str| Node {
        layout: Some(LayoutSpec { width_mode: "fill".into(), height_mode: "hug".into(), ..LayoutSpec::default() }),
        ..plain(id, NodeKind::Rectangle, rect(0, 0, 120_000, 36_000), text, Vec::new())
    };
    let cells = [("h1", "이름"), ("h2", "역할"), ("c11", "김"), ("c12", "개발"), ("c21", "박"), ("c22", "디자인")];
    let table = Node {
        layout: Some(LayoutSpec { mode: "grid".into(), height_mode: "hug".into(), grid_columns: Some(2), gap: 0, row_gap: Some(0), ..LayoutSpec::default() }),
        ext: NodeExt {
            table: Some(CanvasTable {
                rows: vec![
                    CanvasTableRow { id: "row-h".into(), cell_ids: vec!["h1".into(), "h2".into()] },
                    CanvasTableRow { id: "row-1".into(), cell_ids: vec!["c11".into(), "c12".into()] },
                    CanvasTableRow { id: "row-2".into(), cell_ids: vec!["c21".into(), "c22".into()] },
                ],
                header_row_id: Some("row-h".into()),
                column_widths: None,
                cell_spans: Default::default(),
            }),
            ..NodeExt::default()
        },
        ..plain("table", NodeKind::Group, rect(500_000, 500_000, 2_000_000, 1_000_000), "", cells.iter().map(|(id, text)| cell(id, text)).collect())
    };
    let card = plain("card", NodeKind::Group, rect(0, 0, 1_000_000, 300_000), "", vec![plain("title", NodeKind::Rectangle, rect(0, 0, 900_000, 200_000), "", Vec::new())]);
    host.apply(&id, vec![Operation::AddNode { page_id: PAGE.into(), parent_id: None, node: table }, Operation::AddNode { page_id: PAGE.into(), parent_id: None, node: card }], content())
        .expect("the table and the card");
    id
}

fn content() -> std::collections::BTreeSet<Intent> {
    [Intent::Content].into_iter().collect()
}

fn page_nodes(artifact: &Artifact) -> &[Node] {
    let ArtifactBody::Canvas(document) = &artifact.body else { panic!("a canvas") };
    &document.pages[0].nodes
}

fn top(artifact: &Artifact, id: &str) -> Node {
    page_nodes(artifact).iter().find(|node| node.id == id).cloned().expect("the node is on the page")
}

/// A node on any page: the card moves to the component page, where the condition is.
fn node_anywhere(artifact: &Artifact, id: &str) -> Node {
    fn find_in(nodes: &[Node], id: &str) -> Option<Node> {
        nodes.iter().find_map(|node| if node.id == id { Some(node.clone()) } else { find_in(&node.children, id) })
    }
    let ArtifactBody::Canvas(document) = &artifact.body else { panic!("a canvas") };
    document.pages.iter().find_map(|page| find_in(&page.nodes, id)).expect("the node is on a page")
}

/// The card's node as it is on the page now (for the change sets the tests build by hand).
fn card(host: &mut Host, canvas: &str) -> Node {
    top(&host.artifact(canvas), "card")
}

fn page_size(artifact: &Artifact) -> Size {
    let ArtifactBody::Canvas(document) = &artifact.body else { panic!("a canvas") };
    document.pages[0].size
}

/// The change set "반복 만들기" with the ids the tests use, on the canvas as it is now.
fn create_change(host: &mut Host, canvas: &str) -> Vec<Operation> {
    let artifact = host.artifact(canvas);
    let node = top(&artifact, "card");
    create_operations(&node, PAGE, 1, page_size(&artifact), "table", COMPONENT, GROUP)
}

/// Creates the repeat group on a canvas that `canvas_with_table` built.
fn create_repeat(host: &mut Host, canvas: &str) {
    let operations = create_change(host, canvas);
    host.apply(canvas, operations, content()).expect("the repeat group is created");
}

fn group(artifact: &Artifact) -> Node {
    top(artifact, GROUP)
}

/// The instance root of the group's row `index` (the rows come in table order).
fn row(artifact: &Artifact, index: usize) -> Node {
    group(artifact).children[index].clone()
}

fn overrides_of(node: &Node, source: &str) -> designllm_contracts::NodeOverride {
    node.ext.instance.as_ref().expect("an instance root").overrides.get(source).cloned().unwrap_or_default()
}

/// The MCP `changes_apply` path with the same operations, on the same artifact.
fn mcp_change(backend: &HostBackend, mcp: &Mcp, artifact: &str, operations: Vec<Operation>) -> Artifact {
    let (workspace, revision) = backend.with(|host| (host.workspace_id.clone(), host.artifact(artifact).revision));
    mcp.call("changes_apply", change_set_arguments(&workspace, artifact, revision, &[Intent::Content], operations));
    backend.with(|host| host.artifact(artifact))
}

/// Both results are the same content, and the change did something.
fn assert_same_content(before: &Artifact, gui: &Artifact, mcp: &Artifact) {
    assert_ne!(content_of(before), content_of(gui), "the command changed nothing");
    assert_eq!(content_of(gui), content_of(mcp), "the command and the MCP tool left different content");
}

#[test]
fn the_create_change_moves_the_node_to_a_component_page_and_adds_an_empty_row_group() {
    let mut host = Host::start();
    let canvas = canvas_with_table(&mut host, "반복");
    let operations = create_change(&mut host, &canvas);
    let card_bounds = card(&mut host, &canvas).bounds;

    assert_eq!(operations.len(), 5);
    let Operation::InsertPage { index, page } = &operations[0] else { panic!("the component page comes first") };
    assert_eq!((*index, page.id.as_str(), page.role), (1, COMPONENT, None));
    assert!(matches!(&operations[1], Operation::SetPageRole { page_id, role: Some(PageRole::Component) } if page_id == COMPONENT));
    assert!(matches!(
        &operations[2],
        Operation::MoveNode { node_id, page_id, parent_id: None, index: 0 } if node_id == "card" && page_id == COMPONENT
    ));
    let Operation::AddNode { page_id, parent_id, node } = &operations[3] else { panic!("the empty group") };
    assert_eq!((page_id.as_str(), parent_id.as_deref(), node.id.as_str(), node.bounds), (PAGE, None, GROUP, card_bounds));
    assert!(node.children.is_empty());
    assert_eq!(node.layout.as_ref().map(|layout| layout.mode.as_str()), Some("row"));
    let Operation::SetGroupRepeat { node_id, expected: None, repeat: Some(spec) } = &operations[4] else { panic!("the repeat last") };
    assert_eq!(node_id, GROUP);
    assert_eq!(spec, &RepeatSpec { table_id: "table".into(), component_page_id: COMPONENT.into(), bindings: Vec::new() });
}

#[test]
fn the_create_command_gives_the_same_canvas_by_command_and_by_tool() {
    let mut host = Host::start();
    let gui = canvas_with_table(&mut host, "반복");
    let mcp_canvas = canvas_with_table(&mut host, "반복");
    let before = host.artifact(&gui);

    let mut window = Window::open(&mut host, &gui);
    window.step(&mut host, "select.node", json!({ "node_id": "card" })).expect("the card is selected");
    window
        .step(&mut host, "repeat.create", json!({ "node_id": "card", "table_id": "table", "component_page_id": COMPONENT, "group_id": GROUP }))
        .expect("the repeat is created");
    let gui_after = host.artifact(&gui);
    drop(window);

    // The expansion is the engine's: one instance per data row, and the card is in the component page now.
    assert_eq!(group(&gui_after).children.len(), 2, "two data rows, so two instances");
    assert!(page_nodes(&gui_after).iter().all(|node| node.id != "card"), "the card left the page");
    assert_eq!(node_anywhere(&gui_after, "card").kind, NodeKind::Group, "the card is in the component page");

    let backend = HostBackend::new(host);
    let mcp = Mcp::connect(backend.clone());
    let operations = backend.with(|host| create_change(host, &mcp_canvas));
    let mcp_after = mcp_change(&backend, &mcp, &mcp_canvas, operations);
    assert_same_content(&before, &gui_after, &mcp_after);
}

#[test]
fn the_bind_command_gives_the_same_canvas_by_command_and_by_tool() {
    let mut host = Host::start();
    let gui = canvas_with_table(&mut host, "반복");
    let mcp_canvas = canvas_with_table(&mut host, "반복");
    create_repeat(&mut host, &gui);
    create_repeat(&mut host, &mcp_canvas);
    let before = host.artifact(&gui);

    let mut window = Window::open(&mut host, &gui);
    window.step(&mut host, "select.node", json!({ "node_id": GROUP })).expect("the group is selected");
    window
        .step(&mut host, "repeat.bind", json!({ "group_id": GROUP, "bindings": [{ "source_node_id": "title", "column": 0 }] }))
        .expect("the binding is sent");
    let gui_after = host.artifact(&gui);
    drop(window);

    assert_eq!(overrides_of(&row(&gui_after, 0), "title").text.as_deref(), Some("김"));
    assert_eq!(overrides_of(&row(&gui_after, 1), "title").text.as_deref(), Some("박"));

    let backend = HostBackend::new(host);
    let mcp = Mcp::connect(backend.clone());
    let operation = Operation::SetGroupRepeat {
        node_id: GROUP.into(),
        expected: Some(RepeatSpec { table_id: "table".into(), component_page_id: COMPONENT.into(), bindings: Vec::new() }),
        repeat: Some(RepeatSpec {
            table_id: "table".into(),
            component_page_id: COMPONENT.into(),
            bindings: vec![RepeatBinding { source_node_id: "title".into(), column: 0 }],
        }),
    };
    let mcp_after = mcp_change(&backend, &mcp, &mcp_canvas, vec![operation]);
    assert_same_content(&before, &gui_after, &mcp_after);
}

#[test]
fn the_condition_command_gives_the_same_canvas_by_command_and_by_tool() {
    let mut host = Host::start();
    let gui = canvas_with_table(&mut host, "반복");
    let mcp_canvas = canvas_with_table(&mut host, "반복");
    for canvas in [&gui, &mcp_canvas] {
        create_repeat(&mut host, canvas);
    }
    let before = host.artifact(&gui);

    let mut window = Window::open(&mut host, &gui);
    window.step(&mut host, "select.node", json!({ "node_id": "card" })).expect("the card is selected");
    window
        .step(&mut host, "repeat.condition_set", json!({ "node_id": "card", "column": 1, "op": "equals", "value": "개발" }))
        .expect("the condition is sent");
    let gui_after = host.artifact(&gui);
    drop(window);

    // Only the second row's role differs from the value, so only its card is hidden.
    assert_eq!(overrides_of(&row(&gui_after, 0), "card").hidden, None, "the first row passes");
    assert_eq!(overrides_of(&row(&gui_after, 1), "card").hidden, Some(true), "the second row is hidden");

    let backend = HostBackend::new(host);
    let mcp = Mcp::connect(backend.clone());
    let operation = Operation::SetGroupCondition {
        node_id: "card".into(),
        expected: None,
        condition: Some(ShowCondition { column: 1, op: ConditionOp::Equals, value: Some("개발".into()) }),
    };
    let mcp_after = mcp_change(&backend, &mcp, &mcp_canvas, vec![operation]);
    assert_same_content(&before, &gui_after, &mcp_after);
}

#[test]
fn clearing_the_repeat_removes_the_rows_and_one_undo_brings_them_back() {
    let mut host = Host::start();
    let gui = canvas_with_table(&mut host, "반복");
    create_repeat(&mut host, &gui);
    let before = host.artifact(&gui);
    assert_eq!(group(&before).children.len(), 2);

    let mut window = Window::open(&mut host, &gui);
    window.step(&mut host, "select.node", json!({ "node_id": GROUP })).expect("the group is selected");
    window.step(&mut host, "repeat.clear", json!({ "group_id": GROUP })).expect("the repeat is cleared");
    let cleared = host.artifact(&gui);
    assert!(group(&cleared).children.is_empty(), "the rows go with the repeat");
    assert_eq!(group(&cleared).ext.repeat, None);

    window.step(&mut host, "edit.undo", json!({})).expect("the clear is undone");
    drop(window);
    let undone = host.artifact(&gui);
    assert_eq!(group(&undone).children.len(), 2, "the rows come back");
    assert_eq!(content_of(&undone), content_of(&before));
}

#[test]
fn the_bind_command_refuses_a_group_that_is_not_a_repeat_and_changes_nothing() {
    let mut host = Host::start();
    let gui = canvas_with_table(&mut host, "반복");
    let before = host.artifact(&gui);

    let mut window = Window::open(&mut host, &gui);
    let result = window.step(&mut host, "repeat.bind", json!({ "group_id": GROUP, "bindings": [] }));
    drop(window);
    assert!(result.is_err(), "there is no repeat group named {GROUP}");
    assert_eq!(content_of(&host.artifact(&gui)), content_of(&before));
}

#[test]
fn the_create_command_refuses_a_node_that_is_not_on_a_page_and_changes_nothing() {
    let mut host = Host::start();
    let gui = canvas_with_table(&mut host, "반복");
    let before = host.artifact(&gui);

    let mut window = Window::open(&mut host, &gui);
    window.step(&mut host, "select.node", json!({ "node_id": "card" })).expect("the card is selected");
    let result = window.step(&mut host, "repeat.create", json!({ "node_id": "missing", "table_id": "table" }));
    drop(window);
    assert_eq!(result, Err("최상위 항목을 고르세요".to_owned()));
    assert_eq!(content_of(&host.artifact(&gui)), content_of(&before));
}

#[test]
fn the_engine_refuses_a_binding_outside_the_table_and_the_document_stays() {
    let mut host = Host::start();
    let gui = canvas_with_table(&mut host, "반복");
    create_repeat(&mut host, &gui);
    let before = host.artifact(&gui);

    let mut window = Window::open(&mut host, &gui);
    window.step(&mut host, "select.node", json!({ "node_id": GROUP })).expect("the group is selected");
    window
        .step(&mut host, "repeat.bind", json!({ "group_id": GROUP, "bindings": [{ "source_node_id": "title", "column": 9 }] }))
        .expect("the change is sent; the engine answers");
    drop(window);
    assert_eq!(content_of(&host.artifact(&gui)), content_of(&before), "a refused binding leaves the document as it was");
    assert_eq!(group(&host.artifact(&gui)).ext.repeat.as_ref().map(|spec| spec.bindings.len()), Some(0));
}

/// A window whose RPC calls are recorded, as in the path tests: the calls are answered by the host, and the frames are drawn.
struct Recorded {
    app: DesktopApp,
    replies: Sender<RpcReply>,
    work: Receiver<Work>,
    ctx: Context,
    calls: Vec<(String, Value)>,
    /// The errors the host answered the window's calls with, by method.
    refusals: Vec<(String, DltError)>,
}

impl Recorded {
    fn open(host: &mut Host, canvas: &str) -> Self {
        let (rpc, replies, work) = RpcWorker::detached();
        let ctx = Context::default();
        theme::install(&ctx);
        let mut window = Self { app: DesktopApp::new(rpc, "작업공간".into(), None, None), replies, work, ctx, calls: Vec::new(), refusals: Vec::new() };
        window.settle(host);
        let list = host.call("workspace.list", json!({})).expect("the work list");
        let summary = list["artifacts"].as_array().and_then(|all| all.iter().find(|item| item["id"] == canvas)).cloned().expect("the canvas is listed");
        window.app.activate(serde_json::from_value::<ArtifactSummary>(summary).expect("a summary"));
        window.settle(host);
        window.draw();
        window
    }

    /// Draws the panel as the window does (the frames run the properties code, including the repeat controls).
    fn draw(&mut self) {
        for _ in 0..2 {
            let app = &mut self.app;
            let _ = self.ctx.run(screen(1.0, 1280.0, 800.0), |ctx| {
                app.draw_frame(ctx);
            });
        }
    }

    fn settle(&mut self, host: &mut Host) {
        settle_work(&self.work, &mut self.app, &self.ctx, |item| {
            if let Work::Call { ticket, method, params } = item {
                self.calls.push((method.clone(), params.clone()));
                let result = match host.call(&method, params) {
                    Ok(value) => Ok(RpcPayload::Json(value)),
                    Err(error) => {
                        self.refusals.push((method, error.clone()));
                        Err(error)
                    }
                };
                self.replies.send(RpcReply { ticket, result }).expect("the window listens");
            }
        });
    }

    /// Draws one frame and returns the text it drew.
    fn drawn(&mut self) -> Vec<String> {
        let mut texts = Vec::new();
        for _ in 0..2 {
            let app = &mut self.app;
            // Tall enough for the whole properties column: the notice sits under the panel's controls.
            let output = self.ctx.run(screen(1.0, 1280.0, 4000.0), |ctx| {
                app.draw_frame(ctx);
            });
            texts = drawn_text(&output);
        }
        texts
    }

    /// The notice the window shows now, if any.
    fn message(&self) -> Option<String> {
        self.app.notices.current(std::time::Instant::now()).map(|notice| notice.text.clone())
    }

    /// Runs one command as the window does, then plays the host's side of what it sent and draws the frame.
    fn invoke(&mut self, host: &mut Host, id: &str, args: Value) -> Result<(), String> {
        let result = self.app.invoke(id, args);
        self.settle(host);
        self.draw();
        result
    }

    fn select(&mut self, host: &mut Host, id: &str) {
        self.invoke(host, "select.node", json!({ "node_id": id })).expect("the node is selected");
    }

    /// The operation kinds of the change sets sent after the first `mark` change sets, in order.
    fn op_kinds_since(&self, mark: usize) -> Vec<String> {
        self.calls
            .iter()
            .filter(|(method, _)| method == "changes.apply")
            .skip(mark)
            .flat_map(|(_, params)| params["operations"].as_array().cloned().unwrap_or_default())
            .map(|op| op["op"].as_str().expect("an operation kind").to_owned())
            .collect()
    }

    fn change_count(&self) -> usize {
        self.calls.iter().filter(|(method, _)| method == "changes.apply").count()
    }
}

#[test]
fn the_panel_offers_create_on_a_top_level_node_while_a_table_is_on_the_page() {
    let mut host = Host::start();
    let gui = canvas_with_table(&mut host, "반복");
    let mut window = Recorded::open(&mut host, &gui);

    window.select(&mut host, "card");
    assert_eq!(window.app.repeat_panel(), RepeatPanel::Create);
    window.select(&mut host, "table");
    assert_eq!(window.app.repeat_panel(), RepeatPanel::Nothing, "a table is not repeated");
    window.select(&mut host, "h1");
    assert_eq!(window.app.repeat_panel(), RepeatPanel::Nothing, "a cell is not top-level");
    window.app.select_clear();
    assert_eq!(window.app.repeat_panel(), RepeatPanel::Nothing, "nothing is selected");
}

/// `canvas_with_table` plus a second, one-column table `table-2` on the same page: the panel must not offer a table.
fn canvas_with_two_tables(host: &mut Host) -> String {
    let canvas = canvas_with_table(host, "반복");
    let cell = Node {
        layout: Some(LayoutSpec { width_mode: "fill".into(), height_mode: "hug".into(), ..LayoutSpec::default() }),
        ..plain("t2-c1", NodeKind::Rectangle, rect(0, 0, 120_000, 36_000), "값", Vec::new())
    };
    let second = Node {
        layout: Some(LayoutSpec { mode: "grid".into(), height_mode: "hug".into(), grid_columns: Some(1), gap: 0, row_gap: Some(0), ..LayoutSpec::default() }),
        ext: NodeExt {
            table: Some(CanvasTable {
                rows: vec![CanvasTableRow { id: "t2-row".into(), cell_ids: vec!["t2-c1".into()] }],
                header_row_id: None,
                column_widths: None,
                cell_spans: Default::default(),
            }),
            ..NodeExt::default()
        },
        ..plain("table-2", NodeKind::Group, rect(500_000, 2_000_000, 1_000_000, 300_000), "", vec![cell])
    };
    host.apply(&canvas, vec![Operation::AddNode { page_id: PAGE.into(), parent_id: None, node: second }], content()).expect("the second table");
    canvas
}

#[test]
fn a_table_is_not_offered_for_repeat_when_the_document_has_two_tables() {
    let mut host = Host::start();
    let gui = canvas_with_two_tables(&mut host);
    let mut window = Recorded::open(&mut host, &gui);
    assert_eq!(window.app.data_tables().len(), 2, "both tables are offered as sources");
    window.select(&mut host, "table-2");
    assert_eq!(window.app.repeat_panel(), RepeatPanel::Nothing, "a table is not repeated, whatever tables the document has");
    window.select(&mut host, "card");
    assert_eq!(window.app.repeat_panel(), RepeatPanel::Create);
}

#[test]
fn the_panel_moves_from_create_to_the_repeat_and_the_component_states_as_the_commands_run() {
    let mut host = Host::start();
    let gui = canvas_with_table(&mut host, "반복");
    let mut window = Recorded::open(&mut host, &gui);
    window.select(&mut host, "card");

    // The "반복 만들기" button sends the arguments the panel builds; the command runs them.
    window.invoke(&mut host, "repeat.create", create_args("card", "table", COMPONENT, GROUP)).expect("the repeat is created");
    window.select(&mut host, GROUP);
    assert_eq!(window.app.repeat_panel(), RepeatPanel::Repeat);
    window.select(&mut host, "card");
    assert_eq!(window.app.repeat_panel(), RepeatPanel::Condition, "a group of the component page takes a condition");
    window.select(&mut host, "title");
    assert_eq!(window.app.repeat_panel(), RepeatPanel::Nothing, "a text child of the component is not a group");
}

#[test]
fn the_repeat_commands_the_panel_sends_bind_and_clear_the_rows() {
    let mut host = Host::start();
    let gui = canvas_with_table(&mut host, "반복");
    let mut window = Recorded::open(&mut host, &gui);
    window.select(&mut host, "card");
    window.invoke(&mut host, "repeat.create", create_args("card", "table", COMPONENT, GROUP)).expect("the repeat is created");

    window.select(&mut host, GROUP);
    window.invoke(&mut host, "repeat.bind", bind_args(GROUP, &[("title".to_owned(), 0)])).expect("the binding is sent");
    assert_eq!(overrides_of(&row(&host.artifact(&gui), 0), "title").text.as_deref(), Some("김"));

    window.invoke(&mut host, "repeat.clear", json!({ "group_id": GROUP })).expect("the repeat is cleared");
    assert!(group(&host.artifact(&gui)).children.is_empty());
}

#[test]
fn the_condition_arguments_omit_the_value_an_operator_does_not_compare() {
    assert_eq!(condition_args("card", 1, "equals", "개발"), json!({ "node_id": "card", "column": 1, "op": "equals", "value": "개발" }));
    assert_eq!(condition_args("card", 1, "is_empty", "stale text"), json!({ "node_id": "card", "column": 1, "op": "is_empty" }));
    assert_eq!(bind_args("list", &[("title".into(), 2)]), json!({ "group_id": "list", "bindings": [{ "source_node_id": "title", "column": 2 }] }));

    let mut host = Host::start();
    let gui = canvas_with_table(&mut host, "반복");
    let mut window = Recorded::open(&mut host, &gui);
    window.select(&mut host, "card");
    window.invoke(&mut host, "repeat.create", create_args("card", "table", COMPONENT, GROUP)).expect("the repeat is created");
    window.select(&mut host, "card");
    window.invoke(&mut host, "repeat.condition_set", condition_args("card", 1, "equals", "개발")).expect("the condition is set");
    assert_eq!(overrides_of(&row(&host.artifact(&gui), 1), "card").hidden, Some(true));
    window.invoke(&mut host, "repeat.condition_clear", json!({ "node_id": "card" })).expect("the condition is cleared");
    assert_eq!(overrides_of(&row(&host.artifact(&gui), 1), "card").hidden, None);
}

#[test]
fn each_repeat_command_sends_the_operation_kinds_its_registry_entry_declares() {
    let mut host = Host::start();
    let gui = canvas_with_table(&mut host, "반복");
    let mut window = Recorded::open(&mut host, &gui);
    let declared = |id: &str| -> Vec<String> {
        commands::core_calls(id)
            .expect("the command declares its calls")
            .iter()
            .filter_map(|call| call.strip_prefix("changes.apply:").map(str::to_owned))
            .collect()
    };
    let steps: [(&str, Value); 5] = [
        ("repeat.create", create_args("card", "table", COMPONENT, GROUP)),
        ("repeat.bind", bind_args(GROUP, &[("title".to_owned(), 0)])),
        ("repeat.condition_set", condition_args("card", 1, "equals", "개발")),
        ("repeat.condition_clear", json!({ "node_id": "card" })),
        ("repeat.clear", json!({ "group_id": GROUP })),
    ];
    for (id, args) in steps {
        let target = if id == "repeat.bind" || id == "repeat.clear" { GROUP } else { "card" };
        window.select(&mut host, target);
        let mark = window.change_count();
        window.invoke(&mut host, id, args).unwrap_or_else(|error| panic!("{id} refused: {error}"));
        assert_eq!(window.op_kinds_since(mark), declared(id), "the operations {id} sends, in order");
        // One change set, as `commands::one_tracked_change` declares to the UI channel.
        assert_eq!(window.change_count() - mark, 1, "{id} sends one change set");
        assert!(commands::one_tracked_change(id), "{id} is declared as one change set");
    }
}

#[test]
fn every_repeat_diagnostic_code_has_a_korean_sentence() {
    use designllm_contracts::repeat_groups::*;
    let codes = [
        CANVAS_REPEAT_GROUP_NOT_EMPTY, CANVAS_REPEAT_CONDITION_SCOPE, CANVAS_REPEAT_LIMIT, CANVAS_REPEAT_STALE, CANVAS_REPEAT_FLATTENED,
        CANVAS_REPEAT_TARGET, CANVAS_REPEAT_TABLE, CANVAS_REPEAT_NESTED, CANVAS_REPEAT_COLUMN, CANVAS_REPEAT_BINDING,
        CANVAS_REPEAT_CONDITION, CANVAS_REPEAT_EXPECTED,
    ];
    for code in codes {
        let sentence = crate::app::messages::diagnostic_message(code).unwrap_or_else(|| panic!("no sentence for {code}"));
        assert!(sentence.chars().any(|c| ('가'..='힣').contains(&c)), "{code} has a Korean sentence");
    }
}

const REPEAT_COMMANDS: [&str; 5] = ["repeat.create", "repeat.bind", "repeat.clear", "repeat.condition_set", "repeat.condition_clear"];

#[test]
fn each_repeat_command_is_enabled_in_a_canvas_editor_whatever_is_selected() {
    let mut host = Host::start();
    let gui = canvas_with_table(&mut host, "반복");
    let mut window = Recorded::open(&mut host, &gui);
    let enabled = |window: &Recorded| REPEAT_COMMANDS.map(|id| window.app.command_enabled_for_current_kind(id));
    assert_eq!(enabled(&window), [true; 5], "nothing is selected: the arguments name the nodes");
    for id in ["table", "card", "h1"] {
        window.select(&mut host, id);
        assert_eq!(enabled(&window), [true; 5], "{id} is selected");
    }
    window.invoke(&mut host, "nav.home", json!({})).expect("the home screen opens");
    assert_eq!(enabled(&window), [false; 5], "the home screen edits nothing");
}

#[test]
fn the_agent_channel_lists_every_repeat_command_and_runs_the_create_as_one_change() {
    use designllm_contracts::{new_id, UiPermission, UiRun, UiRunExpected, UiRunStatus};
    let mut host = Host::start();
    let gui = canvas_with_table(&mut host, "반복");
    let mut window = Window::open(&mut host, &gui);
    let listed = |window: &Window| -> Vec<bool> {
        let commands = window.app.ui_commands();
        REPEAT_COMMANDS.iter().map(|id| commands.iter().find(|command| command.id == *id).expect("listed").enabled).collect()
    };
    assert_eq!(listed(&window), [true; 5], "the agent channel offers all five");
    window.step(&mut host, "select.node", json!({ "node_id": "table" })).expect("the table is selected");
    assert_eq!(listed(&window), [true; 5], "the selection does not change the registry");

    window.app.publish_ui_if_due();
    window.settle(&mut host);
    let registry = host.call("ui.command.list", json!({})).expect("the window's registry is published");
    let create = registry["commands"].as_array().and_then(|all| all.iter().find(|command| command["id"] == "repeat.create")).cloned();
    assert_eq!(create.map(|command| command["enabled"].clone()), Some(json!(true)), "the host holds repeat.create as enabled");

    // An agent's run of the five-operation change, on the state the window published.
    let state = window.app.ui_state();
    let run = UiRun {
        run_id: new_id(),
        view_id: window.app.ui.view_id.clone(),
        expected: Some(UiRunExpected {
            state_seq: window.app.ui.state_seq.expect("the publish was accepted"),
            screen: state.screen,
            artifact_id: state.artifact_id,
            revision: state.revision,
            selection: state.selection,
            page_index: state.page_index,
        }),
        command_id: "repeat.create".into(),
        args: create_args("card", "table", COMPONENT, GROUP),
        actor: "agent".into(),
        permission: UiPermission::Change,
        allowed_intents: content(),
        status: UiRunStatus::Queued,
        timeout_ms: 1_000,
        result: None,
        error: None,
    };
    assert_eq!(window.app.ui_run_refusal(&run).map(|error| error.message), None, "the run passes the channel's checks");
    let mark = window.calls.len();
    window.app.take_ui_runs(&json!({ "ui_runs": [run] }));
    window.settle(&mut host);
    let sent: Vec<&Value> = window.calls[mark..].iter().filter(|(method, _, _)| method == "changes.apply").map(|(_, params, _)| params).collect();
    assert_eq!(sent.len(), 1, "one change set");
    assert_eq!(sent[0]["actor"], "agent", "the change is the agent's");
    assert_eq!(group(&host.artifact(&gui)).children.len(), 2, "the repeat is created");
}

/// The MCP `changes_apply` path with operations the engine refuses: the tool's error.
fn mcp_refusal(backend: &HostBackend, mcp: &Mcp, artifact: &str, operations: Vec<Operation>) -> DltError {
    let (workspace, revision) = backend.with(|host| (host.workspace_id.clone(), host.artifact(artifact).revision));
    let arguments = change_set_arguments(&workspace, artifact, revision, &[Intent::Content], operations);
    mcp.try_call("changes_apply", arguments).expect_err("the engine refuses the change")
}

/// The same refused change by the command and by the MCP tool, on two canvases that `prepare` set up alike: neither changes
/// its canvas, both stay alike, and the window says what the tool's error says.
fn assert_same_refusal(
    prepare: impl Fn(&mut Host, &str),
    select: &str,
    command: &str,
    args: Value,
    operations: impl FnOnce(&mut Host, &str) -> Vec<Operation>,
) {
    let mut host = Host::start();
    let gui = canvas_with_table(&mut host, "반복");
    let mcp_canvas = canvas_with_table(&mut host, "반복");
    prepare(&mut host, &gui);
    prepare(&mut host, &mcp_canvas);
    let before = host.artifact(&gui);

    let mut window = Recorded::open(&mut host, &gui);
    window.select(&mut host, select);
    window.invoke(&mut host, command, args).expect("the change is sent; the engine answers");
    let shown = window.message();
    let gui_error = window.refusals.iter().find(|(method, _)| method == "changes.apply").map(|(_, error)| error.clone()).expect("the engine refused the change");
    drop(window);
    let gui_after = host.artifact(&gui);
    assert_eq!(content_of(&gui_after), content_of(&before), "{command}: the refused change left the canvas as it was");

    let backend = HostBackend::new(host);
    let mcp = Mcp::connect(backend.clone());
    let operations = backend.with(|host| operations(host, &mcp_canvas));
    let error = mcp_refusal(&backend, &mcp, &mcp_canvas, operations);
    assert_eq!(content_of(&backend.with(|host| host.artifact(&mcp_canvas))), content_of(&gui_after), "{command}: both canvases stay alike");
    assert_eq!(gui_error.code, error.code, "{command}: the window's error code is the tool's");
    assert_eq!(diagnostic_codes(&gui_error), diagnostic_codes(&error), "{command}: the window's diagnostics are the tool's");
    assert_eq!(shown, Some(crate::app::messages::user_error(&error)), "{command}: the window says what the tool's error says");
}

fn diagnostic_codes(error: &DltError) -> Vec<String> {
    error.diagnostics.iter().map(|diagnostic| diagnostic.code.clone()).collect()
}

fn lock(host: &mut Host, canvas: &str, node_id: &str) {
    let operation = Operation::SetNodeAppearance { node_ids: vec![node_id.into()], image_fit: None, locked: Some(true) };
    host.apply(canvas, vec![operation], [Intent::Design].into_iter().collect()).expect("the node is locked");
}

fn empty_repeat() -> RepeatSpec {
    RepeatSpec { table_id: "table".into(), component_page_id: COMPONENT.into(), bindings: Vec::new() }
}

fn role_condition() -> ShowCondition {
    ShowCondition { column: 1, op: ConditionOp::Equals, value: Some("개발".into()) }
}

#[test]
fn the_create_command_and_the_tool_refuse_a_missing_table_alike_and_the_whole_change_set_rolls_back() {
    assert_same_refusal(
        |_, _| {},
        "card",
        "repeat.create",
        create_args("card", "missing", COMPONENT, GROUP),
        |host, canvas| {
            let artifact = host.artifact(canvas);
            create_operations(&top(&artifact, "card"), PAGE, 1, page_size(&artifact), "missing", COMPONENT, GROUP)
        },
    );
}

#[test]
fn the_bind_command_and_the_tool_refuse_a_column_outside_the_table_alike() {
    assert_same_refusal(
        |host, canvas| create_repeat(host, canvas),
        GROUP,
        "repeat.bind",
        bind_args(GROUP, &[("title".to_owned(), 9)]),
        |_, _| {
            let repeat = RepeatSpec { bindings: vec![RepeatBinding { source_node_id: "title".into(), column: 9 }], ..empty_repeat() };
            vec![Operation::SetGroupRepeat { node_id: GROUP.into(), expected: Some(empty_repeat()), repeat: Some(repeat) }]
        },
    );
}

#[test]
fn the_clear_command_and_the_tool_refuse_a_locked_group_alike() {
    assert_same_refusal(
        |host, canvas| {
            create_repeat(host, canvas);
            lock(host, canvas, GROUP);
        },
        GROUP,
        "repeat.clear",
        json!({ "group_id": GROUP }),
        |_, _| vec![Operation::SetGroupRepeat { node_id: GROUP.into(), expected: Some(empty_repeat()), repeat: None }],
    );
}

#[test]
fn the_condition_command_and_the_tool_refuse_an_empty_check_with_a_value_alike() {
    // `is_empty` takes no value, and an empty one is still a value: both paths send it, and the engine refuses both.
    assert_same_refusal(
        |host, canvas| create_repeat(host, canvas),
        "card",
        "repeat.condition_set",
        json!({ "node_id": "card", "column": 1, "op": "is_empty", "value": "" }),
        |_, _| {
            let condition = ShowCondition { column: 1, op: ConditionOp::IsEmpty, value: Some(String::new()) };
            vec![Operation::SetGroupCondition { node_id: "card".into(), expected: None, condition: Some(condition) }]
        },
    );
}

#[test]
fn the_condition_clear_command_and_the_tool_refuse_a_locked_group_alike() {
    assert_same_refusal(
        |host, canvas| {
            create_repeat(host, canvas);
            let operation = Operation::SetGroupCondition { node_id: "card".into(), expected: None, condition: Some(role_condition()) };
            host.apply(canvas, vec![operation], content()).expect("the condition is set");
            lock(host, canvas, "card");
        },
        "card",
        "repeat.condition_clear",
        json!({ "node_id": "card" }),
        |_, _| vec![Operation::SetGroupCondition { node_id: "card".into(), expected: Some(role_condition()), condition: None }],
    );
}

/// The command `command` on one canvas and `operations` by the MCP tool on another, both prepared by `prepare`: the same
/// content, and a change.
fn assert_same_result(prepare: impl Fn(&mut Host, &str), select: &str, command: &str, args: Value, operations: Vec<Operation>) -> Artifact {
    let mut host = Host::start();
    let gui = canvas_with_table(&mut host, "반복");
    let mcp_canvas = canvas_with_table(&mut host, "반복");
    prepare(&mut host, &gui);
    prepare(&mut host, &mcp_canvas);
    let before = host.artifact(&gui);

    let mut window = Window::open(&mut host, &gui);
    window.step(&mut host, "select.node", json!({ "node_id": select })).expect("the node is selected");
    window.step(&mut host, command, args).unwrap_or_else(|error| panic!("{command} refused: {error}"));
    drop(window);
    let gui_after = host.artifact(&gui);

    let backend = HostBackend::new(host);
    let mcp = Mcp::connect(backend.clone());
    let mcp_after = mcp_change(&backend, &mcp, &mcp_canvas, operations);
    assert_same_content(&before, &gui_after, &mcp_after);
    gui_after
}

#[test]
fn the_clear_command_gives_the_same_canvas_by_command_and_by_tool() {
    let after = assert_same_result(
        |host, canvas| create_repeat(host, canvas),
        GROUP,
        "repeat.clear",
        json!({ "group_id": GROUP }),
        vec![Operation::SetGroupRepeat { node_id: GROUP.into(), expected: Some(empty_repeat()), repeat: None }],
    );
    assert!(group(&after).children.is_empty(), "the rows go with the repeat");
}

#[test]
fn the_condition_clear_command_gives_the_same_canvas_by_command_and_by_tool() {
    let after = assert_same_result(
        |host, canvas| {
            create_repeat(host, canvas);
            let operation = Operation::SetGroupCondition { node_id: "card".into(), expected: None, condition: Some(role_condition()) };
            host.apply(canvas, vec![operation], content()).expect("the condition is set");
        },
        "card",
        "repeat.condition_clear",
        json!({ "node_id": "card" }),
        vec![Operation::SetGroupCondition { node_id: "card".into(), expected: Some(role_condition()), condition: None }],
    );
    assert_eq!(overrides_of(&row(&after, 1), "card").hidden, None, "the second row shows again");
}

#[test]
fn an_empty_comparison_value_is_sent_as_the_tool_sends_it() {
    // Both cells of the role column have text, so `equals ""` hides both rows by either path.
    let after = assert_same_result(
        |host, canvas| create_repeat(host, canvas),
        "card",
        "repeat.condition_set",
        condition_args("card", 1, "equals", ""),
        vec![Operation::SetGroupCondition {
            node_id: "card".into(),
            expected: None,
            condition: Some(ShowCondition { column: 1, op: ConditionOp::Equals, value: Some(String::new()) }),
        }],
    );
    assert_eq!([0, 1].map(|index| overrides_of(&row(&after, index), "card").hidden), [Some(true); 2]);
}

#[test]
fn one_undo_takes_the_whole_create_back() {
    let mut host = Host::start();
    let gui = canvas_with_table(&mut host, "반복");
    let before = host.artifact(&gui);
    let mut window = Window::open(&mut host, &gui);
    window.step(&mut host, "select.node", json!({ "node_id": "card" })).expect("the card is selected");
    window.step(&mut host, "repeat.create", create_args("card", "table", COMPONENT, GROUP)).expect("the repeat is created");
    assert_eq!(group(&host.artifact(&gui)).children.len(), 2);
    window.step(&mut host, "edit.undo", json!({})).expect("the create is undone");
    drop(window);
    let undone = host.artifact(&gui);
    assert_eq!(content_of(&undone), content_of(&before), "the page, the move, the group and the repeat go back together");
}

/// A canvas whose table is in a group `holder` on a second page `page-2`; the card stays on `page-1`.
fn canvas_with_table_elsewhere(host: &mut Host) -> String {
    let canvas = canvas_with_table(host, "반복");
    let page = designllm_contracts::Page::new("page-2", Size::slide());
    let holder = plain("holder", NodeKind::Group, rect(0, 0, 3_000_000, 2_000_000), "", Vec::new());
    let operations = vec![
        Operation::InsertPage { index: 1, page },
        Operation::AddNode { page_id: "page-2".into(), parent_id: None, node: holder },
        Operation::MoveNode { node_id: "table".into(), page_id: "page-2".into(), parent_id: Some("holder".into()), index: 0 },
    ];
    host.apply(&canvas, operations, content()).expect("the table moves into a group on another page");
    canvas
}

#[test]
fn the_panel_offers_every_table_of_the_document_and_the_engine_takes_one_on_another_page() {
    let mut host = Host::start();
    let gui = canvas_with_table_elsewhere(&mut host);
    let mut window = Recorded::open(&mut host, &gui);
    window.select(&mut host, "card");
    assert_eq!(window.app.repeat_panel(), RepeatPanel::Create, "the table is on another page, in a group");
    // The table is the only one: "표 1", whatever else is on its page.
    assert_eq!(window.app.data_tables(), [("table".to_owned(), "표 1".to_owned())]);
    window.invoke(&mut host, "repeat.create", create_args("card", "table", COMPONENT, GROUP)).expect("the repeat is sent");
    assert_eq!(group(&host.artifact(&gui)).children.len(), 2, "the engine expands the rows of a table elsewhere");
}

#[test]
fn a_refused_binding_keeps_the_draft() {
    let mut host = Host::start();
    let gui = canvas_with_table(&mut host, "반복");
    create_repeat(&mut host, &gui);
    let mut window = Recorded::open(&mut host, &gui);
    window.select(&mut host, GROUP);
    let key = window.app.draft_key("repeat-bindings", GROUP);
    let draft = vec![("title".to_owned(), 9)];
    let edit = Draft { base: Vec::new(), value: draft.clone() };
    window.ctx.data_mut(|data| data.insert_temp(key, edit.clone()));

    window.invoke(&mut host, "repeat.bind", bind_args(GROUP, &draft)).expect("the change is sent; the engine refuses it");
    assert_eq!(window.app.draft_key("repeat-bindings", GROUP), key, "nothing was stored");
    assert_eq!(window.ctx.data(|data| data.get_temp::<Draft<Vec<(String, u32)>>>(key)), Some(edit), "the draft is there to correct");
}

#[test]
fn a_stored_binding_equal_to_the_draft_ends_the_draft() {
    let mut host = Host::start();
    let gui = canvas_with_table(&mut host, "반복");
    create_repeat(&mut host, &gui);
    let mut window = Recorded::open(&mut host, &gui);
    window.select(&mut host, GROUP);
    let key = window.app.draft_key("repeat-bindings", GROUP);
    let applied = vec![("title".to_owned(), 0)];
    window.ctx.data_mut(|data| data.insert_temp(key, Draft { base: Vec::new(), value: applied.clone() }));

    window.invoke(&mut host, "repeat.bind", bind_args(GROUP, &applied)).expect("the binding is stored");
    assert_eq!(window.ctx.data(|data| data.get_temp::<Draft<Vec<(String, u32)>>>(key)), None, "the change was applied: the draft is gone");
}

#[test]
fn a_stored_binding_that_differs_keeps_the_draft_and_says_so() {
    let mut host = Host::start();
    let gui = canvas_with_table(&mut host, "반복");
    create_repeat(&mut host, &gui);
    let mut window = Recorded::open(&mut host, &gui);
    window.select(&mut host, GROUP);
    let key = window.app.draft_key("repeat-bindings", GROUP);
    let edit = Draft { base: Vec::new(), value: vec![("title".to_owned(), 1)] };
    window.ctx.data_mut(|data| data.insert_temp(key, edit.clone()));

    window.invoke(&mut host, "repeat.bind", bind_args(GROUP, &[("title".to_owned(), 0)])).expect("another binding is stored");
    assert_eq!(window.ctx.data(|data| data.get_temp::<Draft<Vec<(String, u32)>>>(key)), Some(edit), "the unapplied edit is kept");
    let texts = window.drawn();
    assert!(texts.iter().any(|text| text.contains("저장된 값이 바뀌었습니다")), "the panel says the stored value moved: {texts:?}");
}

#[test]
fn a_refused_condition_clear_keeps_the_draft() {
    let mut host = Host::start();
    let gui = canvas_with_table(&mut host, "반복");
    create_repeat(&mut host, &gui);
    host.apply(&gui, vec![Operation::SetGroupCondition { node_id: "card".into(), expected: None, condition: Some(role_condition()) }], content())
        .expect("the condition is set");
    lock(&mut host, &gui, "card");
    let mut window = Recorded::open(&mut host, &gui);
    window.select(&mut host, "card");
    let key = window.app.draft_key("repeat-condition", "card");
    let form = ConditionForm { column: 1, op: "equals".into(), value: "개발".into() };
    let edit = ConditionDraft { base: Some(form.clone()), form, clearing: true };
    window.ctx.data_mut(|data| data.insert_temp(key, edit.clone()));

    window.invoke(&mut host, "repeat.condition_clear", json!({ "node_id": "card" })).expect("the change is sent; the engine refuses it");
    assert_eq!(node_anywhere(&host.artifact(&gui), "card").ext.condition, Some(role_condition()), "the locked condition stays");
    assert_eq!(window.ctx.data(|data| data.get_temp::<ConditionDraft>(key)), Some(edit), "the refused clear keeps the form");
}

#[test]
fn a_stored_condition_clear_ends_the_draft() {
    let mut host = Host::start();
    let gui = canvas_with_table(&mut host, "반복");
    create_repeat(&mut host, &gui);
    host.apply(&gui, vec![Operation::SetGroupCondition { node_id: "card".into(), expected: None, condition: Some(role_condition()) }], content())
        .expect("the condition is set");
    let mut window = Recorded::open(&mut host, &gui);
    window.select(&mut host, "card");
    let key = window.app.draft_key("repeat-condition", "card");
    let form = ConditionForm { column: 1, op: "equals".into(), value: "개발".into() };
    window.ctx.data_mut(|data| data.insert_temp(key, ConditionDraft { base: Some(form.clone()), form, clearing: true }));

    window.invoke(&mut host, "repeat.condition_clear", json!({ "node_id": "card" })).expect("the condition is cleared");
    assert_eq!(node_anywhere(&host.artifact(&gui), "card").ext.condition, None);
    assert_eq!(window.ctx.data(|data| data.get_temp::<ConditionDraft>(key)), None, "the stored clear ends the draft");
}

#[test]
fn an_edit_of_another_node_keeps_the_bindings_draft() {
    let mut host = Host::start();
    let gui = canvas_with_table(&mut host, "반복");
    create_repeat(&mut host, &gui);
    let mut window = Recorded::open(&mut host, &gui);
    window.select(&mut host, GROUP);
    let key = window.app.draft_key("repeat-bindings", GROUP);
    let draft = vec![("title".to_owned(), 9)];
    window.ctx.data_mut(|data| data.insert_temp(key, Draft { base: Vec::new(), value: draft.clone() }));
    let revision = host.artifact(&gui).revision;

    window.invoke(&mut host, "repeat.condition_set", json!({ "node_id": "card", "column": 1, "op": "equals", "value": "개발" })).expect("the card's condition is sent");
    assert!(host.artifact(&gui).revision > revision, "the document changed");
    assert_eq!(window.app.draft_key("repeat-bindings", GROUP), key, "the draft's key does not follow the document's revision");
    assert_eq!(window.ctx.data(|data| data.get_temp::<Draft<Vec<(String, u32)>>>(key)), Some(Draft { base: Vec::new(), value: draft }), "the unapplied binding is kept");
}

#[test]
fn clearing_the_condition_shows_every_row_again() {
    let mut host = Host::start();
    let gui = canvas_with_table(&mut host, "반복");
    create_repeat(&mut host, &gui);
    let mut window = Window::open(&mut host, &gui);
    window.step(&mut host, "select.node", json!({ "node_id": "card" })).expect("the card is selected");
    window.step(&mut host, "repeat.condition_set", json!({ "node_id": "card", "column": 1, "op": "equals", "value": "개발" })).expect("the condition is set");
    assert_eq!(overrides_of(&row(&host.artifact(&gui), 1), "card").hidden, Some(true));

    window.step(&mut host, "repeat.condition_clear", json!({ "node_id": "card" })).expect("the condition is cleared");
    drop(window);
    let cleared = host.artifact(&gui);
    assert_eq!(node_anywhere(&cleared, "card").ext.condition, None);
    assert_eq!(overrides_of(&row(&cleared, 1), "card").hidden, None, "the second row shows again");
}

#[test]
fn a_repeat_spec_with_an_unknown_field_is_refused_before_it_is_sent() {
    let mut host = Host::start();
    let gui = canvas_with_table(&mut host, "반복");
    create_repeat(&mut host, &gui);
    let mut window = Window::open(&mut host, &gui);
    let result = window.step(&mut host, "repeat.bind", json!({ "group_id": GROUP, "bindings": [{ "source_node_id": "title", "column": 0, "format": "x" }] }));
    drop(window);
    assert!(result.is_err(), "the argument schema refuses the extra field");
}
