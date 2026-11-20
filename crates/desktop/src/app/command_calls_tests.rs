//! Every wired command runs against the real host behind a recording RPC. The core calls it sends (each
//! method once, and `changes.apply:<op>` for each operation kind of a change set) must equal the calls that
//! `commands::core_calls` declares. The reads the window makes to refresh its view after a change belong to
//! no command, so they are left out of both sides.
//!
//! Commands in `NOT_RUN` are not run: a native dialog would open on the user's screen, and a job would start
//! a runtime that calls a model. Their declared calls are checked against the dispatch arms by review.

use std::sync::mpsc::{Receiver, Sender};

use designllm_contracts::{ArtifactBody, ArtifactSummary};
use eframe::egui::Context;
use serde_json::{json, Value};

use super::{
    figure_flow_tests::Host,
    link_flow_tests::canvas_with_three_boxes,
    test_support::{preview_pixels, settle_work},
    tests::screen,
    DesktopApp,
};
use crate::{
    commands::{self, CommandSpec},
    rpc::{RpcPayload, RpcReply, RpcWorker, Work},
    theme,
};

#[path = "ui_channel_command_tests.rs"]
mod ui_channel_command_tests;

/// Commands that are not run here, with the reason.
const NOT_RUN: &[(&str, &str)] = &[
    ("props.path", "needs a selected Path node; covered by properties_path_tests"),
    ("add.image", "native dialog"),
    ("file.save_as", "native dialog"),
    ("file.export", "native dialog"),
    ("file.save", "needs a native document with a save path"),
    ("file.dismiss_loss", "needs a pending export confirmation"),
    ("add.paragraph", "needs a text document"),
    ("add.figure_link", "figure_link_calls_match_the_registry_for_page_and_flow_placement covers both operations"),
    ("web.preview", "needs a document that can materialise a web preview"),
    ("ai.request", "starts a job"),
    ("ai.retry", "starts a job"),
    ("ai.cancel", "cancels a job"),
    ("ai.undo_change", "needs an agent batch"),
    ("canvas.design_system.list", "tested with the shared design-system flow"),
    ("canvas.design_system.publish", "needs a prepared Canvas publisher"),
    ("canvas.design_system.set", "needs a published Canvas source and consumer"),
    ("home.create", "starts a job"),
    ("view.play", "opens the preview window"),
    ("file.export_selection", "native dialog"),
    ("figure.diagram_provenance", "needs a figure with a diagram element; figure_r07_tests sends it"),
    ("figure.common_legend", "needs an open figure; figure_r07_tests sends it"),
    ("figure.visual_review", "needs an open figure; figure_r07_tests sends it"),
    ("figure.image_transform", "needs an image panel; figure_r07_tests sends it"),
    ("figure.image_calibration", "needs an image panel; figure_r07_tests sends it"),
    ("figure.plot_display", "needs an open figure with a plot; parity_tests sends it"), ("canvas.image.replace", "needs a selected canvas picture and an imported asset; parity_tests sends it"),
    ("canvas.image.crop", "needs a canvas picture with an asset; parity_tests sends it"), ("canvas.board_memos.set", "needs a canvas page; parity_tests sends it"), ("canvas.page_sections.set", "needs a canvas page; parity_tests sends it"),
    ("proposal.apply", "needs a listed proposal; proposals_tests sends it"), ("proposal.select", "needs a listed proposal; proposals_tests sends it"), ("proposal.discard", "needs a listed proposal; proposals_tests sends it"),
    ("web.mapped_edit", "needs an open WebCode file with a mapping; web_mapping_tests sends it"), ("web.open_file", "needs a WebCode project with files; web_mapping_tests sends it"),
    ("repeat.create", "needs a top-level canvas node and a data table; repeat_groups_tests sends it"), ("repeat.bind", "needs a repeat group; repeat_groups_tests sends it"),
    ("repeat.clear", "needs a repeat group; repeat_groups_tests sends it"), ("repeat.condition_set", "needs a repeat component group; repeat_groups_tests sends it"),
    ("repeat.condition_clear", "needs a condition; repeat_groups_tests sends it"),
    ("slides.theme", "needs an open slides deck; slides_theme tests send it"),
];

/// Commands that need another kind of document or a prepared selection: they run in
/// `every_kind_command_sends_the_core_calls_it_declares` instead.
const KIND_RUN: &[&str] = &[
    "add.figure_panel", "add.diagram_node", "add.connector", "figure.add_equation", "figure.arrange_panels", "figure.remove_panel",
    "figure.reorder_panels", "figure.caption", "figure.panel_label", "figure.plot", "figure.axis_label", "figure.axis_ticks",
    "figure.put_dataset", "figure.put_evidence", "figure.update_link", "figure.open_source", "page.remove", "page.reorder", "page.component",
    "component.instantiate", "component.detach",
    "props.image_fit", "props.image_alt",
    "page.notes", "props.variant_style", "layer.move_to", "props.grid", "props.grid_span", "props.padding_sides",
    "props.align_self", "props.stroke_style", "props.gradient", "props.gradient_remove", "text.replace", "text.style_range",
    "doc.paragraph_format", "doc.page_setup", "doc.named_style", "doc.table_columns", "web.replace_source", "data.edit_cell", "add.table", "figure.style", "figure.toggle_sheet",
    "data.add_column", "data.add_row", "data.remove_row", "data.remove_column", "add.figure_image",
    "fix.grow_to_min", "fix.darken_text", "fix.grow_box", "fix.shrink_text", "fix.grow_parent", "fix.move_into_page", "text.list",
    "layer.find.next", "layer.find.previous", "canvas.open_url", "canvas.interaction.set", "canvas.interaction.remove",
    "canvas.token.put", "canvas.token.remove", "canvas.token.mode", "canvas.token.bind", "canvas.token.unbind",
    "canvas.table.add", "canvas.table.add_row", "canvas.table.remove_row", "canvas.table.add_column", "canvas.table.remove_column", "canvas.table.set_header",
    "canvas.table.set_column_widths", "table.merge_cells", "table.unmerge_cell",
];

/// A canvas with one node per lint finding: `small` (20 × 20 px), `pale` (#BBBBBB text on white), `kid` drawn
/// outside its frame `frame`, and `far` left of the page.
fn canvas_with_findings(host: &mut Host) -> String {
    use designllm_contracts::{Intent, Node, Operation};
    let canvas = host.create("canvas", "고침");
    let page = match &host.artifact(&canvas).body {
        ArtifactBody::Canvas(document) => document.pages[0].id.clone(),
        _ => panic!("a canvas"),
    };
    let node = |value: Value| -> Node { serde_json::from_value(value).expect("a node parses") };
    let px = 9525;
    let nodes = [
        node(json!({ "id": "small", "kind": "rectangle", "style": { "fill": "#2563EB" }, "bounds": { "x": 100 * px, "y": 100 * px, "width": 20 * px, "height": 20 * px } })),
        node(json!({ "id": "pale", "kind": "text", "text": "흐린 글", "style": { "color": "#BBBBBB", "font_size": 16 * designllm_contracts::EMU_PER_PT }, "bounds": { "x": 200 * px, "y": 100 * px, "width": 200 * px, "height": 40 * px } })),
        node(json!({
            "id": "frame", "kind": "rectangle", "style": { "fill": "#F3F4F6" },
            "bounds": { "x": 100 * px, "y": 300 * px, "width": 100 * px, "height": 100 * px },
            "children": [{ "id": "kid", "kind": "rectangle", "style": { "fill": "#2563EB" }, "bounds": { "x": 80 * px, "y": 80 * px, "width": 60 * px, "height": 60 * px } }],
        })),
        node(json!({ "id": "far", "kind": "rectangle", "style": { "fill": "#2563EB" }, "bounds": { "x": -50 * px, "y": 500 * px, "width": 80 * px, "height": 80 * px } })),
    ];
    let operations = nodes.into_iter().map(|node| Operation::AddNode { page_id: page.clone(), parent_id: None, node }).collect();
    host.apply(&canvas, operations, [Intent::Content, Intent::Design].into_iter().collect()).expect("the nodes go in");
    canvas
}

fn canvas_with_text_nodes(host: &mut Host, locked_second: bool) -> String {
    use designllm_contracts::{ArtifactBody, Intent, Node, Operation};
    let canvas = host.create("canvas", "목록");
    let page = match &host.artifact(&canvas).body {
        ArtifactBody::Canvas(document) => document.pages[0].id.clone(),
        _ => panic!("a canvas"),
    };
    let nodes: Vec<Node> = ["list-text-1", "list-text-2"]
        .into_iter()
        .enumerate()
        .map(|(index, id)| {
            serde_json::from_value(json!({
                "id": id,
                "kind": "text",
                "text": "",
                "bounds": { "x": 100 * 9525, "y": (100 + index as i64 * 100) * 9525, "width": 240 * 9525, "height": 80 * 9525 }
            }))
            .expect("text node")
        })
        .collect();
    let operations = nodes
        .into_iter()
        .map(|node| Operation::AddNode { page_id: page.clone(), parent_id: None, node })
        .collect();
    host.apply(&canvas, operations, [Intent::Content].into_iter().collect()).expect("empty text nodes are added");
    if locked_second {
        host.apply(
            &canvas,
            vec![Operation::SetNodeAppearance { node_ids: vec!["list-text-2".into()], image_fit: None, locked: Some(true) }],
            [Intent::Design].into_iter().collect(),
        )
        .expect("the second text node is locked before the window opens");
    }
    canvas
}

/// Reads the window makes to refresh its view; they belong to no command.
const REFRESH_READS: &[&str] = &[
    "workspace.list",
    "design_system.list",
    "artifact.inspect",
    // The design warnings of each revision the window shows (lint.rs).
    "artifact.validate",
    "context.read",
    "targets.list",
    "render.get",
    "render.hit_test",
    "render.caret_rect",
    "render.selection_rects",
];

/// The window with a recording RPC: every call it sends is stored in `calls` before the host answers it.
struct Recorder {
    app: DesktopApp,
    replies: Sender<RpcReply>,
    work: Receiver<Work>,
    ctx: Context,
    calls: Vec<String>,
    /// Answer asset reads too (page textures). Off by default: the command tests count only core calls.
    read_assets: bool,
}

impl Recorder {
    /// A window showing `canvas`, with a frame drawn so that the board knows its rectangle.
    fn open(host: &mut Host, canvas: &str) -> Self {
        let (rpc, replies, work) = RpcWorker::detached();
        let ctx = Context::default();
        theme::install(&ctx);
        let mut recorder = Self { app: DesktopApp::new(rpc, "작업공간".into(), None, None), replies, work, ctx, calls: Vec::new(), read_assets: false };
        recorder.settle(host);
        let list = host.call("workspace.list", json!({})).expect("the work list");
        let summary = list["artifacts"]
            .as_array()
            .and_then(|all| all.iter().find(|item| item["id"] == canvas))
            .cloned()
            .expect("the canvas is in the work list");
        recorder.app.activate(serde_json::from_value::<ArtifactSummary>(summary).expect("a summary"));
        recorder.settle(host);
        for _ in 0..3 {
            let _ = recorder.ctx.run(screen(1.0, 1280.0, 800.0), |ctx| {
                recorder.app.draw_frame(ctx);
            });
        }
        recorder.settle(host);
        recorder
    }

    /// Plays the host's side until the window has nothing left to ask.
    fn settle(&mut self, host: &mut Host) {
        settle_work(&self.work, &mut self.app, &self.ctx, |item| {
            if let Work::Call { ticket, method, params } = item {
                if method == "changes.apply" {
                    for operation in params["operations"].as_array().into_iter().flatten() {
                        self.calls.push(format!("changes.apply:{}", operation["op"].as_str().unwrap_or("?")));
                    }
                } else {
                    self.calls.push(method.clone());
                }
                let result = host.call(&method, params).map(RpcPayload::Json);
                self.replies.send(RpcReply { ticket, result }).expect("the window listens");
            } else if let (Work::ReadAsset { ticket, asset }, true) = (item, self.read_assets) {
                let payload = preview_pixels(host, &asset);
                self.replies.send(RpcReply { ticket, result: Ok(payload) }).expect("the window listens");
            }
        });
    }

    /// Runs one command as a setup step, outside the measured calls.
    fn step(&mut self, host: &mut Host, id: &str, args: Value) -> Result<(), String> {
        let result = self.app.invoke(id, args);
        self.settle(host);
        result
    }

    /// The id of the first top-level node on the page that has children (a group).
    fn top_group_id(&self) -> Option<String> {
        let artifact = self.app.artifact.as_ref()?;
        let ArtifactBody::Canvas(document) = &artifact.body else {
            return None;
        };
        document.pages[0].nodes.iter().find(|node| !node.children.is_empty()).map(|node| node.id.clone())
    }
}

/// The arguments a command runs with: each required field gets the first value its schema allows, and the
/// commands whose arms need particular values get those.
fn arguments(spec: &CommandSpec, canvas: &str) -> Value {
    let schema: Value = serde_json::from_str(spec.args).expect("the argument schema is JSON");
    let mut args = serde_json::Map::new();
    for key in schema["required"].as_array().into_iter().flatten().filter_map(Value::as_str) {
        let property = &schema["properties"][key];
        let value = match (property["enum"].get(0), property["type"].as_str()) {
            (Some(first), _) => first.clone(),
            (None, Some("number" | "integer")) => json!(1),
            (None, Some("array")) => json!([]),
            (None, Some("boolean")) => json!(true),
            (None, _) => json!("x"),
        };
        args.insert(key.to_owned(), value);
    }
    match spec.id {
        "select.node" => json!({ "node_id": "a" }),
        "select.many" => json!({ "node_ids": ["a", "b", "c"] }),
        "layer.move" => json!({ "node_id": "a", "index": 1 }),
        "nav.page" | "view.zoom_page" => json!({ "page_index": 0 }),
        // The schema asks for an intent or a mode (no key is required by itself); the segment that keeps the defaults.
        "ai.toggle_intent" => json!({ "mode": "both" }),
        "nav.open_project" => json!({ "artifact_id": canvas }),
        "nav.open_file" => json!({ "path": super::test_support::repository_root().join("fixtures/office/basic-preserve.docx") }),
        "edit.rename" => json!({ "name": "새 이름" }),
        "props.position" => json!({ "x": 2, "y": 2 }),
        "props.size" => json!({ "width": 2, "height": 2 }),
        "props.fill" | "props.stroke" => json!({ "color": "#1E7F4F" }),
        "props.line_height" => json!({ "value": 1.5 }),
        "add.path" => json!({ "d": "M 0 0 L 24 24", "view_box": "0 0 24 24" }),
        _ => Value::Object(args),
    }
}

/// Runs one command on a fresh canvas and returns the calls it sent, with the refusal if it refused.
fn run_command(host: &mut Host, spec: &CommandSpec) -> (Vec<String>, Result<(), String>) {
    let canvas = canvas_with_three_boxes(host, [0, 1_000_000, 3_000_000]);
    let mut recorder = Recorder::open(host, &canvas);
    let (selection, undo_first): (Vec<&str>, bool) = match spec.id {
        "edit.undo" => (vec!["a"], false),
        "edit.redo" => (vec!["a"], true),
        _ => (vec!["a", "b", "c"], false),
    };
    recorder.step(host, "select.many", json!({ "node_ids": selection })).expect("the boxes are selected");
    if spec.id == "edit.undo" || spec.id == "edit.redo" {
        recorder.step(host, "edit.rename", json!({ "name": "바뀜" })).expect("a change to undo");
    }
    if undo_first {
        recorder.step(host, "edit.undo", json!({})).expect("the change is undone first");
    }
    if spec.id == "edit.ungroup" || spec.id.starts_with("props.layout_") {
        recorder.step(host, "edit.group", json!({})).expect("the boxes group");
        let group = recorder.top_group_id().expect("the group is on the page");
        recorder.step(host, "select.node", json!({ "node_id": group })).expect("the group is selected");
    }
    if spec.id == "edit.paste" {
        recorder.step(host, "edit.copy", json!({})).expect("the nodes are copied");
    }
    if spec.id == "edit.paste_properties" {
        recorder.step(host, "edit.copy_properties", json!({})).expect("the style is copied");
    }
    // A clear that changes nothing sends no change set, so the page gets a guide to clear first.
    if spec.id == "canvas.guides.clear" {
        recorder.step(host, "canvas.guides.set", json!({ "axis": "columns", "count": 1, "spacing_emu": 0, "leading_margin_emu": 0, "trailing_margin_emu": 0 })).expect("a guide to clear");
    }
    let mark = recorder.calls.len();
    let result = recorder.app.invoke(spec.id, arguments(spec, &canvas));
    recorder.settle(host);
    (recorder.calls[mark..].to_vec(), result)
}

/// The calls without repeats, in the order they were first sent, and without the refresh reads.
fn distinct_in_order(calls: &[String]) -> Vec<String> {
    let mut distinct: Vec<String> = Vec::new();
    for call in calls {
        if !REFRESH_READS.contains(&call.as_str()) && !distinct.contains(call) {
            distinct.push(call.clone());
        }
    }
    distinct
}

#[test]
fn every_wired_command_declares_its_core_calls() {
    let missing: Vec<&str> = commands::COMMANDS
        .iter()
        .filter(|spec| commands::available(spec.id) && commands::core_calls(spec.id).is_none())
        .map(|spec| spec.id)
        .collect();
    assert!(missing.is_empty(), "no core calls declared for: {missing:?}");
}

#[test]
fn every_wired_command_sends_the_core_calls_it_declares() {
    let mut host = Host::start();
    let mut problems: Vec<String> = Vec::new();
    let mut ran = 0;
    for spec in commands::COMMANDS.iter().filter(|spec| commands::available(spec.id)) {
        if NOT_RUN.iter().any(|(id, _)| *id == spec.id) || KIND_RUN.contains(&spec.id) {
            continue;
        }
        let (sent, result) = run_command(&mut host, spec);
        ran += 1;
        if let Err(message) = result {
            problems.push(format!("{}: refused: {message}", spec.id));
        }
        let sent = distinct_in_order(&sent);
        let sent_refs: Vec<&str> = sent.iter().map(String::as_str).collect();
        match commands::core_calls(spec.id) {
            None => problems.push(format!("{}: no calls declared; sent {sent_refs:?}", spec.id)),
            Some(declared) => {
                let expected: Vec<&str> = declared
                    .iter()
                    .copied()
                    .filter(|call| !matches!(spec.id, "edit.duplicate" | "edit.paste") || *call != "changes.apply:update_figure_link")
                    .collect();
                if expected != sent_refs {
                    problems.push(format!("{}: declared {expected:?}, sent {sent_refs:?}", spec.id));
                }
            }
        }
    }
    assert!(ran >= 60, "only {ran} commands ran");
    assert!(problems.is_empty(), "{} commands differ:\n{}", problems.len(), problems.join("\n"));
}

/// A figure with a diagram panel of two boxes and a line chart over a four-row table.
fn figure_with_diagram_and_chart(host: &mut Host) -> String {
    let figure_id = host.create("figure", "그림");
    let mut draft = host.declared_draft("id,value\nA,12\nB,18\nC,25\nD,31\n", |_| {});
    draft.y_unit = "%".into();
    let (figure, _) = host.figure(&figure_id);
    let (operations, intents) = super::figure_tools::chart_change(&figure, &draft, true).expect("a chart");
    host.apply(&figure_id, operations, intents).expect("the chart panel");
    // The diagram comes second, so its boxes are placed in the panel's final (tiled) size.
    let (figure, _) = host.figure(&figure_id);
    let diagram = super::figure_tools::new_diagram_panel(&figure);
    host.apply(
        &figure_id,
        super::figure_tools::panel_additions(&figure, diagram),
        [designllm_contracts::Intent::Content, designllm_contracts::Intent::Design].into_iter().collect(),
    )
    .expect("the diagram panel");
    for _ in 0..2 {
        let (figure, _) = host.figure(&figure_id);
        let panel = figure.panels.iter().find(|panel| matches!(panel.body, designllm_contracts::PanelBody::Diagram(_))).expect("diagram").id.clone();
        let operation = super::figure_tools::node_addition(&figure, &panel, designllm_contracts::NodeKind::Rectangle).expect("room");
        host.apply(&figure_id, vec![operation], [designllm_contracts::Intent::Content].into_iter().collect()).expect("a box");
    }
    figure_id
}

/// A Word document with two paragraphs and a two-column table.
fn document_with_paragraphs_and_table(host: &mut Host) -> String {
    use designllm_contracts::{FlowBlock, Intent, Operation, Table, TableCell};
    let document = host.create("docx", "문서");
    let section = match &host.artifact(&document).body {
        ArtifactBody::Flow(flow) => flow.sections[0].id.clone(),
        _ => panic!("a flowing document"),
    };
    let cell = || TableCell { id: designllm_contracts::new_id(), blocks: vec![super::link_flow_tests::paragraph("칸")], column_span: 1, row_span: 1 };
    let table = FlowBlock::Table(Table {
        id: designllm_contracts::new_id(),
        column_widths: vec![60 * designllm_contracts::EMU_PER_MM, 60 * designllm_contracts::EMU_PER_MM],
        rows: vec![vec![cell(), cell()]],
        style: Default::default(),
    });
    let blocks = [super::link_flow_tests::paragraph("첫째 문단"), super::link_flow_tests::paragraph("둘째 문단"), table];
    for block in blocks {
        let index = match &host.artifact(&document).body {
            ArtifactBody::Flow(flow) => flow.sections[0].blocks.len() as u32,
            _ => unreachable!(),
        };
        host.apply(&document, vec![Operation::InsertFlowBlock { section_id: section.clone(), index, block }], [Intent::Content].into_iter().collect())
            .expect("a block");
    }
    document
}

fn figure_of(recorder: &Recorder) -> designllm_contracts::FigureDocument {
    recorder.app.figure_document().expect("a figure is open")
}

fn panel_id(recorder: &Recorder, plot: bool) -> String {
    figure_of(recorder)
        .panels
        .iter()
        .find(|panel| matches!(panel.body, designllm_contracts::PanelBody::Plot(_)) == plot)
        .map(|panel| panel.id.clone())
        .expect("the panel")
}

fn diagram_nodes(recorder: &Recorder) -> Vec<String> {
    figure_of(recorder)
        .panels
        .iter()
        .find_map(|panel| match &panel.body {
            designllm_contracts::PanelBody::Diagram(diagram) => Some(diagram.nodes.iter().map(|node| node.id.clone()).collect()),
            _ => None,
        })
        .expect("a diagram")
}

/// The paragraph whose text is `text`.
fn paragraph_id(recorder: &Recorder, text: &str) -> String {
    let ArtifactBody::Flow(flow) = &recorder.app.artifact.as_ref().expect("open").body else { panic!("a flow document") };
    flow.sections[0]
        .blocks
        .iter()
        .find_map(|block| match block {
            designllm_contracts::FlowBlock::Paragraph(paragraph) if paragraph.runs.iter().map(|run| run.text.as_str()).collect::<String>() == text => {
                Some(paragraph.id.clone())
            }
            _ => None,
        })
        .expect("the paragraph")
}

/// Opens the document a kind command needs, prepares the selection and returns the arguments it runs with.
fn prepare_kind(host: &mut Host, id: &str) -> (Recorder, Value) {
    if matches!(id, "component.instantiate" | "component.detach") {
        use designllm_contracts::{ArtifactBody, Intent, Node, NodeKind, Operation, Page, PageRole, Size, Style};
        let canvas = host.create("canvas", "재사용 컴포넌트");
        let target_page = match &host.artifact(&canvas).body {
            ArtifactBody::Canvas(document) => document.pages[0].id.clone(),
            _ => panic!("Canvas artifact"),
        };
        let source_node = Node {
            id: "component-text".into(), kind: NodeKind::Text,
            bounds: designllm_contracts::Rect { x: 10, y: 10, width: 900, height: 300 }, text: "Reusable".into(),
            style: Style::default(), asset: None, children: vec![], layout: None, interactions: vec![], variants: Default::default(), ext: Default::default(),
        };
        host.apply(&canvas, vec![Operation::InsertPage { index: 1, page: Page {
            nodes: vec![source_node], name: Some("Card".into()), role: Some(PageRole::Component), ..Page::new("component-source", Size::slide())
        }}],[Intent::Content].into_iter().collect()).expect("the source page is registered");
        if id == "component.detach" {
            host.apply(&canvas, vec![Operation::InstantiateComponent {
                page_id: target_page, parent_id: None, instance_id: "instance".into(), component_page_id: "component-source".into(),
                bounds: designllm_contracts::Rect { x: 200, y: 200, width: 2000, height: 1000 },
            }], [Intent::Content].into_iter().collect()).expect("the component instance is created");
        }
        let recorder = Recorder::open(host, &canvas);
        let args = if id == "component.detach" {
            json!({ "node_id": "instance" })
        } else {
            json!({ "component_page_id": "component-source", "instance_id": "command-instance", "bounds": { "x": 200, "y": 200, "width": 2000, "height": 1000 } })
        };
        return (recorder, args);
    }
    if id == "page.component" {
        let canvas = canvas_with_three_boxes(host, [0, 1_000_000, 3_000_000]);
        return (Recorder::open(host, &canvas), json!({ "page_index": 0, "enabled": true }));
    }

    if id.starts_with("canvas.table.") {
        let canvas = canvas_with_three_boxes(host, [0, 1_000_000, 3_000_000]);
        let mut recorder = Recorder::open(host, &canvas);
        if id == "canvas.table.add" {
            return (recorder, json!({ "rows": 2, "columns": 2 }));
        }
        recorder.step(host, "canvas.table.add", json!({ "rows": 2, "columns": 2 })).expect("a Canvas table is ready");
        let table_id = recorder.app.selected_node.clone().expect("the new table is selected");
        let args = match id {
            "canvas.table.add_row" => json!({ "table_id": table_id, "index": 1 }),
            "canvas.table.remove_row" => json!({ "table_id": table_id, "index": 1 }),
            "canvas.table.add_column" => json!({ "table_id": table_id, "index": 2 }),
            "canvas.table.remove_column" => json!({ "table_id": table_id, "index": 1 }),
            "canvas.table.set_header" => json!({ "table_id": table_id, "enabled": false }),
            "canvas.table.set_column_widths" => json!({ "table_id": table_id, "column_widths": [1, 2] }),
            _ => unreachable!(),
        };
        return (recorder, args);
    }

    if id.starts_with("table.") {
        let canvas = canvas_with_three_boxes(host, [0, 1_000_000, 3_000_000]);
        let mut recorder = Recorder::open(host, &canvas);
        recorder.step(host, "canvas.table.add", json!({ "rows": 2, "columns": 2 })).expect("a Canvas table is ready");
        let table_id = recorder.app.selected_node.clone().expect("the new table is selected");
        recorder.step(host, "canvas.table.set_header", json!({ "table_id": table_id, "enabled": false })).expect("the header is off");
        let merge = json!({ "table_id": table_id, "row": 1, "column": 0, "rows": 1, "columns": 2 });
        if id == "table.merge_cells" {
            return (recorder, merge);
        }
        recorder.step(host, "table.merge_cells", merge).expect("a merged region to unmerge");
        return (recorder, json!({ "table_id": table_id, "row": 1, "column": 1 }));
    }

    if id.starts_with("canvas.token.") {
        use designllm_contracts::{BindTarget, Intent, Operation, TokenRef, TokenSource, TokenValues};
        let canvas = canvas_with_three_boxes(host, [0, 1_000_000, 3_000_000]);
        host.apply(
            &canvas,
            vec![Operation::PutToken {
                token_id: "brand.ink".into(),
                name: "브랜드 잉크".into(),
                values: TokenValues::Color { light: "#111111".into(), dark: "#F7F8FA".into() },
            }],
            [Intent::Design].into_iter().collect(),
        )
        .expect("the local token is ready");
        if id == "canvas.token.unbind" {
            host.apply(
                &canvas,
                vec![Operation::BindToken {
                    node_ids: vec!["a".into(), "b".into()],
                    target: BindTarget::StyleFill,
                    token: TokenRef { source: TokenSource::Local, token_id: "brand.ink".into() },
                }],
                [Intent::Design].into_iter().collect(),
            )
            .expect("the binding is ready to remove");
        }
        let mut recorder = Recorder::open(host, &canvas);
        if matches!(id, "canvas.token.bind" | "canvas.token.unbind") {
            recorder.step(host, "select.many", json!({ "node_ids": ["a", "b"] })).expect("two nodes selected");
        }
        let args = match id {
            "canvas.token.put" => json!({
                "token_id": "brand.ink", "name": "Brand ink",
                "values": { "kind": "color", "light": "#222222", "dark": "#F8F8F8" }
            }),
            "canvas.token.remove" => json!({ "token_id": "brand.ink" }),
            "canvas.token.mode" => json!({ "mode": "dark" }),
            "canvas.token.bind" => json!({ "token_id": "brand.ink", "target": "style_fill" }),
            "canvas.token.unbind" => json!({ "target": "style_fill" }),
            _ => unreachable!(),
        };
        return (recorder, args);
    }
    if id == "canvas.open_url" {
        let canvas = canvas_with_three_boxes(host, [0, 1_000_000, 3_000_000]);
        let mut recorder = Recorder::open(host, &canvas);
        recorder.step(host, "select.many", json!({ "node_ids": ["a", "b"] })).expect("both nodes are selected");
        return (recorder, json!({ "url": "https://example.test/" }));
    }
    if matches!(id, "canvas.interaction.set" | "canvas.interaction.remove") {
        use designllm_contracts::{Interaction, Intent, Operation};
        let canvas = canvas_with_three_boxes(host, [0, 1_000_000, 3_000_000]);
        host.apply(
            &canvas,
            vec![Operation::SetInteractions { node_id: "a".into(), interactions: vec![Interaction::Back] }],
            [Intent::Content].into_iter().collect(),
        )
        .expect("a Back action is stored");
        let mut recorder = Recorder::open(host, &canvas);
        recorder.step(host, "select.node", json!({ "node_id": "a" })).expect("one node selected");
        let args = if id == "canvas.interaction.set" {
            json!({ "index": 0, "action": "toggle_visibility", "target_id": "b" })
        } else {
            json!({ "index": 0 })
        };
        return (recorder, args);
    }
    if matches!(id, "layer.find.next" | "layer.find.previous") {
        let document = document_with_paragraphs_and_table(host);
        let mut recorder = Recorder::open(host, &document);
        recorder.app.layer_find_query = "첫째".into();
        recorder.app.sync_flow_find_search();
        return (recorder, json!({}));
    }
    if id == "text.list" {
        let canvas = canvas_with_text_nodes(host, false);
        let mut recorder = Recorder::open(host, &canvas);
        recorder
            .step(host, "select.many", json!({ "node_ids": ["list-text-1", "list-text-2"] }))
            .expect("both empty text nodes are selected");
        return (recorder, json!({ "list": "bullet" }));
    }
    let figure_commands = id.starts_with("figure.") && !matches!(id, "figure.update_link" | "figure.open_source")
        || matches!(id, "add.figure_panel" | "add.diagram_node" | "add.connector" | "add.figure_image")
        || id.starts_with("data.");
    if figure_commands {
        // A third panel would squeeze the chart until its labels collide: a new panel goes next to one diagram.
        let figure = if matches!(id, "add.figure_panel" | "add.figure_image") {
            super::link_flow_tests::drawn_figure(host, "그림", 0)
        } else {
            figure_with_diagram_and_chart(host)
        };
        let mut recorder = Recorder::open(host, &figure);
        let args = match id {
            "add.figure_image" => json!({ "path": test_png(64, 48).to_string_lossy(), "origin": "experimental_original" }),
            "data.add_column" => json!({ "dataset_id": figure_of(&recorder).datasets[0].id, "name": "반복" }),
            "data.add_row" => json!({ "dataset_id": figure_of(&recorder).datasets[0].id }),
            "data.remove_row" => {
                let dataset = &figure_of(&recorder).datasets[0];
                json!({ "dataset_id": dataset.id, "row_id": dataset.rows[0].id })
            }
            "data.remove_column" => {
                // A column no chart draws: added first.
                let dataset_id = figure_of(&recorder).datasets[0].id.clone();
                recorder.step(host, "data.add_column", json!({ "dataset_id": dataset_id, "name": "메모", "data_type": "text" })).expect("a column");
                let dataset = figure_of(&recorder).datasets.last().cloned().expect("the dataset");
                let column = dataset.columns.iter().find(|column| column.name == "메모").expect("the new column").id.clone();
                json!({ "dataset_id": dataset.id, "column_id": column })
            }
            "add.diagram_node" => json!({ "kind": "oval" }),
            "figure.add_equation" => json!({ "source": "x^2" }),
            "figure.style" => json!({ "target_id": diagram_nodes(&recorder)[0], "fill": "#E8EEFC", "stroke_width": 1 }),
            "data.edit_cell" => {
                let dataset = &figure_of(&recorder).datasets[0];
                let column = dataset.columns.iter().find(|column| column.data_type == "numeric").expect("a numeric column");
                json!({ "dataset_id": dataset.id, "row_id": dataset.rows[0].id, "column_id": column.id, "value": "13" })
            }
            "add.connector" => {
                let nodes = diagram_nodes(&recorder);
                json!({ "from": nodes[0], "to": nodes[1] })
            }
            "figure.remove_panel" => json!({ "panel_id": panel_id(&recorder, false) }),
            "figure.reorder_panels" => json!({ "panel_id": panel_id(&recorder, true), "index": 0 }),
            "figure.caption" => json!({ "caption": "그림 1. 농도별 생존율" }),
            "figure.panel_label" => json!({ "panel_id": panel_id(&recorder, false), "label": "(가)" }),
            "figure.plot" => json!({ "panel_id": panel_id(&recorder, true), "y_minimum": 0, "y_maximum": 40, "error_kind": "none" }),
            "figure.axis_label" => json!({ "panel_id": panel_id(&recorder, true), "axis": "x", "label": "시료" }),
            "figure.axis_ticks" => json!({ "panel_id": panel_id(&recorder, true), "axis": "y", "tick_step": 5 }),
            "figure.put_dataset" => {
                let mut other = host.declared_draft("id,value\nE,4\nF,7\n", |_| {});
                other.y_unit = "%".into();
                json!({ "dataset": other.dataset })
            }
            "figure.put_evidence" => json!({ "text": "세 번 반복 측정했다" }),
            _ => json!({}),
        };
        return (recorder, args);
    }
    if matches!(id, "figure.update_link" | "figure.open_source" | "props.image_fit" | "props.image_alt") {
        let figure = super::link_flow_tests::drawn_figure(host, "그림", 1);
        let canvas = canvas_with_three_boxes(host, [0, 1_000_000, 3_000_000]);
        let mut recorder = Recorder::open(host, &canvas);
        recorder.step(host, "add.figure_link", json!({ "figure_id": figure })).expect("the picture goes in");
        let picture = recorder.app.selected_node.clone().expect("the picture is selected");
        if id == "figure.update_link" {
            super::link_flow_tests::add_box(host, &figure);
            recorder.app.call("workspace.list", json!({}), super::PendingAction::WorkspaceList);
            recorder.settle(host);
        }
        let args = match id {
            "props.image_fit" => json!({ "fit": "cover" }),
            "props.image_alt" => json!({ "alt": "장면 설명" }),
            _ => json!({ "node_id": picture }),
        };
        return (recorder, args);
    }
    if id.starts_with("doc.") || id.starts_with("text.") || id == "add.table" {
        let document = document_with_paragraphs_and_table(host);
        let mut recorder = Recorder::open(host, &document);
        let first = paragraph_id(&recorder, "첫째 문단");
        let args = match id {
            "text.replace" => json!({ "paragraph_id": first, "start": 0, "end": 2, "text": "처음" }),
            "text.style_range" => json!({ "paragraph_id": first, "start": 0, "end": 2, "bold": true }),
            "doc.paragraph_format" => json!({ "paragraph_id": first, "align": "center", "space_after": 12 }),
            "doc.page_setup" => json!({ "margin_left": 25 }),
            "doc.named_style" => {
                let ArtifactBody::Flow(flow) = &recorder.app.artifact.as_ref().expect("open").body else { unreachable!() };
                let name = flow.styles.keys().next().cloned().unwrap_or_else(|| "Normal".into());
                json!({ "name": name, "bold": true })
            }
            "doc.table_columns" => json!({ "column_widths": [50, 70] }),
            _ => json!({}),
        };
        recorder.app.select_paragraph(first, "첫째 문단");
        recorder.settle(host);
        return (recorder, args);
    }
    if id == "web.replace_source" {
        let web = host.create("web_code", "웹");
        let recorder = Recorder::open(host, &web);
        return (recorder, json!({ "content": "<!doctype html><title>새 화면</title>" }));
    }
    if id.starts_with("fix.") {
        let canvas = canvas_with_findings(host);
        let mut recorder = Recorder::open(host, &canvas);
        let target = match id {
            "fix.grow_to_min" => "small",
            "fix.darken_text" | "fix.grow_box" | "fix.shrink_text" => "pale",
            "fix.grow_parent" => "kid",
            _ => "far",
        };
        recorder.step(host, "select.node", json!({ "node_id": target })).expect("the node is selected");
        return (recorder, json!({ "node_id": target }));
    }
    if id == "page.notes" {
        let slides = host.create("slides", "발표");
        let recorder = Recorder::open(host, &slides);
        return (recorder, json!({ "notes": "세 번 반복했습니다" }));
    }
    let canvas = canvas_with_three_boxes(host, [0, 1_000_000, 3_000_000]);
    let mut recorder = Recorder::open(host, &canvas);
    let args = match id {
        "page.remove" | "page.reorder" => {
            recorder.step(host, "add.page", json!({})).expect("a second page");
            if id == "page.remove" { json!({ "page_index": 1 }) } else { json!({ "page_index": 1, "to_index": 0 }) }
        }
        "props.variant_style" => {
            recorder.step(host, "select.node", json!({ "node_id": "a" })).expect("selected");
            json!({ "variant": "hover", "fill": "#1E7F4F" })
        }
        "layer.move_to" | "props.grid" | "props.padding_sides" | "props.grid_span" | "props.align_self" => {
            recorder.step(host, "select.many", json!({ "node_ids": ["a", "b", "c"] })).expect("selected");
            recorder.step(host, "edit.stack", json!({})).expect("the boxes stack");
            let group = recorder.top_group_id().expect("the stack frame");
            recorder.step(host, "select.node", json!({ "node_id": group })).expect("the frame is selected");
            match id {
                "layer.move_to" => json!({ "node_id": "a", "index": 0 }),
                "props.grid" => json!({ "columns": 2, "row_gap": 8 }),
                "props.padding_sides" => json!({ "top": 4, "right": 8, "bottom": 4, "left": 8 }),
                "props.grid_span" => {
                    recorder.step(host, "props.grid", json!({ "columns": 2 })).expect("a grid");
                    json!({ "node_id": "a", "span": 2 })
                }
                _ => json!({ "node_id": "a", "align": "center" }),
            }
        }
        "props.stroke_style" | "props.gradient" | "props.gradient_remove" => {
            recorder.step(host, "add.rect", json!({})).expect("a rectangle");
            let rectangle = match &recorder.app.artifact.as_ref().expect("open").body {
                ArtifactBody::Canvas(document) => document.pages[0].nodes.last().expect("the rectangle").id.clone(),
                _ => unreachable!(),
            };
            recorder.step(host, "select.node", json!({ "node_id": rectangle })).expect("selected");
            // Removing a gradient needs one on the rectangle first; this preparation is not counted in the calls.
            if id == "props.gradient_remove" {
                recorder.step(host, "props.gradient", json!({ "from": "#3D73F4", "to": "#2350D1", "angle": 180 })).expect("a gradient");
            }
            match id {
                "props.stroke_style" => json!({ "dash": "dashed", "cap": "round" }),
                "props.gradient" => json!({ "from": "#3D73F4", "to": "#2350D1", "angle": 180 }),
                _ => json!({}),
            }
        }
        _ => json!({}),
    };
    (recorder, args)
}

/// In the window (page texture from the host), the outlines of stacked boxes are where the engine drew them: three
/// frames side by side, none at the stack's corner.
#[test]
fn the_window_outlines_stack_children_where_they_are_drawn() {
    let mut host = Host::start();
    let canvas = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
    let mut recorder = Recorder::open(&mut host, &canvas);
    recorder.read_assets = true;
    recorder.step(&mut host, "select.many", json!({ "node_ids": ["a", "b", "c"] })).expect("selected");
    recorder.step(&mut host, "edit.stack", json!({})).expect("the boxes stack");
    for _ in 0..4 {
        let _ = recorder.ctx.run(screen(1.0, 1280.0, 800.0), |ctx| {
            recorder.app.draw_frame(ctx);
        });
        recorder.settle(&mut host);
    }
    let texture = recorder.app.page_textures.get(&0).expect("the page texture");
    let drawn = texture.frames.clone();
    assert!(["a", "b", "c"].iter().all(|id| drawn.contains_key(*id)), "the scene draws the children: {:?}", texture.page.source_map);
    // The board asks for the engine's own frames (`include_node_frames`): the window's frame is the engine's, end to end.
    let revision = host.artifact(&canvas).revision;
    let engine: std::collections::BTreeMap<String, designllm_contracts::Rect> = host
        .call("render.get", json!({ "artifact_id": canvas, "expected_revision": revision, "page_index": 0, "profile": { "dpi": 96, "transparent": false }, "include_node_frames": true }))
        .ok()
        .and_then(|value| serde_json::from_value(value["scene"]["metadata"]["node_frames"].clone()).ok())
        .expect("render.get returns the engine's node frames");
    for id in ["a", "b", "c"] {
        assert_eq!(drawn[id], engine[id], "the window uses the engine's frame of {id}");
    }
    let board = recorder.app.board_rect.expect("the board");
    let (pan, zoom) = (recorder.app.view_ref().pan, recorder.app.view_ref().zoom);
    let frames = recorder.app.node_frames(board.min + pan, zoom);
    let frame = |id: &str| frames.iter().find(|(found, _)| found == id).expect("a frame").1;
    let (a, b, c) = (frame("a"), frame("b"), frame("c"));
    assert!(a.right() <= b.left() + 0.5 && b.right() <= c.left() + 0.5, "side by side: {a:?} {b:?} {c:?} drawn {drawn:?}");
}

/// A v1.2 canvas (a node named `button` makes it one) whose `note` has pale text in a narrow box (clipped, low
/// contrast) and whose `button` breaks five rules at once (small, clipped, pale, outside its frame and the page).
pub(super) fn canvas_with_lint_findings(host: &mut Host) -> String {
    use designllm_contracts::{Intent, Node, Operation};
    let canvas = host.create("canvas", "디자인 경고");
    let page = match &host.artifact(&canvas).body {
        ArtifactBody::Canvas(document) => document.pages[0].id.clone(),
        _ => panic!("a canvas"),
    };
    let px = 9525;
    let node = |value: Value| -> Node { serde_json::from_value(value).expect("a node parses") };
    let pale = json!({ "color": "#BBBBBB", "fill": "#FFFFFF", "font_size": 16 * designllm_contracts::EMU_PER_PT });
    let nodes = [
        node(json!({ "id": "note", "kind": "text", "text": "실험 결과를 정리한 메모가 이 상자보다 깁니다", "style": pale, "bounds": { "x": 100 * px, "y": 100 * px, "width": 120 * px, "height": 24 * px } })),
        node(json!({
            "id": "frame", "kind": "rectangle", "style": { "fill": "#F3F4F6" },
            "bounds": { "x": 4 * px, "y": 300 * px, "width": 100 * px, "height": 60 * px },
            "children": [{ "id": "button", "kind": "rectangle", "text": "아주 긴 확인 단추 글", "style": pale, "name": "button",
                "bounds": { "x": -20 * px, "y": 50 * px, "width": 20 * px, "height": 20 * px } }],
        })),
    ];
    let operations = nodes.into_iter().map(|node| Operation::AddNode { page_id: page.clone(), parent_id: None, node }).collect();
    host.apply(&canvas, operations, [Intent::Content, Intent::Design].into_iter().collect()).expect("the nodes go in");
    canvas
}

/// The window shows the engine's design warnings of the revision it shows, without refusing anything.
#[test]
fn the_window_keeps_the_design_warnings_of_the_revision_it_shows() {
    let mut host = Host::start();
    let canvas = canvas_with_lint_findings(&mut host);
    let recorder = Recorder::open(&mut host, &canvas);
    let codes = |id: &str| -> Vec<String> {
        recorder.app.lint_warnings.iter().filter(|warning| warning.node_id.as_deref() == Some(id)).map(|warning| warning.code.clone()).collect()
    };
    let note = codes("note");
    assert!(note.iter().any(|code| code.contains("CLIPPED")) && note.iter().any(|code| code.contains("CONTRAST")), "note: {note:?}");
    let button = codes("button");
    assert!(button.len() >= 4, "button: {button:?} (all: {:?})", recorder.app.lint_warnings);
    assert!(recorder.app.lint_warnings.iter().all(|warning| !warning.blocking));
    // The frame breaks no rule itself (its child does): its open layer row shows no warning icon.
    assert_eq!(codes("frame"), Vec::<String>::new(), "frame: {:?}", recorder.app.lint_warnings);
}

/// A `width` × `height` PNG in a fresh scratch folder.
fn test_png(width: u32, height: u32) -> std::path::PathBuf {
    let folder = std::env::temp_dir().join(format!("designllm-desktop-png-{}", designllm_contracts::new_id()));    std::fs::create_dir_all(&folder).expect("a scratch folder");
    let path = folder.join("picture.png");
    image::RgbaImage::from_pixel(width, height, image::Rgba([30, 120, 200, 255])).save(&path).expect("the PNG is written");
    path
}

/// The picture becomes an image panel that knows its pixel size and where it comes from.
#[test]
fn a_picture_becomes_an_image_panel_with_its_pixel_size() {
    let mut host = Host::start();
    // A third panel would squeeze the chart until its labels collide: the picture goes next to one diagram.
    let figure = super::link_flow_tests::drawn_figure(&mut host, "그림", 0);
    let mut recorder = Recorder::open(&mut host, &figure);
    let before = figure_of(&recorder).panels.len();
    recorder
        .step(&mut host, "add.figure_image", json!({ "path": test_png(64, 48).to_string_lossy(), "origin": "generated_illustration" }))
        .expect("the picture goes in");
    let panels = figure_of(&recorder).panels;
    assert_eq!(panels.len(), before + 1);
    let designllm_contracts::PanelBody::Image(image) = &panels.last().expect("the new panel").body else {
        panic!("an image panel");
    };
    assert_eq!((image.pixel_width, image.pixel_height, image.origin.as_str()), (64, 48, "generated_illustration"));
    assert!(recorder.step(&mut host, "add.figure_image", json!({ "path": "x.png", "origin": "made_up" })).is_err(), "an unknown origin is refused");
    assert!(
        recorder.step(&mut host, "add.figure_image", json!({ "path": "x.png", "origin": "paper_extracted" })).is_err(),
        "a picture from a paper without its evidence is refused"
    );
}

/// Rows and columns go in and out of the sheet's table; a column a chart draws is refused before the host is asked.
#[test]
fn the_data_sheet_adds_and_removes_rows_and_columns() {
    let mut host = Host::start();
    let figure = figure_with_diagram_and_chart(&mut host);
    let mut recorder = Recorder::open(&mut host, &figure);
    let dataset = |recorder: &Recorder| figure_of(recorder).datasets.last().cloned().expect("a dataset");
    let rows = dataset(&recorder).rows.len();
    recorder.step(&mut host, "data.add_row", json!({ "dataset_id": dataset(&recorder).id, "count": 2 })).expect("rows go in");
    assert_eq!(dataset(&recorder).rows.len(), rows + 2);
    let first = dataset(&recorder).rows[0].id.clone();
    recorder.step(&mut host, "data.remove_row", json!({ "dataset_id": dataset(&recorder).id, "row_id": first })).expect("a row goes");
    assert_eq!(dataset(&recorder).rows.len(), rows + 1);
    let columns = dataset(&recorder).columns.len();
    recorder.step(&mut host, "data.add_column", json!({ "dataset_id": dataset(&recorder).id, "name": "반복", "data_type": "count" })).expect("a column");
    assert_eq!(dataset(&recorder).columns.len(), columns + 1);
    let plotted = figure_of(&recorder)
        .panels
        .iter()
        .find_map(|panel| match &panel.body {
            designllm_contracts::PanelBody::Plot(plot) => Some(plot.x_column.clone()),
            _ => None,
        })
        .expect("a chart");
    let mark = recorder.calls.len();
    let refused = recorder.step(&mut host, "data.remove_column", json!({ "dataset_id": dataset(&recorder).id, "column_id": plotted }));
    assert!(refused.is_err(), "a plotted column is refused");
    assert!(!recorder.calls[mark..].iter().any(|call| call == "figure.dataset.edit"), "the host is not asked");
}

/// Each lint fix changes what its finding names, in one change set.
#[test]
fn each_fix_repairs_the_finding_it_names() {
    let mut host = Host::start();
    let px = 9525;
    let node_after = |recorder: &Recorder, id: &str| super::doc_tree::find_node(recorder.app.artifact.as_ref().expect("open"), id).expect("the node").clone();
    let run = |host: &mut Host, id: &str, target: &str| -> Recorder {
        let canvas = canvas_with_findings(host);
        let mut recorder = Recorder::open(host, &canvas);
        recorder.step(host, id, json!({ "node_id": target })).unwrap_or_else(|error| panic!("{id}: {error}"));
        recorder
    };
    let recorder = run(&mut host, "fix.grow_to_min", "small");
    let small = node_after(&recorder, "small").bounds;
    assert_eq!((small.width, small.height), (44 * px, 44 * px), "{small:?}");
    assert_eq!(small.x + small.width / 2, 110 * px, "it grows around its centre");

    let recorder = run(&mut host, "fix.darken_text", "pale");
    let color = node_after(&recorder, "pale").style.color;
    assert!(super::fix_commands::contrast(&color, "#FFFFFF").expect("a ratio") >= 4.5, "{color}");

    let recorder = run(&mut host, "fix.shrink_text", "pale");
    assert!(node_after(&recorder, "pale").style.font_size < 16 * designllm_contracts::EMU_PER_PT);

    let recorder = run(&mut host, "fix.grow_box", "pale");
    assert!(node_after(&recorder, "pale").bounds.height > 40 * px);

    let recorder = run(&mut host, "fix.grow_parent", "kid");
    let frame = node_after(&recorder, "frame").bounds;
    assert!(frame.width >= 140 * px && frame.height >= 140 * px, "the frame holds the kid: {frame:?}");

    let recorder = run(&mut host, "fix.move_into_page", "far");
    assert_eq!(node_after(&recorder, "far").bounds.x, 0);
}

#[test]
fn a_page_zoomed_past_the_texture_limit_gets_its_visible_part_rendered_sharp() {
    let mut host = Host::start();
    let canvas = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
    let mut recorder = Recorder::open(&mut host, &canvas);
    recorder.read_assets = true;
    let board = recorder.app.board_rect.expect("the board is drawn");
    recorder.app.requested_pages.clear();
    recorder.app.set_zoom(board, 8.0);
    let draw = |recorder: &mut Recorder| {
        let _ = recorder.ctx.run(screen(1.0, 1280.0, 800.0), |ctx| {
            recorder.app.draw_frame(ctx);
        });
    };
    draw(&mut recorder);
    recorder.settle(&mut host);
    std::thread::sleep(super::board::SETTLE + std::time::Duration::from_millis(30));
    let mark = recorder.calls.len();
    draw(&mut recorder);
    recorder.settle(&mut host);
    draw(&mut recorder);
    let region = recorder.app.region_textures.get(&0).expect("a region texture for the zoomed page");
    let page = recorder.app.page_textures.get(&0).expect("the whole-page texture");
    assert!(region.dpi > page.dpi, "the region ({}) is sharper than the capped page ({})", region.dpi, page.dpi);
    assert!(region.region.width < page.page.size.width, "only part of the page: {:?}", region.region);
    assert!(recorder.calls[mark..].iter().any(|call| call == "render.get"));
}

/// A headless run opens no window: every wired command, also the ones that would show a file dialog or the preview
/// window, runs against a canvas and the process still has no visible top-level window. Jobs are left out: they start a
/// model runtime, not a window.
#[test]
fn no_command_opens_a_window_in_a_headless_run() {
    crate::dialogs::set_headless(true);
    assert!(crate::dialogs::headless());
    let mut host = Host::start();
    let before = crate::dialogs::visible_windows_of_this_process();
    for spec in commands::COMMANDS.iter().filter(|spec| commands::available(spec.id)) {
        if matches!(spec.id, "ai.request" | "ai.retry" | "ai.cancel" | "home.create") {
            continue;
        }
        let canvas = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
        let mut recorder = Recorder::open(&mut host, &canvas);
        recorder.step(&mut host, "select.many", json!({ "node_ids": ["a", "b", "c"] })).expect("selected");
        let _ = recorder.app.invoke(spec.id, arguments(spec, &canvas));
        recorder.settle(&mut host);
        assert_eq!(crate::dialogs::visible_windows_of_this_process(), before, "{} opened a window", spec.id);
    }
}

#[test]
fn the_view_group_keeps_12_from_the_board_corner_whatever_it_holds() {
    let mut host = Host::start();
    let canvas = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
    let mut recorder = Recorder::open(&mut host, &canvas);
    let mut layout = None;
    for _ in 0..2 {
        let _ = recorder.ctx.run(screen(1.0, 1440.0, 900.0), |ctx| {
            layout = Some(recorder.app.draw_frame(ctx));
        });
    }
    let board = layout.expect("a frame").center;
    let group = recorder.app.view_group_rect.expect("the view group is drawn");
    // tokens `layout.board.view-group`: right 12, top 12, 36 high (1 padding + 1 border around 32 buttons).
    assert!((group.right() - (board.right() - theme::FLOAT_INSET)).abs() < 0.5, "{group:?} in {board:?}");
    assert!((group.top() - (board.top() + theme::FLOAT_INSET)).abs() < 0.5, "{group:?} in {board:?}");
    assert!((group.height() - theme::VIEW_GROUP_HEIGHT).abs() < 0.5, "{group:?}");
}

#[test]
fn copied_group_pastes_between_canvases_with_fresh_internal_ids() {
    let mut host = Host::start();
    let source = host.create("canvas", "source");
    let target = host.create("canvas", "target");
    let page_id = match &host.artifact(&source).body {
        ArtifactBody::Canvas(document) => document.pages[0].id.clone(),
        _ => panic!("a canvas"),
    };
    let group: designllm_contracts::Node = serde_json::from_value(json!({
        "id": "group",
        "kind": "group",
        "bounds": { "x": 0, "y": 0, "width": 100000, "height": 100000 },
        "interactions": [{ "action": "toggle_visibility", "node_id": "child" }],
        "variants": { "hover": { "bold": true } },
        "children": [{
            "id": "child",
            "kind": "rectangle",
            "bounds": { "x": 1000, "y": 1000, "width": 10000, "height": 10000 },
            "interactions": [{ "action": "set_variant", "node_id": "group", "variant": "hover" }]
        }]
    })).expect("group with internal interactions");
    host.apply(
        &source,
        vec![designllm_contracts::Operation::AddNode { page_id, parent_id: None, node: group }],
        [designllm_contracts::Intent::Content].into_iter().collect(),
    ).expect("group added");

    let mut recorder = Recorder::open(&mut host, &source);
    recorder.step(&mut host, "select.node", json!({ "node_id": "group" })).expect("group selected");
    recorder.step(&mut host, "edit.copy", json!({})).expect("group copied");
    recorder.step(&mut host, "nav.open_project", json!({ "artifact_id": target })).expect("target opened");
    recorder.step(&mut host, "edit.paste", json!({})).expect("group pasted");

    let pasted_id = recorder.app.selected_nodes.first().expect("pasted group selected").clone();
    let pasted = super::doc_tree::find_node(recorder.app.artifact.as_ref().expect("target loaded"), &pasted_id).expect("pasted group");
    let child = pasted.children.first().expect("pasted child");
    assert_ne!(pasted.id, "group");
    assert_ne!(child.id, "child");
    let designllm_contracts::Interaction::ToggleVisibility { node_id } = &pasted.interactions[0] else { panic!("root interaction") };
    assert_eq!(node_id, &child.id);
    let designllm_contracts::Interaction::SetVariant { node_id, variant } = &child.interactions[0] else { panic!("child interaction") };
    assert_eq!((node_id.as_str(), variant.as_str()), (pasted.id.as_str(), "hover"));
}

fn canvas_with_cross_root_interactions(host: &mut Host, title: &str) -> String {
    use designllm_contracts::{Interaction, Intent, Node, Operation};

    let canvas = host.create("canvas", title);
    let page_id = match &host.artifact(&canvas).body {
        ArtifactBody::Canvas(document) => document.pages[0].id.clone(),
        _ => panic!("a canvas"),
    };
    let first: Node = serde_json::from_value(json!({
        "id": "first",
        "kind": "rectangle",
        "bounds": { "x": 0, "y": 0, "width": 100000, "height": 100000 },
        "variants": { "hover": { "bold": true } }
    })).expect("first root with a variant");
    let second: Node = serde_json::from_value(json!({
        "id": "second",
        "kind": "rectangle",
        "bounds": { "x": 200000, "y": 0, "width": 100000, "height": 100000 }
    })).expect("second root");
    host.apply(
        &canvas,
        vec![
            Operation::AddNode { page_id: page_id.clone(), parent_id: None, node: first },
            Operation::AddNode { page_id: page_id.clone(), parent_id: None, node: second },
            Operation::SetInteraction { node_id: "first".into(), interaction: Interaction::ToggleVisibility { node_id: "second".into() } },
            Operation::SetInteraction { node_id: "second".into(), interaction: Interaction::SetVariant { node_id: "first".into(), variant: "hover".into() } },
        ],
        [Intent::Content].into_iter().collect(),
    ).expect("two roots with cross-root interactions");
    host.apply(
        &canvas,
        vec![Operation::SetNodeAppearance { node_ids: vec!["second".into()], image_fit: None, locked: Some(true) }],
        [Intent::Design].into_iter().collect(),
    ).expect("second root locked");
    canvas
}

fn assert_fresh_cross_root_interactions(
    nodes: &[designllm_contracts::Node],
    original_first: &str,
    original_second: &str,
) -> (String, String) {
    use designllm_contracts::Interaction;

    let first = nodes
        .iter()
        .find(|node| node.interactions.iter().any(|interaction| matches!(interaction, Interaction::ToggleVisibility { .. })))
        .expect("copied first root");
    let second = nodes
        .iter()
        .find(|node| node.interactions.iter().any(|interaction| matches!(interaction, Interaction::SetVariant { .. })))
        .expect("copied second root");
    assert_ne!(first.id, original_first);
    assert_ne!(second.id, original_second);
    let [Interaction::ToggleVisibility { node_id: first_target }] = first.interactions.as_slice() else { panic!("the first root keeps its ToggleVisibility interaction") };
    assert_eq!(first_target, &second.id);
    let [Interaction::SetVariant { node_id: second_target, variant }] = second.interactions.as_slice() else { panic!("the second root keeps its SetVariant interaction") };
    assert_eq!((second_target, variant.as_str()), (&first.id, "hover"));
    assert!(first.variants.contains_key("hover"), "SetVariant target metadata is retained");
    assert_eq!(second.ext.locked, Some(true), "the locked root remains locked without a Design edit in the copy action");
    (first.id.clone(), second.id.clone())
}

#[test]
fn cross_root_interactions_and_lock_survive_duplicate_and_canvas_paste() {
    let mut host = Host::start();
    let source = canvas_with_cross_root_interactions(&mut host, "source");
    let target = host.create("canvas", "target");
    let mut recorder = Recorder::open(&mut host, &source);
    recorder.step(&mut host, "select.many", json!({ "node_ids": ["first", "second"] })).expect("roots selected");
    recorder.step(&mut host, "edit.duplicate", json!({})).expect("cross-root references duplicate atomically");

    let duplicated_artifact = recorder.app.artifact.as_ref().expect("source is loaded");
    let duplicates: Vec<_> = recorder.app.selected_nodes.iter().filter_map(|id| super::doc_tree::find_node(duplicated_artifact, id).cloned()).collect();
    assert_eq!(duplicates.len(), 2);
    let (duplicate_first, duplicate_second) = assert_fresh_cross_root_interactions(&duplicates, "first", "second");
    let source_after_duplicate = host.artifact(&source);
    let source_first = super::doc_tree::find_node(&source_after_duplicate, "first").expect("original first root");
    let source_second = super::doc_tree::find_node(&source_after_duplicate, "second").expect("original second root");
    let [designllm_contracts::Interaction::ToggleVisibility { node_id }] = source_first.interactions.as_slice() else { panic!("the original first root keeps ToggleVisibility") };
    assert_eq!(node_id.as_str(), "second");
    let [designllm_contracts::Interaction::SetVariant { node_id, variant }] = source_second.interactions.as_slice() else { panic!("the original second root keeps SetVariant") };
    assert_eq!((node_id.as_str(), variant.as_str()), ("first", "hover"));
    assert_eq!(source_second.ext.locked, Some(true), "the original lock remains unchanged");
    let source_revision = source_after_duplicate.revision;

    recorder.step(&mut host, "select.many", json!({ "node_ids": [duplicate_first.clone(), duplicate_second.clone()] })).expect("duplicates selected");
    recorder.step(&mut host, "edit.copy", json!({})).expect("both linked roots copied");
    recorder.step(&mut host, "nav.open_project", json!({ "artifact_id": target })).expect("target canvas opened");
    recorder.step(&mut host, "edit.paste", json!({})).expect("cross-root references paste atomically");

    let target_artifact = recorder.app.artifact.as_ref().expect("target is loaded");
    let pasted: Vec<_> = recorder.app.selected_nodes.iter().filter_map(|id| super::doc_tree::find_node(target_artifact, id).cloned()).collect();
    assert_eq!(pasted.len(), 2);
    assert_fresh_cross_root_interactions(&pasted, "first", "second");
    assert!(pasted.iter().all(|node| node.id != duplicate_first && node.id != duplicate_second), "paste uses a second fresh ID set");
    assert_eq!(host.artifact(&source).revision, source_revision, "pasting leaves the source artifact unchanged");
}

#[test]
fn slides_preflight_refuses_cross_root_interactions_without_a_change() {
    let mut host = Host::start();
    let source = canvas_with_cross_root_interactions(&mut host, "source");
    let slides = host.create("slides", "target");
    let source_revision = host.artifact(&source).revision;
    let slides_revision = host.artifact(&slides).revision;
    let mut recorder = Recorder::open(&mut host, &source);
    recorder.step(&mut host, "select.many", json!({ "node_ids": ["first", "second"] })).expect("roots selected");
    recorder.step(&mut host, "edit.copy", json!({})).expect("roots copied");
    recorder.step(&mut host, "nav.open_project", json!({ "artifact_id": slides.clone() })).expect("Slides opened");

    let calls_before = recorder.calls.len();
    let refusal = recorder.step(&mut host, "edit.paste", json!({})).expect_err("Slides cannot preserve interactions or variants");
    assert!(refusal.contains("상호작용과 변형"), "the reason is explicit: {refusal}");
    assert_eq!(recorder.calls.len(), calls_before, "preflight sends no partial AddNode call");
    assert_eq!(host.artifact(&source).revision, source_revision);
    assert_eq!(host.artifact(&slides).revision, slides_revision);
}

#[test]
fn simple_node_trees_paste_between_canvas_and_created_slides() {
    let mut host = Host::start();
    let canvas = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
    let slides = host.create("slides", "created slides");
    let mut recorder = Recorder::open(&mut host, &canvas);
    recorder.step(&mut host, "select.node", json!({ "node_id": "a" })).expect("canvas node selected");
    recorder.step(&mut host, "edit.copy", json!({})).expect("canvas node copied");
    recorder.step(&mut host, "nav.open_project", json!({ "artifact_id": slides })).expect("slides opened");
    recorder.step(&mut host, "edit.paste", json!({})).expect("canvas node pasted into slides");
    let pasted_id = recorder.app.selected_nodes[0].clone();
    let pasted = super::doc_tree::find_node(recorder.app.artifact.as_ref().expect("slides loaded"), &pasted_id).expect("slide node");
    assert_eq!(pasted.text, "가");

    let canvas_target = host.create("canvas", "canvas target");
    let slides_source = host.create("slides", "created slide source");
    let page_id = match &host.artifact(&slides_source).body {
        ArtifactBody::Slides(document) => document.pages[0].id.clone(),
        _ => panic!("slides"),
    };
    let node: designllm_contracts::Node = serde_json::from_value(json!({
        "id": "slide-text", "kind": "text", "text": "슬라이드 텍스트", "bounds": { "x": 0, "y": 0, "width": 1000000, "height": 400000 }
    })).expect("text node");
    host.apply(
        &slides_source,
        vec![designllm_contracts::Operation::AddNode { page_id, parent_id: None, node }],
        [designllm_contracts::Intent::Content].into_iter().collect(),
    ).expect("node added to created slides");
    let mut recorder = Recorder::open(&mut host, &slides_source);
    recorder.step(&mut host, "select.node", json!({ "node_id": "slide-text" })).expect("slide node selected");
    recorder.step(&mut host, "edit.copy", json!({})).expect("slide node copied");
    recorder.step(&mut host, "nav.open_project", json!({ "artifact_id": canvas_target })).expect("canvas opened");
    recorder.step(&mut host, "edit.paste", json!({})).expect("slide node pasted into canvas");
    let pasted_id = recorder.app.selected_nodes[0].clone();
    let pasted = super::doc_tree::find_node(recorder.app.artifact.as_ref().expect("canvas loaded"), &pasted_id).expect("canvas node");
    assert_eq!(pasted.text, "슬라이드 텍스트");
}

#[test]
fn imported_pptx_objects_are_not_copied_without_their_native_mappings() {
    let mut host = Host::start();
    let path = std::fs::canonicalize(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/office/basic-preserve.pptx"))
        .expect("shared PPTX fixture");
    let opened = host.call("artifact.open", json!({
        "request_id": designllm_contracts::new_id(), "actor": "test", "path": path.to_string_lossy(), "kind": "slides"
    })).expect("PPTX opened");
    let source = opened["id"].as_str().expect("artifact id").to_owned();
    let mut recorder = Recorder::open(&mut host, &source);
    recorder.step(&mut host, "select.node", json!({ "node_id": "slide-256:2" })).expect("PPTX node selected");
    let mark = recorder.calls.len();
    assert!(recorder.step(&mut host, "edit.copy", json!({})).is_err(), "opaque PPTX mapping is refused");
    assert!(recorder.calls[mark..].is_empty(), "no partial copy call reaches the host");
}

#[test]
fn cfb_pptx_open_refusal_uses_the_user_message_without_registering_an_artifact() {
    let mut host = Host::start();
    let canvas = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
    let mut recorder = Recorder::open(&mut host, &canvas);
    let scratch = std::env::temp_dir().join(format!("designllm-desktop-open-refusal-{}", designllm_contracts::new_id()));
    std::fs::create_dir(&scratch).expect("create a unique scratch directory");
    let path = scratch.join("unsupported.pptx");
    let mut bytes = vec![0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1];
    bytes.extend_from_slice(b"synthetic container payload");
    std::fs::write(&path, &bytes).expect("write synthetic CFB input");

    recorder.app.open_path = path.to_string_lossy().into_owned();
    recorder.app.open_file();
    recorder.settle(&mut host);

    assert!(recorder.calls.contains(&"artifact.open".to_owned()));
    assert_eq!(
        recorder.app.notices.current(std::time::Instant::now()).map(|notice| notice.text.as_str()),
        Some("암호화된 문서이거나 지원하지 않는 이전 Office 형식이라 열 수 없습니다")
    );
    let artifacts = host.call("workspace.list", json!({})).expect("workspace list");
    assert_eq!(artifacts["artifacts"].as_array().map(Vec::len), Some(1), "refused file is not registered");
    assert_eq!(std::fs::read(&path).expect("source remains present"), bytes, "opening does not change the input file");

    std::fs::remove_file(&path).expect("remove this test's input file");
    std::fs::remove_dir(&scratch).expect("remove this test's empty scratch directory");
}

#[test]
fn copying_a_nested_node_converts_its_frame_to_page_coordinates() {
    let mut host = Host::start();
    let source = host.create("canvas", "nested source");
    let target = host.create("canvas", "nested target");
    let page_id = match &host.artifact(&source).body {
        ArtifactBody::Canvas(document) => document.pages[0].id.clone(),
        _ => panic!("canvas"),
    };
    let group: designllm_contracts::Node = serde_json::from_value(json!({
        "id": "frame", "kind": "group", "bounds": { "x": 1000, "y": 2000, "width": 100000, "height": 100000 },
        "children": [{ "id": "child", "kind": "text", "text": "nested", "bounds": { "x": 300, "y": 400, "width": 10000, "height": 5000 } }]
    })).expect("group tree");
    host.apply(
        &source,
        vec![designllm_contracts::Operation::AddNode { page_id, parent_id: None, node: group }],
        [designllm_contracts::Intent::Content].into_iter().collect(),
    ).expect("group added");

    let mut recorder = Recorder::open(&mut host, &source);
    recorder.step(&mut host, "select.node", json!({ "node_id": "child" })).expect("child selected");
    recorder.step(&mut host, "edit.copy", json!({})).expect("child copied");
    recorder.step(&mut host, "nav.open_project", json!({ "artifact_id": target })).expect("target opened");
    recorder.step(&mut host, "edit.paste", json!({})).expect("child pasted");
    let pasted_id = recorder.app.selected_nodes[0].clone();
    let pasted = super::doc_tree::find_node(recorder.app.artifact.as_ref().expect("target loaded"), &pasted_id).expect("pasted child");
    assert_eq!((pasted.bounds.x, pasted.bounds.y), (191_800, 192_900));
}

#[test]
fn duplicate_copies_every_selected_node_and_selects_the_new_ids() {
    let mut host = Host::start();
    let canvas = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
    let mut recorder = Recorder::open(&mut host, &canvas);
    recorder.step(&mut host, "select.many", json!({ "node_ids": ["a", "b"] })).expect("nodes selected");
    recorder.step(&mut host, "edit.duplicate", json!({})).expect("nodes duplicated");

    assert_eq!(recorder.app.selected_nodes.len(), 2);
    let artifact = recorder.app.artifact.as_ref().expect("canvas loaded");
    for (source_id, duplicate_id) in ["a", "b"].into_iter().zip(&recorder.app.selected_nodes) {
        assert_ne!(duplicate_id, source_id);
        let source = super::doc_tree::find_node(artifact, source_id).expect("source");
        let duplicate = super::doc_tree::find_node(artifact, duplicate_id).expect("duplicate");
        assert_eq!((duplicate.bounds.x - source.bounds.x, duplicate.bounds.y - source.bounds.y), (20 * 9525, 20 * 9525));
        assert_eq!(duplicate.text, source.text);
    }
}

#[test]
fn property_copy_paste_applies_the_source_style_to_the_selection() {
    let mut host = Host::start();
    let canvas = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
    host.apply(
        &canvas,
        vec![designllm_contracts::Operation::SetStyle {
            node_ids: vec!["a".into()],
            patch: designllm_contracts::StylePatch {
                fill: Some("#FF0000".into()),
                font_size: Some(20 * designllm_contracts::EMU_PER_PT),
                bold: Some(true),
                font_weight: Some(700),
                underline: Some(true),
                ..Default::default()
            },
        }],
        [designllm_contracts::Intent::Design].into_iter().collect(),
    ).expect("source style set");
    host.apply(
        &canvas,
        vec![designllm_contracts::Operation::SetStyle {
            node_ids: vec!["b".into()],
            patch: designllm_contracts::StylePatch {
                corner_radius: Some(400),
                opacity_milli: Some(750),
                font_weight: Some(400),
                underline: Some(false),
                strikethrough: Some(true),
                ..Default::default()
            },
        }],
        [designllm_contracts::Intent::Design].into_iter().collect(),
    ).expect("target starts with different effects");
    let mut recorder = Recorder::open(&mut host, &canvas);
    recorder.step(&mut host, "select.node", json!({ "node_id": "a" })).expect("source selected");
    recorder.step(&mut host, "edit.copy_properties", json!({})).expect("properties copied");
    recorder.step(&mut host, "select.node", json!({ "node_id": "b" })).expect("target selected");
    assert_eq!(
        recorder.app.context.as_ref().unwrap().capabilities[&designllm_contracts::Feature::Effects].edit,
        designllm_contracts::Support::Approximate,
        "copy properties treats approximate effects as editable",
    );
    recorder.step(&mut host, "edit.paste_properties", json!({})).expect("properties pasted");
    let artifact = recorder.app.artifact.as_ref().expect("canvas loaded");
    let source = super::doc_tree::find_node(artifact, "a").expect("source");
    let target = super::doc_tree::find_node(artifact, "b").expect("target");
    assert_eq!(target.style, source.style, "all base and v1.2 style fields, including source defaults, match");
}

#[test]
fn property_copy_refuses_effects_unsupported_by_the_target_without_revision_change() {
    let mut host = Host::start();
    let canvas = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
    host.apply(
        &canvas,
        vec![designllm_contracts::Operation::SetStyle {
            node_ids: vec!["a".into()],
            patch: designllm_contracts::StylePatch { opacity_milli: Some(700), ..Default::default() },
        }],
        [designllm_contracts::Intent::Design].into_iter().collect(),
    ).expect("source effect set");
    let slides = host.create("slides", "effects unsupported");
    let page_id = match &host.artifact(&slides).body {
        ArtifactBody::Slides(document) => document.pages[0].id.clone(),
        _ => panic!("slides"),
    };
    let node: designllm_contracts::Node = serde_json::from_value(json!({
        "id": "slide-target", "kind": "text", "text": "target", "bounds": { "x": 0, "y": 0, "width": 1000000, "height": 400000 }
    })).expect("slide node");
    host.apply(
        &slides,
        vec![designllm_contracts::Operation::AddNode { page_id, parent_id: None, node }],
        [designllm_contracts::Intent::Content].into_iter().collect(),
    ).expect("slide node added");
    let mut recorder = Recorder::open(&mut host, &canvas);
    recorder.step(&mut host, "select.node", json!({ "node_id": "a" })).expect("source selected");
    recorder.step(&mut host, "edit.copy_properties", json!({})).expect("properties copied");
    recorder.step(&mut host, "nav.open_project", json!({ "artifact_id": slides })).expect("slides opened");
    // Slides edits effects approximately: an outer shadow is kept, and the other effects (opacity here) are refused by the engine.
    assert_eq!(
        recorder.app.context.as_ref().unwrap().capabilities[&designllm_contracts::Feature::Effects].edit,
        designllm_contracts::Support::Approximate,
    );
    recorder.step(&mut host, "select.node", json!({ "node_id": "slide-target" })).expect("target selected");
    let revision = recorder.app.active.as_ref().unwrap().revision;
    let mark = recorder.calls.len();
    recorder.step(&mut host, "edit.paste_properties", json!({})).expect("the paste is sent");
    // The window does not refuse an approximate effect itself: the change reaches the host, which refuses it.
    assert!(
        recorder.calls[mark..].iter().any(|call| call == "changes.apply:set_style"),
        "the change reaches the host: {:?}",
        &recorder.calls[mark..],
    );
    assert_eq!(recorder.app.active.as_ref().unwrap().revision, revision);
    assert_eq!(host.artifact(&slides).revision, revision);
}

#[test]
fn linked_picture_sidecar_is_duplicated_and_pasted_with_its_new_node_id() {
    let mut host = Host::start();
    let figure = super::link_flow_tests::drawn_figure(&mut host, "figure", 1);
    let (_, figure_revision) = host.figure(&figure);
    let rendered = host.call("figure.render_link", json!({
        "figure_id": figure, "figure_revision": figure_revision, "representation": "png", "profile": { "dpi": 96, "transparent": false }
    })).expect("figure render prepared");
    let asset: designllm_contracts::AssetRef = serde_json::from_value(rendered["asset"].clone()).expect("render asset");
    let link = designllm_contracts::FigureLink {
        figure_id: figure.clone(),
        figure_revision,
        target_node_id: "linked-image".into(),
        representation: "png".into(),
        render_hash: rendered["render_hash"].as_str().expect("render hash").into(),
    };
    let source = host.create("canvas", "source");
    let source_page = match &host.artifact(&source).body {
        ArtifactBody::Canvas(document) => document.pages[0].id.clone(),
        _ => panic!("a canvas"),
    };
    let image: designllm_contracts::Node = serde_json::from_value(json!({
        "id": "linked-image", "kind": "image", "bounds": { "x": 0, "y": 0, "width": 1000000, "height": 500000 }, "asset": asset
    })).expect("linked image");
    host.apply(
        &source,
        vec![
            designllm_contracts::Operation::AddNode { page_id: source_page, parent_id: None, node: image },
            designllm_contracts::Operation::UpdateFigureLink { link },
        ],
        [designllm_contracts::Intent::Content].into_iter().collect(),
    ).expect("linked image inserted");
    let target = host.create("canvas", "target");
    let mut recorder = Recorder::open(&mut host, &source);
    recorder.step(&mut host, "select.node", json!({ "node_id": "linked-image" })).expect("source selected");
    let duplicate_mark = recorder.calls.len();
    recorder.step(&mut host, "edit.duplicate", json!({})).expect("linked image duplicated");
    let duplicate_calls_owned = distinct_in_order(&recorder.calls[duplicate_mark..]);
    let duplicate_calls: Vec<&str> = duplicate_calls_owned.iter().map(String::as_str).collect();
    assert_eq!(
        duplicate_calls,
        ["changes.apply:add_node", "changes.apply:update_figure_link", "selection.set"],
    );
    let duplicate_id = recorder.app.selected_nodes[0].clone();
    let source_artifact = recorder.app.artifact.as_ref().expect("source loaded");
    let links = &source_artifact.figure_links;
    assert_eq!(links.len(), 2);
    assert!(links.iter().any(|link| link.target_node_id == duplicate_id));
    assert!(super::doc_tree::find_node(source_artifact, &duplicate_id).is_some(), "duplicate node exists with its sidecar");
    let original_link = (*links.iter().find(|link| link.target_node_id == "linked-image").expect("original link")).clone();
    assert!(links.iter().any(|link| link.target_node_id == "linked-image" && link.render_hash == original_link.render_hash));
    recorder.step(&mut host, "edit.undo", json!({})).expect("duplicate undone");
    let links = &recorder.app.artifact.as_ref().expect("source reloaded").figure_links;
    assert_eq!(links.len(), 1, "undo removes only the copied link");
    assert_eq!((links[0].target_node_id.as_str(), links[0].render_hash.as_str()), ("linked-image", original_link.render_hash.as_str()));
    recorder.step(&mut host, "edit.redo", json!({})).expect("duplicate redone");
    let source_artifact = recorder.app.artifact.as_ref().expect("source reloaded");
    let links = &source_artifact.figure_links;
    assert_eq!(links.len(), 2, "redo restores the copied link");
    assert!(super::doc_tree::find_node(source_artifact, &duplicate_id).is_some(), "redo restores the copied node");

    recorder.step(&mut host, "select.node", json!({ "node_id": "linked-image" })).expect("original reselected");
    recorder.step(&mut host, "edit.copy", json!({})).expect("linked image copied");
    recorder.step(&mut host, "nav.open_project", json!({ "artifact_id": target })).expect("target opened");
    let paste_mark = recorder.calls.len();
    recorder.step(&mut host, "edit.paste", json!({})).expect("linked image pasted");
    let paste_calls_owned = distinct_in_order(&recorder.calls[paste_mark..]);
    let paste_calls: Vec<&str> = paste_calls_owned.iter().map(String::as_str).collect();
    assert_eq!(
        paste_calls,
        ["changes.apply:add_node", "changes.apply:update_figure_link", "selection.set"],
    );
    let pasted_id = recorder.app.selected_nodes[0].clone();
    let links = &recorder.app.artifact.as_ref().expect("target loaded").figure_links;
    assert_eq!(links.len(), 1);
    assert_eq!(links[0].target_node_id, pasted_id);
    assert_eq!((links[0].figure_id.as_str(), links[0].render_hash.as_str()), (original_link.figure_id.as_str(), original_link.render_hash.as_str()));
    host.apply(
        &figure,
        vec![designllm_contracts::Operation::SetCaption { caption: "revision 2".into() }],
        [designllm_contracts::Intent::Content].into_iter().collect(),
    ).expect("figure advanced");
    recorder.step(&mut host, "figure.update_link", json!({ "node_id": pasted_id })).expect("copy refreshed to the new figure revision");
    let refreshed = &recorder.app.artifact.as_ref().expect("target reloaded").figure_links;
    assert_eq!(refreshed.len(), 1);
    assert_eq!(refreshed[0].figure_revision, figure_revision + 1);
    assert_ne!(refreshed[0].render_hash, original_link.render_hash);
    let target_image = super::doc_tree::find_node(recorder.app.artifact.as_ref().unwrap(), &pasted_id).expect("refreshed image");
    assert_eq!(target_image.asset.as_ref().unwrap().sha256, refreshed[0].render_hash);
    let source_links = host.artifact(&source).figure_links;
    assert!(source_links.iter().any(|link| link.target_node_id == "linked-image" && link.figure_revision == figure_revision && link.render_hash == original_link.render_hash));
    assert_eq!(source_links.len(), 2, "pasting into another artifact leaves the source links untouched");
    assert!(!source_links.iter().any(|link| link.target_node_id == pasted_id));
}

#[test]
fn invalid_figure_link_and_unsupported_target_leave_added_node_and_sidecar_uncommitted() {
    let mut host = Host::start();
    let figure = super::link_flow_tests::drawn_figure(&mut host, "figure", 1);
    let (_, figure_revision) = host.figure(&figure);
    let rendered = host.call("figure.render_link", json!({
        "figure_id": figure,
        "figure_revision": figure_revision,
        "representation": "png",
        "profile": { "dpi": 96, "transparent": false },
    })).expect("figure render prepared");
    let render_hash = rendered["render_hash"].as_str().expect("render hash").to_owned();
    let canvas = host.create("canvas", "atomic links");
    let page_id = match &host.artifact(&canvas).body {
        ArtifactBody::Canvas(document) => document.pages[0].id.clone(),
        _ => panic!("canvas"),
    };
    let revision = host.artifact(&canvas).revision;
    let rect = |id: &str| -> designllm_contracts::Node {
        serde_json::from_value(json!({
            "id": id, "kind": "rectangle", "bounds": { "x": 0, "y": 0, "width": 1000000, "height": 500000 }
        })).expect("rectangle")
    };
    let add = |id: &str| designllm_contracts::Operation::AddNode { page_id: page_id.clone(), parent_id: None, node: rect(id) };
    let link = |target: &str, hash: String| designllm_contracts::FigureLink {
        figure_id: figure.clone(), figure_revision, target_node_id: target.into(), representation: "png".into(), render_hash: hash,
    };

    assert!(host.apply(
        &canvas,
        vec![add("invalid-render"), designllm_contracts::Operation::UpdateFigureLink { link: link("invalid-render", "0".repeat(64)) }],
        [designllm_contracts::Intent::Content].into_iter().collect(),
    ).is_err(), "a render not prepared by the named figure is refused");
    let after_invalid = host.artifact(&canvas);
    assert_eq!(after_invalid.revision, revision);
    assert!(super::doc_tree::find_node(&after_invalid, "invalid-render").is_none());
    assert!(after_invalid.figure_links.is_empty());

    assert!(host.apply(
        &canvas,
        vec![add("not-an-image"), designllm_contracts::Operation::UpdateFigureLink { link: link("not-an-image", render_hash) }],
        [designllm_contracts::Intent::Content].into_iter().collect(),
    ).is_err(), "a figure link cannot target a rectangle");
    let after_target = host.artifact(&canvas);
    assert_eq!(after_target.revision, revision);
    assert!(super::doc_tree::find_node(&after_target, "not-an-image").is_none());
    assert!(after_target.figure_links.is_empty());
}

#[test]
fn a_cross_document_paste_whose_figure_link_is_refused_adds_none_of_its_nodes() {
    let mut host = Host::start();
    let figure = super::link_flow_tests::drawn_figure(&mut host, "figure", 1);
    let (_, figure_revision) = host.figure(&figure);
    let rendered = host.call("figure.render_link", json!({
        "figure_id": figure, "figure_revision": figure_revision, "representation": "png", "profile": { "dpi": 96, "transparent": false }
    })).expect("figure render prepared");
    let asset: designllm_contracts::AssetRef = serde_json::from_value(rendered["asset"].clone()).expect("render asset");
    let link = designllm_contracts::FigureLink {
        figure_id: figure.clone(),
        figure_revision,
        target_node_id: "linked-image".into(),
        representation: "png".into(),
        render_hash: rendered["render_hash"].as_str().expect("render hash").into(),
    };
    let source = host.create("canvas", "source");
    let source_page = match &host.artifact(&source).body {
        ArtifactBody::Canvas(document) => document.pages[0].id.clone(),
        _ => panic!("a canvas"),
    };
    let plain: designllm_contracts::Node = serde_json::from_value(json!({
        "id": "plain-box", "kind": "rectangle", "bounds": { "x": 0, "y": 0, "width": 1000000, "height": 500000 }
    })).expect("plain box");
    let image: designllm_contracts::Node = serde_json::from_value(json!({
        "id": "linked-image", "kind": "image", "bounds": { "x": 0, "y": 600000, "width": 1000000, "height": 500000 }, "asset": asset
    })).expect("linked image");
    host.apply(
        &source,
        vec![
            designllm_contracts::Operation::AddNode { page_id: source_page.clone(), parent_id: None, node: plain },
            designllm_contracts::Operation::AddNode { page_id: source_page, parent_id: None, node: image },
            designllm_contracts::Operation::UpdateFigureLink { link },
        ],
        [designllm_contracts::Intent::Content].into_iter().collect(),
    ).expect("the source canvas gets a box and a linked image");

    let target = host.create("canvas", "target");
    let mut recorder = Recorder::open(&mut host, &source);
    recorder.step(&mut host, "select.many", json!({ "node_ids": ["plain-box", "linked-image"] })).expect("both nodes are selected");
    recorder.step(&mut host, "edit.copy", json!({})).expect("both nodes are copied");
    recorder.step(&mut host, "nav.open_project", json!({ "artifact_id": target.clone() })).expect("target canvas opened");
    let target_revision = host.artifact(&target).revision;
    {
        let clipboard = recorder.app.node_clipboard.as_mut().expect("the copy is on the clipboard");
        assert_eq!(clipboard.figure_links.len(), 1, "the copied image carries its figure link");
        clipboard.figure_links[0].render_hash = "0".repeat(64);
    }

    recorder.step(&mut host, "edit.paste", json!({})).expect("the paste is sent as one change");
    recorder.settle(&mut host);
    let after = host.artifact(&target);
    assert_eq!(after.revision, target_revision, "the target document does not change");
    assert!(after.figure_links.is_empty(), "no figure link is stored");
    let ArtifactBody::Canvas(document) = &after.body else { panic!("a canvas") };
    assert!(document.pages[0].nodes.is_empty(), "the box pasted with the refused link is not added either");
}

#[test]
fn layer_find_opens_the_layers_search_and_sets_its_query() {
    let mut host = Host::start();
    let canvas = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
    let mut recorder = Recorder::open(&mut host, &canvas);
    let before = host.artifact(&canvas);
    let before_body = serde_json::to_value(&before.body).expect("serialize the document body");
    let before_revision = before.revision;
    let calls = recorder.calls.len();
    recorder.step(&mut host, "layer.find", json!({ "text": "box" })).expect("search opened");
    assert!(recorder.app.layer_find_open);
    assert_eq!(recorder.app.layer_find_query, "box");
    assert!(recorder.app.view_ref().left_open);
    assert_eq!(recorder.app.view_ref().left_tab, super::board::LeftTab::Layers);
    assert_eq!(recorder.calls.len(), calls, "search is a local read-only UI action");
    let after = host.artifact(&canvas);
    assert_eq!(after.revision, before_revision);
    assert_eq!(serde_json::to_value(&after.body).expect("serialize the body"), before_body, "search leaves document content unchanged");
}

#[test]
fn locked_nodes_can_be_unlocked_and_hidden_nodes_restored_from_the_layer_tree() {
    use designllm_contracts::{ErrorCode, Intent, Operation};

    let mut host = Host::start();
    let canvas = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
    let mut recorder = Recorder::open(&mut host, &canvas);
    recorder.step(&mut host, "select.node", json!({ "node_id": "a" })).expect("the node is selected");

    recorder.step(&mut host, "edit.lock", json!({})).expect("the node is locked");
    let locked_artifact = host.artifact(&canvas);
    let locked_node = super::doc_tree::find_node(&locked_artifact, "a").expect("locked node remains in the artifact");
    assert_eq!(locked_node.ext.locked, Some(true));
    let mut changed_bounds = locked_node.bounds;
    changed_bounds.x += 1;
    let refusal = host
        .apply(
            &canvas,
            vec![Operation::SetGeometry { node_id: "a".into(), bounds: changed_bounds }],
            [Intent::Design].into_iter().collect(),
        )
        .expect_err("the actual Canvas host rejects edits to a locked node");
    assert_eq!(refusal.code, ErrorCode::ContentLocked, "{refusal:?}");

    recorder.step(&mut host, "edit.unlock", json!({})).expect("the existing appearance operation unlocks it");
    assert_eq!(super::doc_tree::find_node(&host.artifact(&canvas), "a").unwrap().ext.locked, Some(false));

    recorder.step(&mut host, "edit.hide", json!({})).expect("the node is hidden");
    assert_eq!(super::doc_tree::find_node(&host.artifact(&canvas), "a").unwrap().ext.hidden, Some(true));
    recorder.step(&mut host, "select.node", json!({ "node_id": "a" })).expect("a hidden layer remains selectable");
    assert_eq!(recorder.app.selected_node.as_deref(), Some("a"));
    recorder.step(&mut host, "edit.show", json!({})).expect("the existing info operation restores visibility");
    assert_eq!(super::doc_tree::find_node(&host.artifact(&canvas), "a").unwrap().ext.hidden, Some(false));
}

#[test]
fn ui_command_list_tracks_canvas_slides_switch_and_refuses_disabled_layer_actions() {
    use designllm_contracts::ErrorCode;

    fn paint_and_settle(recorder: &mut Recorder, host: &mut Host) {
        for _ in 0..3 {
            let _ = recorder.ctx.run(screen(1.0, 1280.0, 800.0), |ctx| {
                recorder.app.draw_frame(ctx);
            });
            recorder.app.publish_ui_if_due();
            recorder.settle(host);
        }
    }

    let mut host = Host::start();
    let canvas = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
    let slides = host.create("slides", "슬라이드");
    let mut recorder = Recorder::open(&mut host, &canvas);
    // The layer commands take the selection (backlog desktop 29): a box is selected so that only the kind decides here.
    recorder.step(&mut host, "select.node", json!({ "node_id": "a" })).expect("a box is selected");
    recorder.app.publish_ui_if_due();
    recorder.settle(&mut host);
    let view_id = recorder.app.ui.view_id.clone();
    let layer_commands = ["edit.lock", "edit.unlock", "edit.hide", "edit.show"];
    let find_commands = ["layer.find.next", "layer.find.previous"];
    let canvas_only_commands = ["canvas.open_url", "canvas.interaction.set", "canvas.interaction.remove"];
    let enabled = |list: &Value, id: &str| {
        list["commands"]
            .as_array()
            .and_then(|commands| commands.iter().find(|command| command["id"] == id))
            .and_then(|command| command["enabled"].as_bool())
            .unwrap_or(false)
    };

    let canvas_list = host.call("ui.command.list", json!({ "view_id": view_id.clone() })).expect("Canvas command list");
    let canvas_state_seq = canvas_list["state_seq"].as_u64().expect("Canvas state sequence");
    for id in layer_commands {
        assert!(enabled(&canvas_list, id), "{id} is enabled in Canvas");
    }
    for id in find_commands {
        assert!(enabled(&canvas_list, id), "{id} finds Canvas layers (D17.6)");
    }
    for id in canvas_only_commands {
        assert!(enabled(&canvas_list, id), "{id} is enabled in Canvas");
    }

    recorder.step(&mut host, "nav.open_project", json!({ "artifact_id": slides.clone() })).expect("Slides opened");
    paint_and_settle(&mut recorder, &mut host);
    let slides_list = host.call("ui.command.list", json!({ "view_id": view_id.clone() })).expect("Slides command list");
    let slides_state_seq = slides_list["state_seq"].as_u64().expect("Slides state sequence");
    assert!(slides_state_seq > canvas_state_seq, "the registry transition advances the view state");
    for id in layer_commands {
        assert!(!enabled(&slides_list, id), "{id} is disabled in Slides");
    }
    for id in find_commands {
        assert!(!enabled(&slides_list, id), "{id} is disabled in Slides");
    }
    for id in canvas_only_commands {
        assert!(!enabled(&slides_list, id), "{id} is disabled in Slides");
    }

    let calls_before = recorder.calls.len();
    assert!(recorder.step(&mut host, "edit.show", json!({})).is_err(), "local Slides command is refused");
    assert_eq!(recorder.calls.len(), calls_before, "the refused local command sends no core call");
    let refused = host
        .call(
            "ui.command.run",
            json!({
                "view_id": view_id.clone(),
                "expected_state_seq": slides_state_seq,
                "command_id": "edit.show",
                "actor": "test",
                "permission": "change",
                "allowed_intents": ["content"],
                "args": {},
            }),
        )
        .expect_err("Core refuses the disabled Slides command");
    assert_eq!(refused.code, ErrorCode::InvalidArgument, "{refused:?}");
    let calls_before = recorder.calls.len();
    assert!(recorder.step(&mut host, "layer.find.next", json!({})).is_err(), "local Slides search navigation is refused");
    assert_eq!(recorder.calls.len(), calls_before, "the refused search command sends no core call");
    assert!(
        host.call(
            "ui.command.run",
            json!({
                "view_id": view_id.clone(),
                "expected_state_seq": slides_state_seq,
                "command_id": "layer.find.next",
                "actor": "test",
                "permission": "read",
                "allowed_intents": [],
                "args": {},
            }),
        )
        .is_err(),
        "Core refuses disabled search navigation too"
    );
    let calls_before = recorder.calls.len();
    assert!(recorder.step(&mut host, "canvas.open_url", json!({ "url": "https://example.test/" })).is_err());
    assert!(recorder.step(&mut host, "canvas.interaction.set", json!({ "index": 0, "action": "back" })).is_err());
    assert!(recorder.step(&mut host, "canvas.interaction.remove", json!({ "index": 0 })).is_err());
    assert_eq!(recorder.calls.len(), calls_before, "Slides interaction editing is refused locally");
    assert!(
        host.call(
            "ui.command.run",
            json!({
                "view_id": view_id.clone(),
                "expected_state_seq": slides_state_seq,
                "command_id": "canvas.open_url",
                "actor": "test",
                "permission": "change",
                "allowed_intents": ["content"],
                "args": { "url": "https://example.test/" },
            }),
        )
        .is_err(),
        "Core refuses disabled Canvas URL editing in Slides"
    );

    recorder.step(&mut host, "nav.open_project", json!({ "artifact_id": canvas.clone() })).expect("Canvas reopened");
    recorder.step(&mut host, "select.node", json!({ "node_id": "a" })).expect("a box is selected again");
    paint_and_settle(&mut recorder, &mut host);
    let canvas_again = host.call("ui.command.list", json!({ "view_id": view_id })).expect("Canvas command list refreshed");
    assert!(canvas_again["state_seq"].as_u64().unwrap() > slides_state_seq);
    for id in layer_commands {
        assert!(enabled(&canvas_again, id), "{id} is enabled again in Canvas");
    }
    for id in find_commands {
        assert!(enabled(&canvas_again, id), "{id} is enabled again in Canvas");
    }
    for id in canvas_only_commands {
        assert!(enabled(&canvas_again, id), "{id} is enabled again in Canvas");
    }

    let document = document_with_paragraphs_and_table(&mut host);
    recorder.app.call("workspace.list", json!({}), super::PendingAction::WorkspaceList);
    recorder.settle(&mut host);
    recorder.step(&mut host, "nav.open_project", json!({ "artifact_id": document })).expect("Word document opened");
    paint_and_settle(&mut recorder, &mut host);
    let document_list = host.call("ui.command.list", json!({ "view_id": view_id })).expect("document command list");
    assert!(document_list["state_seq"].as_u64().unwrap() > canvas_again["state_seq"].as_u64().unwrap());
    for (id, shortcut) in [("layer.find.next", "F3"), ("layer.find.previous", "Shift+F3")] {
        let command = document_list["commands"].as_array().unwrap().iter().find(|command| command["id"] == id).expect("find command");
        assert_eq!(command["enabled"], true, "{id} is enabled for flowing documents");
        assert_eq!(command["shortcut"], shortcut);
    }
    for id in canvas_only_commands {
        assert!(!enabled(&document_list, id), "{id} is disabled for flowing documents");
    }
    recorder.app.layer_find_query = "첫째".into();
    recorder.app.sync_flow_find_search();
    let revision = recorder.app.active.as_ref().unwrap().revision;
    recorder.app.invoke("layer.find.next", json!({})).expect("next result command runs");
    recorder.settle(&mut host);
    assert_eq!(recorder.app.artifact.as_ref().unwrap().revision, revision, "finding does not edit the document");
}

fn node_interactions(host: &mut Host, artifact_id: &str, node_id: &str) -> Value {
    let artifact = host.artifact(artifact_id);
    serde_json::to_value(&super::doc_tree::find_node(&artifact, node_id).expect("the node exists").interactions).expect("interactions serialize")
}

#[test]
fn interaction_edit_commands_refuse_both_artifact_context_revision_mismatch_orders() {
    use designllm_contracts::{Intent, Interaction, Operation};

    let commands = [
        ("canvas.interaction.set", json!({ "index": 0, "action": "navigate_page", "target_id": "page-1" })),
        ("canvas.interaction.remove", json!({ "index": 0 })),
        ("canvas.open_url", json!({ "url": "https://example.test/" })),
    ];

    for artifact_is_stale in [true, false] {
        let mut host = Host::start();
        let canvas = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
        let page_id = match &host.artifact(&canvas).body {
            ArtifactBody::Canvas(document) => document.pages[0].id.clone(),
            _ => panic!("Canvas"),
        };
        host.apply(
            &canvas,
            vec![Operation::SetInteractions { node_id: "a".into(), interactions: vec![Interaction::Back] }],
            [Intent::Content].into_iter().collect(),
        )
        .expect("initial action");
        let mut recorder = Recorder::open(&mut host, &canvas);
        recorder.step(&mut host, "select.node", json!({ "node_id": "a" })).expect("source selected");
        let matching_revision = recorder.app.artifact.as_ref().unwrap().revision;
        assert_eq!(recorder.app.context.as_ref().unwrap().revision, matching_revision, "the starting snapshots match");

        if artifact_is_stale {
            recorder.app.artifact.as_mut().unwrap().revision -= 1;
        } else {
            recorder.app.context.as_mut().unwrap().revision -= 1;
        }
        recorder.app.select_after_commit = Some(vec!["pending-target".into()]);
        recorder.app.select_after_commit_ticket = Some(901);
        let selected = recorder.app.selected_nodes.clone();
        let primary = recorder.app.selected_node.clone();
        let before_revision = host.artifact(&canvas).revision;
        let before_actions = node_interactions(&mut host, &canvas, "a");

        for (command, mut args) in commands.clone() {
            if command == "canvas.interaction.set" {
                args["target_id"] = json!(page_id);
            }
            let calls_before = recorder.calls.len();
            let error = recorder.step(&mut host, command, args).expect_err("mismatched local snapshots refuse before reading actions");
            assert!(error.contains("최신 내용을"), "the UI asks the user to reload: {error}");
            assert_eq!(recorder.calls.len(), calls_before, "{command} sends no RPC");
            assert_eq!(host.artifact(&canvas).revision, before_revision, "{command} cannot change the Host revision");
            assert_eq!(node_interactions(&mut host, &canvas, "a"), before_actions, "{command} preserves the action list");
            assert_eq!(recorder.app.selected_nodes, selected, "{command} preserves selection");
            assert_eq!(recorder.app.selected_node, primary, "{command} preserves the primary selection");
            assert_eq!(recorder.app.select_after_commit, Some(vec!["pending-target".into()]));
            assert_eq!(recorder.app.select_after_commit_ticket, Some(901));
        }
    }
}

#[test]
fn interaction_edit_commands_work_when_artifact_and_context_revisions_match() {
    use designllm_contracts::{Intent, Interaction, Operation};

    let mut host = Host::start();
    let canvas = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
    let page_id = match &host.artifact(&canvas).body {
        ArtifactBody::Canvas(document) => document.pages[0].id.clone(),
        _ => panic!("Canvas"),
    };
    host.apply(
        &canvas,
        vec![Operation::SetInteractions { node_id: "a".into(), interactions: vec![Interaction::Back] }],
        [Intent::Content].into_iter().collect(),
    )
    .expect("initial action");
    let mut recorder = Recorder::open(&mut host, &canvas);
    recorder.step(&mut host, "select.node", json!({ "node_id": "a" })).expect("source selected");

    for (command, args) in [
        ("canvas.interaction.set", json!({ "index": 0, "action": "navigate_page", "target_id": page_id })),
        ("canvas.interaction.remove", json!({ "index": 0 })),
        ("canvas.open_url", json!({ "url": "https://example.test/" })),
    ] {
        let artifact_revision = recorder.app.artifact.as_ref().unwrap().revision;
        let context_revision = recorder.app.context.as_ref().unwrap().revision;
        assert_eq!(artifact_revision, context_revision, "the edit uses matching snapshots");
        recorder.step(&mut host, command, args).expect("matching snapshots allow the edit");
        assert_eq!(
            recorder.app.artifact.as_ref().unwrap().revision,
            recorder.app.context.as_ref().unwrap().revision,
            "the reply refreshes both revisions together"
        );
    }

    assert_eq!(node_interactions(&mut host, &canvas, "a"), json!([{ "action": "open_url", "url": "https://example.test/" }]));
}

fn queue_ui_content_run(recorder: &mut Recorder, host: &mut Host, command_id: &str, args: Value) -> (String, u64) {
    queue_ui_content_run_with_timeout(recorder, host, command_id, args, None)
}

fn queue_ui_content_run_with_timeout(
    recorder: &mut Recorder,
    host: &mut Host,
    command_id: &str,
    args: Value,
    timeout_ms: Option<u64>,
) -> (String, u64) {
    recorder.app.publish_ui_if_due();
    recorder.settle(host);
    let view_id = recorder.app.ui.view_id.clone();
    let command_list = host.call("ui.command.list", json!({ "view_id": view_id.clone() })).expect("the live Canvas registry");
    let state_seq = command_list["state_seq"].as_u64().expect("published state sequence");
    let command_enabled = command_list["commands"].as_array().unwrap().iter().any(|command| command["id"] == command_id && command["enabled"] == true);
    let table_commands: Vec<_> = command_list["commands"].as_array().unwrap().iter().filter(|command| command["id"].as_str().is_some_and(|id| id.starts_with("canvas.table."))).collect();
    assert!(
        command_enabled,
        "the {command_id} command is enabled when queued; selection={:?}/{:?} context={:?} active={:?} artifact={:?} view_page={} context_current={} selected_table={:?} table_capability={:?} table_commands={:?}",
        recorder.app.selected_node,
        recorder.app.selected_nodes,
        recorder.app.context.as_ref().map(|context| (context.artifact_id.as_str(), context.revision, context.visible_page, context.selection.clone())),
        recorder.app.active.as_ref().map(|active| (active.id.as_str(), active.revision)),
        recorder.app.artifact.as_ref().map(|artifact| (artifact.id.as_str(), artifact.revision)),
        recorder.app.view_ref().page_index,
        recorder.app.canvas_table_context_current(),
        recorder.app.selected_canvas_table_node().map(|node| node.id.as_str()),
        recorder.app.context.as_ref().and_then(|context| context.capabilities.get(&designllm_contracts::Feature::Tables)),
        table_commands,
    );
    let mut request = json!({
        "view_id": view_id,
        "expected_state_seq": state_seq,
        "command_id": command_id,
        "actor": "stale-run-test",
        "permission": "change",
        "allowed_intents": ["content"],
        "args": args,
    });
    if let Some(timeout_ms) = timeout_ms {
        request["timeout_ms"] = json!(timeout_ms);
    }
    let queued = host
        .call(
            "ui.command.run",
            request,
        )
        .expect("Core accepts the run for the published state");
    (queued["run_id"].as_str().expect("queued run id").to_owned(), state_seq)
}

fn queue_ui_design_run(recorder: &mut Recorder, host: &mut Host, command_id: &str, args: Value) -> (String, u64) {
    recorder.app.publish_ui_if_due();
    recorder.settle(host);
    let view_id = recorder.app.ui.view_id.clone();
    let command_list = host.call("ui.command.list", json!({ "view_id": view_id.clone() })).expect("the live Canvas registry");
    let state_seq = command_list["state_seq"].as_u64().expect("published state sequence");
    assert!(
        command_list["commands"].as_array().unwrap().iter().any(|command| command["id"] == command_id && command["enabled"] == true),
        "the {command_id} design command is enabled"
    );
    let queued = host
        .call(
            "ui.command.run",
            json!({
                "view_id": view_id,
                "expected_state_seq": state_seq,
                "command_id": command_id,
                "actor": "canvas-token-test",
                "permission": "change",
                "allowed_intents": ["design"],
                "args": args,
            }),
        )
        .expect("Core accepts the design command run");
    (queued["run_id"].as_str().expect("queued run id").to_owned(), state_seq)
}


fn queue_ui_selection_run(recorder: &mut Recorder, host: &mut Host, command_id: &str) -> (String, u64) {
    recorder.app.publish_ui_if_due();
    recorder.settle(host);
    let view_id = recorder.app.ui.view_id.clone();
    let command_list = host.call("ui.command.list", json!({ "view_id": view_id.clone() })).expect("the live Canvas registry");
    let state_seq = command_list["state_seq"].as_u64().expect("published state sequence");
    assert!(
        command_list["commands"].as_array().unwrap().iter().any(|command| command["id"] == command_id && command["enabled"] == true),
        "{command_id} is enabled while this page has layers"
    );
    let queued = host
        .call(
            "ui.command.run",
            json!({
                "view_id": view_id,
                "expected_state_seq": state_seq,
                "command_id": command_id,
                "actor": "state-bound-selection-test",
                "permission": "read",
                "allowed_intents": [],
                "args": {},
            }),
        )
        .expect("Core accepts the state-bound selection run");
    (queued["run_id"].as_str().expect("queued run id").to_owned(), state_seq)
}

fn queue_ui_file_save_run(
    recorder: &mut Recorder,
    host: &mut Host,
    permission: &str,
    args: Value,
) -> Result<(String, u64), designllm_contracts::DltError> {
    recorder.app.publish_ui_if_due();
    recorder.settle(host);
    let view_id = recorder.app.ui.view_id.clone();
    let command_list = host.call("ui.command.list", json!({ "view_id": view_id.clone() })).expect("the live editor registry");
    let state_seq = command_list["state_seq"].as_u64().expect("published state sequence");
    assert!(
        command_list["commands"].as_array().unwrap().iter().any(|command| command["id"] == "file.save" && command["enabled"] == true),
        "native save is enabled for this artifact"
    );
    let queued = host.call(
        "ui.command.run",
        json!({
            "view_id": view_id,
            "expected_state_seq": state_seq,
            "command_id": "file.save",
            "actor": "explicit-save-test",
            "permission": permission,
            "allowed_intents": [],
            "args": args,
        }),
    )?;
    Ok((queued["run_id"].as_str().expect("queued save run id").to_owned(), state_seq))
}

fn queue_ui_file_export_run(
    recorder: &mut Recorder,
    host: &mut Host,
    permission: &str,
    args: Value,
) -> Result<(String, u64), designllm_contracts::DltError> {
    recorder.app.publish_ui_if_due();
    recorder.settle(host);
    let view_id = recorder.app.ui.view_id.clone();
    let command_list = host.call("ui.command.list", json!({ "view_id": view_id.clone() })).expect("the live editor registry");
    let state_seq = command_list["state_seq"].as_u64().expect("published state sequence");
    assert!(
        command_list["commands"].as_array().unwrap().iter().any(|command| command["id"] == "file.export" && command["enabled"] == true),
        "the explicit export command is enabled for this artifact"
    );
    let queued = host.call(
        "ui.command.run",
        json!({
            "view_id": view_id,
            "expected_state_seq": state_seq,
            "command_id": "file.export",
            "actor": "explicit-export-test",
            "permission": permission,
            "allowed_intents": [],
            "args": args,
        }),
    )?;
    Ok((queued["run_id"].as_str().expect("queued export run id").to_owned(), state_seq))
}

fn start_job_for_cancel(host: &mut Host, artifact_id: &str) -> designllm_contracts::Job {
    let revision = host.artifact(artifact_id).revision;
    serde_json::from_value(
        host.call(
            "job.start",
            json!({
                "request_id": designllm_contracts::new_id(),
                "artifact_id": artifact_id,
                "base_revision": revision,
                "allowed_intents": ["content"],
                "runtime": "mock",
                "model": "cancel-test",
                "effort": "low",
                "prompt": "cancel test job",
            }),
        )
        .expect("the test job starts in the Core Host"),
    )
    .expect("a Job response parses")
}

fn queue_ui_cancel_run(
    recorder: &mut Recorder,
    host: &mut Host,
    job: &designllm_contracts::Job,
    actor: &str,
) -> (String, u64) {
    recorder.app.job = Some(job.clone());
    let events = host.call("events.poll", json!({ "after_seq": 0, "limit": 1000 })).expect("current event sequence");
    recorder.app.last_event_seq = events["last_seq"].as_u64().expect("event sequence");
    recorder.app.publish_ui_if_due();
    recorder.settle(host);

    let view_id = recorder.app.ui.view_id.as_str();
    let list = host.call("ui.command.list", json!({ "view_id": view_id })).expect("editor command list");
    assert!(
        list["commands"].as_array().unwrap().iter().any(|command| command["id"] == "ai.cancel" && command["enabled"] == true),
        "a cancellable job in the active editor enables ai.cancel"
    );
    let state = host.call("ui.state.read", json!({ "view_id": view_id })).expect("the published job state");
    assert_eq!(state["state"]["detail"]["job_id"], job.id.as_str());
    assert_eq!(state["state"]["detail"]["job_attempt_id"], job.attempt_id.as_str());
    let state_seq = list["state_seq"].as_u64().expect("published state sequence");
    let queued = host
        .call(
            "ui.command.run",
            json!({
                "view_id": view_id,
                "expected_state_seq": state_seq,
                "command_id": "ai.cancel",
                "actor": actor,
                "permission": "change",
                "allowed_intents": ["content"],
                "args": { "job_id": job.id.as_str(), "attempt_id": job.attempt_id.as_str() },
            }),
        )
        .expect("the state-bound cancel run is queued");
    (queued["run_id"].as_str().expect("run id").to_owned(), state_seq)
}

#[test]
fn queued_ai_cancel_waits_for_core_and_returns_the_canceled_job() {
    let mut host = Host::start();
    let canvas = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
    let job = start_job_for_cancel(&mut host, &canvas);
    let mut recorder = Recorder::open(&mut host, &canvas);
    let (run_id, _) = queue_ui_cancel_run(&mut recorder, &mut host, &job, "cancel-agent");
    let queued = host.call("ui.command.get", json!({ "run_id": run_id })).expect("the run is queryable");
    assert_eq!(queued["actor"], "cancel-agent", "the caller identity stays on the UI run");

    let requests = deliver_ui_runs_until_rpc(&mut recorder, &mut host, "job.cancel");
    assert_eq!(requests.len(), 1);
    let (ticket, params) = requests.into_iter().next().unwrap();
    assert_eq!(params["job_id"], job.id);
    assert_eq!(params["attempt_id"], job.attempt_id);
    let waiting = host.call("ui.command.get", json!({ "run_id": run_id })).expect("the run waits for job.cancel");
    assert_eq!(waiting["status"], "delivered", "the GUI does not report success before Core replies");

    let result = host.call("job.cancel", params);
    assert_eq!(result.as_ref().unwrap()["status"], "canceled");
    answer_ui_apply_reply(&mut recorder, &mut host, ticket, result);
    let finished = host.call("ui.command.get", json!({ "run_id": run_id })).expect("the finished run is queryable");
    assert_eq!(finished["status"], "completed");
    assert_eq!(finished["actor"], "cancel-agent");
    assert_eq!(finished["result"]["id"], job.id);
    assert_eq!(finished["result"]["attempt_id"], job.attempt_id);
    assert_eq!(finished["result"]["status"], "canceled");

    let direct_job = start_job_for_cancel(&mut host, &canvas);
    let direct_job_id = direct_job.id.clone();
    recorder.app.job = Some(direct_job);
    recorder.step(&mut host, "ai.cancel", json!({})).expect("the existing GUI cancel button needs no args");
    assert_eq!(
        host.call("job.get", json!({ "job_id": direct_job_id })).unwrap()["status"],
        "canceled",
        "the GUI path still cancels the job currently shown"
    );
}

#[test]
fn queued_ai_cancel_refuses_a_job_attempt_that_changed_before_delivery() {
    let mut host = Host::start();
    let canvas = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
    let job = start_job_for_cancel(&mut host, &canvas);
    let mut recorder = Recorder::open(&mut host, &canvas);
    let (run_id, _) = queue_ui_cancel_run(&mut recorder, &mut host, &job, "cancel-agent");

    host.set_job_status_for_test(&job.id, &job.attempt_id, designllm_contracts::JobStatus::Running, None).expect("the runtime starts");
    host.set_job_status_for_test(
        &job.id,
        &job.attempt_id,
        designllm_contracts::JobStatus::Failed,
        Some(designllm_contracts::DltError::new(designllm_contracts::ErrorCode::Internal, "test failure")),
    )
    .expect("the test attempt fails");
    let resumed = host.call("job.resume", json!({ "job_id": &job.id })).expect("Core creates a new attempt");
    assert_ne!(resumed["attempt_id"], job.attempt_id);
    recorder.app.job = Some(serde_json::from_value(resumed).expect("resumed job response"));

    let calls_before = recorder.calls.len();
    poll_queued_ui_runs(&mut recorder, &mut host);
    assert!(!recorder.calls[calls_before..].iter().any(|call| call == "job.cancel"), "the stale attempt never reaches Core");
    let finished = host.call("ui.command.get", json!({ "run_id": run_id })).expect("the refused run is queryable");
    assert_eq!(finished["status"], "failed");
    let refusal: designllm_contracts::DltError = serde_json::from_value(finished["error"].clone()).expect("attempt refusal");
    assert_eq!(refusal.code, designllm_contracts::ErrorCode::RevisionConflict);
    let current = host.call("job.get", json!({ "job_id": &job.id })).expect("current job");
    assert_ne!(current["attempt_id"], job.attempt_id);
    assert_eq!(current["status"], "queued", "the newer attempt is not canceled");
    assert_eq!(finished["actor"], "cancel-agent");
}

#[test]
fn queued_ai_cancel_reports_a_core_attempt_race_as_failed() {
    let mut host = Host::start();
    let canvas = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
    let job = start_job_for_cancel(&mut host, &canvas);
    let mut recorder = Recorder::open(&mut host, &canvas);
    let (run_id, _) = queue_ui_cancel_run(&mut recorder, &mut host, &job, "cancel-agent");

    let requests = deliver_ui_runs_until_rpc(&mut recorder, &mut host, "job.cancel");
    assert_eq!(requests.len(), 1);
    let (ticket, params) = requests.into_iter().next().unwrap();

    host.set_job_status_for_test(&job.id, &job.attempt_id, designllm_contracts::JobStatus::Running, None).expect("the runtime starts");
    host.set_job_status_for_test(
        &job.id,
        &job.attempt_id,
        designllm_contracts::JobStatus::Failed,
        Some(designllm_contracts::DltError::new(designllm_contracts::ErrorCode::Internal, "test failure")),
    )
    .expect("the test attempt fails");
    let resumed = host.call("job.resume", json!({ "job_id": &job.id })).expect("Core creates a new attempt");
    assert_ne!(resumed["attempt_id"], job.attempt_id);

    let result = host.call("job.cancel", params);
    assert_eq!(result.as_ref().unwrap_err().code, designllm_contracts::ErrorCode::JobNotActive);
    answer_ui_apply_reply(&mut recorder, &mut host, ticket, result);
    let finished = host.call("ui.command.get", json!({ "run_id": run_id })).expect("the failed run is queryable");
    assert_eq!(finished["status"], "failed");
    assert_eq!(finished["actor"], "cancel-agent");
    let refusal: designllm_contracts::DltError = serde_json::from_value(finished["error"].clone()).expect("Core attempt refusal");
    assert_eq!(refusal.code, designllm_contracts::ErrorCode::JobNotActive);
    let current = host.call("job.get", json!({ "job_id": &job.id })).expect("current job");
    assert_eq!(current["attempt_id"], resumed["attempt_id"]);
    assert_eq!(current["status"], "queued", "a raced old-attempt cancellation leaves the new attempt untouched");
}


#[test]
fn content_only_queued_undo_cannot_revert_a_data_change() {
    use designllm_contracts::{ChangeSet, Intent, Operation};

    for (command, rpc_method) in [("edit.undo", "history.undo"), ("ai.undo_change", "history.undo_change")] {
        let mut host = Host::start();
        let figure = host.create("figure", "되돌림 권한");
        let change_id = designllm_contracts::new_id();
        let args = if command == "ai.undo_change" { json!({ "change_id": change_id.clone() }) } else { json!({}) };
        let dataset = host.import_table("value\n1\n");
        let before = host.artifact(&figure);
        host.call(
            "changes.apply",
            serde_json::to_value(ChangeSet {
                request_id: change_id,
                actor: "seed".into(),
                workspace_id: host.workspace_id.clone(),
                artifact_id: figure.clone(),
                base_revision: before.revision,
                allowed_intents: [Intent::Data].into_iter().collect(),
                job_ref: None,
                read_dependencies: Vec::new(),
                operations: vec![Operation::PutDataset { dataset }],
            })
            .expect("a dataset ChangeSet serializes"),
        )
        .expect("the data change is committed");
        let data_revision = host.artifact(&figure).revision;
        let mut recorder = Recorder::open(&mut host, &figure);
        let (run_id, _) = queue_ui_content_run(&mut recorder, &mut host, command, args);

        let requests = deliver_ui_runs_until_rpc(&mut recorder, &mut host, rpc_method);
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].1["actor"], "stale-run-test", "the run actor is preserved");
        assert_eq!(requests[0].1["allowed_intents"], json!(["content"]), "the grant is not widened to design or data");
        let result = host.call(rpc_method, requests[0].1.clone());
        assert_eq!(result.as_ref().unwrap_err().code, designllm_contracts::ErrorCode::ContentLocked);
        answer_ui_apply_reply(&mut recorder, &mut host, requests[0].0, result);

        let finished = host.call("ui.command.get", json!({ "run_id": run_id })).expect("the refusal is recorded");
        assert_eq!(finished["status"], "failed");
        let refusal: designllm_contracts::DltError = serde_json::from_value(finished["error"].clone()).expect("Core intent refusal");
        assert_eq!(refusal.code, designllm_contracts::ErrorCode::ContentLocked);
        let current = host.artifact(&figure);
        assert_eq!(current.revision, data_revision, "the latest Data change is not undone");
        let designllm_contracts::ArtifactBody::Figure(document) = current.body else { panic!("Figure") };
        assert_eq!(document.datasets.len(), 1, "the dataset stays in the document");
    }
}

#[test]
fn content_only_queued_undo_cannot_revert_a_design_change() {
    use designllm_contracts::{ChangeSet, Intent, Operation, StylePatch};

    for (command, rpc_method) in [("edit.undo", "history.undo"), ("ai.undo_change", "history.undo_change")] {
        let mut host = Host::start();
        let canvas = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
        let change_id = designllm_contracts::new_id();
        let args = if command == "ai.undo_change" { json!({ "change_id": change_id.clone() }) } else { json!({}) };
        let before = host.artifact(&canvas);
        host.call(
            "changes.apply",
            serde_json::to_value(ChangeSet {
                request_id: change_id,
                actor: "seed".into(),
                workspace_id: host.workspace_id.clone(),
                artifact_id: canvas.clone(),
                base_revision: before.revision,
                allowed_intents: [Intent::Design].into_iter().collect(),
                job_ref: None,
                read_dependencies: Vec::new(),
                operations: vec![Operation::SetStyle {
                    node_ids: vec!["a".into()],
                    patch: StylePatch { fill: Some("#2563EB".into()), ..Default::default() },
                }],
            })
            .expect("a design ChangeSet serializes"),
        )
        .expect("the design change is committed");
        let design_revision = host.artifact(&canvas).revision;
        let mut recorder = Recorder::open(&mut host, &canvas);
        let (run_id, _) = queue_ui_content_run(&mut recorder, &mut host, command, args);

        let requests = deliver_ui_runs_until_rpc(&mut recorder, &mut host, rpc_method);
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].1["actor"], "stale-run-test");
        assert_eq!(requests[0].1["allowed_intents"], json!(["content"]));
        let result = host.call(rpc_method, requests[0].1.clone());
        assert_eq!(result.as_ref().unwrap_err().code, designllm_contracts::ErrorCode::ContentLocked);
        answer_ui_apply_reply(&mut recorder, &mut host, requests[0].0, result);

        let finished = host.call("ui.command.get", json!({ "run_id": run_id })).expect("the refusal is recorded");
        assert_eq!(finished["status"], "failed");
        let refusal: designllm_contracts::DltError = serde_json::from_value(finished["error"].clone()).expect("Core intent refusal");
        assert_eq!(refusal.code, designllm_contracts::ErrorCode::ContentLocked);
        let current = host.artifact(&canvas);
        assert_eq!(current.revision, design_revision, "the latest Design change is not undone");
        assert_eq!(super::doc_tree::find_node(&current, "a").unwrap().style.fill, "#2563EB");
    }
}

#[test]
fn direct_gui_undo_keeps_its_existing_full_intent_grant() {
    use designllm_contracts::{Intent, Operation};

    let mut host = Host::start();
    let figure = host.create("figure", "직접 실행 취소");
    let dataset = host.import_table("value\n1\n");
    host.apply(&figure, vec![Operation::PutDataset { dataset }], [Intent::Data].into_iter().collect())
        .expect("the data change is committed");
    let revision = host.artifact(&figure).revision;
    let mut recorder = Recorder::open(&mut host, &figure);

    recorder.step(&mut host, "edit.undo", json!({})).expect("direct GUI undo succeeds");

    let artifact = host.artifact(&figure);
    assert_eq!(artifact.revision, revision + 1);
    let designllm_contracts::ArtifactBody::Figure(document) = artifact.body else { panic!("Figure") };
    assert!(document.datasets.is_empty(), "the direct user undo keeps its existing Data authority");
}

fn queue_canvas_interaction_run(recorder: &mut Recorder, host: &mut Host, args: Value) -> (String, u64) {
    queue_ui_content_run(recorder, host, "canvas.interaction.set", args)
}

fn answer_recorded_work(recorder: &mut Recorder, host: &mut Host, work: Vec<Work>) {
    for item in work {
        if let Work::Call { ticket, method, params } = item {
            if method == "changes.apply" {
                for operation in params["operations"].as_array().into_iter().flatten() {
                    recorder.calls.push(format!("changes.apply:{}", operation["op"].as_str().unwrap_or("?")));
                }
            } else {
                recorder.calls.push(method.clone());
            }
            let result = host.call(&method, params).map(RpcPayload::Json);
            recorder.replies.send(RpcReply { ticket, result }).expect("the window still listens");
        }
    }
    recorder.app.process_replies(&recorder.ctx);
}

fn settle_recorded_work(recorder: &mut Recorder, host: &mut Host) {
    for _ in 0..60 {
        let work: Vec<Work> = recorder.work.try_iter().collect();
        if work.is_empty() {
            return;
        }
        answer_recorded_work(recorder, host, work);
    }
    panic!("the window keeps asking for more host work");
}

fn deliver_ui_runs_until_apply(recorder: &mut Recorder, host: &mut Host) -> Vec<(u64, Value)> {
    deliver_ui_runs_until_rpc(recorder, host, "changes.apply")
}

fn deliver_ui_runs_until_rpc(recorder: &mut Recorder, host: &mut Host, expected_method: &str) -> Vec<(u64, Value)> {
    recorder.app.last_event_poll = std::time::Instant::now() - std::time::Duration::from_secs(10);
    recorder.app.poll_events();
    let poll_work: Vec<Work> = recorder.work.try_iter().collect();
    assert!(poll_work.iter().any(|item| matches!(item, Work::Call { method, .. } if method == "events.poll")));
    answer_recorded_work(recorder, host, poll_work);

    let command_work: Vec<Work> = recorder.work.try_iter().collect();
    let debug_calls: Vec<(String, Value)> = command_work.iter().filter_map(|item| match item { Work::Call { method, params, .. } => Some((method.clone(), params.clone())), _ => None }).collect();
    assert!(command_work.iter().all(|item| !matches!(item, Work::Call { method, .. } if method == "ui.command.complete")), "runs are not completed before changes.apply replies: {debug_calls:?}");
    let mut applies = Vec::new();
    for item in command_work {
        if let Work::Call { ticket, method, params } = item {
            if method == expected_method {
                applies.push((ticket, params));
            } else if method == "ui.command.complete" {
                panic!("a run completed before its {expected_method} reply");
            }
        }
    }
    assert!(!applies.is_empty(), "the queued run sends {expected_method}");
    applies
}

fn answer_ui_apply_reply(recorder: &mut Recorder, host: &mut Host, ticket: u64, result: Result<Value, designllm_contracts::DltError>) {
    recorder
        .replies
        .send(RpcReply { ticket, result: result.map(RpcPayload::Json) })
        .expect("the window still listens");
    recorder.app.process_replies(&recorder.ctx);
    let followup: Vec<Work> = recorder.work.try_iter().collect();
    assert!(followup.iter().any(|item| matches!(item, Work::Call { method, .. } if method == "ui.command.complete")), "UiRun completion follows the changes.apply reply");
    answer_recorded_work(recorder, host, followup);
    settle_recorded_work(recorder, host);
}

fn poll_queued_ui_runs(recorder: &mut Recorder, host: &mut Host) {
    recorder.app.last_event_poll = std::time::Instant::now() - std::time::Duration::from_secs(10);
    recorder.app.poll_events();
    recorder.settle(host);
}

#[test]
fn a_queued_interaction_run_does_not_follow_a_changed_selection() {
    use designllm_contracts::{Interaction, Intent, Operation};

    let mut host = Host::start();
    let canvas = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
    let page_id = match &host.artifact(&canvas).body {
        ArtifactBody::Canvas(document) => document.pages[0].id.clone(),
        _ => panic!("Canvas"),
    };
    host.apply(
        &canvas,
        vec![
            Operation::SetInteractions { node_id: "a".into(), interactions: vec![Interaction::Back] },
            Operation::SetInteractions { node_id: "b".into(), interactions: vec![Interaction::ToggleVisibility { node_id: "a".into() }] },
        ],
        [Intent::Content].into_iter().collect(),
    )
    .expect("initial actions");
    let mut recorder = Recorder::open(&mut host, &canvas);
    recorder.step(&mut host, "select.node", json!({ "node_id": "a" })).expect("the published source is selected");
    let (run_id, queued_state_seq) = queue_canvas_interaction_run(
        &mut recorder,
        &mut host,
        json!({ "index": 0, "action": "navigate_page", "target_id": page_id }),
    );
    let before_a = node_interactions(&mut host, &canvas, "a");
    let before_b = node_interactions(&mut host, &canvas, "b");

    recorder.step(&mut host, "select.node", json!({ "node_id": "b" })).expect("selection changes while the run waits");
    recorder.app.publish_ui_if_due();
    recorder.settle(&mut host);
    let new_state = host.call("ui.command.list", json!({ "view_id": recorder.app.ui.view_id.clone() })).expect("updated view state");
    assert!(new_state["state_seq"].as_u64().unwrap() > queued_state_seq, "the changed selection is published before delivery");
    poll_queued_ui_runs(&mut recorder, &mut host);

    assert_eq!(node_interactions(&mut host, &canvas, "a"), before_a, "the queued command must not use its old index on the old node");
    assert_eq!(node_interactions(&mut host, &canvas, "b"), before_b, "the queued command must not be retargeted to the new selection");
    let finished = host.call("ui.command.get", json!({ "run_id": run_id })).expect("the run has a result");
    assert_eq!(finished["status"], "failed", "the stale run should complete with a refusal");
    assert!(finished["error"]["message"].as_str().unwrap().contains("화면 상태가 바뀌었거나"), "state-bound guard refuses the stale selection");
    let refusal: designllm_contracts::DltError = serde_json::from_value(finished["error"].clone()).expect("a run refusal");
    assert_eq!(refusal.code, designllm_contracts::ErrorCode::RevisionConflict);
}

#[test]
fn a_queued_interaction_run_does_not_reuse_an_index_after_the_action_list_changes() {
    use designllm_contracts::{Interaction, Intent, Operation};

    let mut host = Host::start();
    let canvas = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
    let page_id = match &host.artifact(&canvas).body {
        ArtifactBody::Canvas(document) => document.pages[0].id.clone(),
        _ => panic!("Canvas"),
    };
    host.apply(
        &canvas,
        vec![Operation::SetInteractions {
            node_id: "a".into(),
            interactions: vec![Interaction::Back, Interaction::OpenUrl { url: "https://keep.example.test/".into() }],
        }],
        [Intent::Content].into_iter().collect(),
    )
    .expect("initial action list");
    let revision_before_queue = host.artifact(&canvas).revision;
    let mut recorder = Recorder::open(&mut host, &canvas);
    recorder.step(&mut host, "select.node", json!({ "node_id": "a" })).expect("source selected");
    let (run_id, queued_state_seq) = queue_canvas_interaction_run(
        &mut recorder,
        &mut host,
        json!({ "index": 0, "action": "navigate_page", "target_id": page_id }),
    );

    recorder.step(&mut host, "canvas.interaction.remove", json!({ "index": 0 })).expect("the user removes the old action while the run waits");
    let after_user_edit = node_interactions(&mut host, &canvas, "a");
    assert_eq!(after_user_edit, json!([{ "action": "open_url", "url": "https://keep.example.test/" }]));
    let revision_after_user_edit = host.artifact(&canvas).revision;
    assert!(revision_after_user_edit > revision_before_queue, "the user's action-list edit advances the artifact revision");
    recorder.app.publish_ui_if_due();
    recorder.settle(&mut host);
    let new_state = host.call("ui.command.list", json!({ "view_id": recorder.app.ui.view_id.clone() })).expect("updated revision state");
    assert!(new_state["state_seq"].as_u64().unwrap() > queued_state_seq, "the new action-list revision is published before delivery");
    poll_queued_ui_runs(&mut recorder, &mut host);

    assert_eq!(node_interactions(&mut host, &canvas, "a"), after_user_edit, "the OpenUrl that moved to index 0 must survive");
    assert_eq!(host.artifact(&canvas).revision, revision_after_user_edit, "the stale index command must not add a revision");
    let finished = host.call("ui.command.get", json!({ "run_id": run_id })).expect("the run has a result");
    assert_eq!(finished["status"], "failed", "the stale run should complete with a refusal");
    assert!(finished["error"]["message"].as_str().unwrap().contains("화면 상태가 바뀌었거나"), "state-bound guard refuses the stale revision");
    let refusal: designllm_contracts::DltError = serde_json::from_value(finished["error"].clone()).expect("a run refusal");
    assert_eq!(refusal.code, designllm_contracts::ErrorCode::RevisionConflict);
}

#[test]
fn queued_interaction_set_and_remove_complete_after_their_apply_replies() {
    use designllm_contracts::{Intent, Interaction, Operation};

    let mut host = Host::start();
    let canvas = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
    let page_id = match &host.artifact(&canvas).body {
        ArtifactBody::Canvas(document) => document.pages[0].id.clone(),
        _ => panic!("Canvas"),
    };
    host.apply(
        &canvas,
        vec![Operation::SetInteractions { node_id: "a".into(), interactions: vec![Interaction::Back] }],
        [Intent::Content].into_iter().collect(),
    )
    .expect("initial action");
    let mut recorder = Recorder::open(&mut host, &canvas);
    recorder.step(&mut host, "select.node", json!({ "node_id": "a" })).expect("source selected");

    let (set_run, _) = queue_ui_content_run(
        &mut recorder,
        &mut host,
        "canvas.interaction.set",
        json!({ "index": 0, "action": "navigate_page", "target_id": page_id }),
    );
    let applies = deliver_ui_runs_until_apply(&mut recorder, &mut host);
    assert_eq!(applies.len(), 1);
    let set_result = host.call("changes.apply", applies[0].1.clone());
    assert!(set_result.is_ok());
    answer_ui_apply_reply(&mut recorder, &mut host, applies[0].0, set_result);
    assert_eq!(host.call("ui.command.get", json!({ "run_id": set_run })).unwrap()["status"], "completed");
    assert_eq!(node_interactions(&mut host, &canvas, "a")[0]["action"], "navigate_page");

    let (remove_run, _) = queue_ui_content_run(&mut recorder, &mut host, "canvas.interaction.remove", json!({ "index": 0 }));
    let applies = deliver_ui_runs_until_apply(&mut recorder, &mut host);
    assert_eq!(applies.len(), 1);
    let remove_result = host.call("changes.apply", applies[0].1.clone());
    assert!(remove_result.is_ok());
    answer_ui_apply_reply(&mut recorder, &mut host, applies[0].0, remove_result);
    assert_eq!(host.call("ui.command.get", json!({ "run_id": remove_run })).unwrap()["status"], "completed");
    assert_eq!(node_interactions(&mut host, &canvas, "a"), json!([]));
}

#[test]
fn queued_canvas_url_run_completes_only_after_the_host_change_reply() {
    let mut host = Host::start();
    let canvas = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
    let mut recorder = Recorder::open(&mut host, &canvas);
    recorder.step(&mut host, "select.node", json!({ "node_id": "a" })).expect("source selected");
    let (run_id, _) = queue_ui_content_run(
        &mut recorder,
        &mut host,
        "canvas.open_url",
        json!({ "url": "https://queued.example.test/" }),
    );

    let before_revision = host.artifact(&canvas).revision;
    let applies = deliver_ui_runs_until_apply(&mut recorder, &mut host);
    assert_eq!(applies.len(), 1);
    assert_eq!(applies[0].1["operations"][0]["op"], "set_interactions");
    let waiting = host.call("ui.command.get", json!({ "run_id": run_id })).expect("the run remains queryable while apply waits");
    assert_eq!(waiting["status"], "delivered", "it is not completed when changes.apply is merely enqueued");
    assert_eq!(host.artifact(&canvas).revision, before_revision, "the Host has not processed the held apply request");

    let result = host.call("changes.apply", applies[0].1.clone());
    assert!(result.is_ok(), "the same changes.apply succeeds");
    answer_ui_apply_reply(&mut recorder, &mut host, applies[0].0, result);
    let finished = host.call("ui.command.get", json!({ "run_id": run_id })).expect("the completed run is queryable");
    assert_eq!(finished["status"], "completed");
    assert_eq!(finished["result"]["command"], "canvas.open_url");
    assert!(finished["error"].is_null());
    assert_eq!(node_interactions(&mut host, &canvas, "a"), json!([{ "action": "open_url", "url": "https://queued.example.test/" }]));
}

#[test]
fn queued_text_list_run_completes_only_after_the_host_change_reply() {
    let mut host = Host::start();
    let canvas = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
    let mut recorder = Recorder::open(&mut host, &canvas);
    assert!(recorder.app.canvas_list_support().is_some(), "Canvas publishes a list extension");
    recorder.step(&mut host, "select.node", json!({ "node_id": "a" })).expect("text node selected");
    let (run_id, _) = queue_ui_content_run(&mut recorder, &mut host, "text.list", json!({ "list": "bullet" }));

    let applies = deliver_ui_runs_until_apply(&mut recorder, &mut host);
    assert_eq!(applies.len(), 1);
    assert_eq!(applies[0].1["operations"][0]["op"], "set_node_list");
    let waiting = host.call("ui.command.get", json!({ "run_id": run_id })).expect("the run remains queryable while apply waits");
    assert_eq!(waiting["status"], "delivered");

    let result = host.call("changes.apply", applies[0].1.clone());
    assert!(result.is_ok(), "the text list ChangeSet succeeds");
    answer_ui_apply_reply(&mut recorder, &mut host, applies[0].0, result);
    let finished = host.call("ui.command.get", json!({ "run_id": run_id })).expect("the completed run is queryable");
    assert_eq!(finished["status"], "completed");
    assert_eq!(finished["result"]["command"], "text.list");
    assert!(finished["error"].is_null());
    let artifact = host.artifact(&canvas);
    let node = super::doc_tree::find_node(&artifact, "a").expect("the text node remains");
    assert_eq!(serde_json::to_value(&node.ext.list).unwrap(), json!("bullet"));
}

#[test]
fn an_agent_run_with_null_for_an_optional_field_completes_like_one_without_it() {
    let mut host = Host::start();
    let canvas = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
    let mut recorder = Recorder::open(&mut host, &canvas);
    recorder.step(&mut host, "select.many", json!({ "node_ids": ["a", "b", "c"] })).expect("the boxes are selected");
    // The agent leaves width_mode out by sending null for it; the run must not refuse the null.
    let (run_id, _) = queue_ui_design_run(&mut recorder, &mut host, "props.size_mode", json!({ "width_mode": null, "height_mode": "fixed" }));

    let applies = deliver_ui_runs_until_apply(&mut recorder, &mut host);
    assert_eq!(applies.len(), 1);
    assert!(applies[0].1["operations"].as_array().is_some_and(|operations| operations.iter().all(|op| op["op"] == "set_layout")));
    let result = host.call("changes.apply", applies[0].1.clone());
    assert!(result.is_ok(), "the layout ChangeSet succeeds");
    answer_ui_apply_reply(&mut recorder, &mut host, applies[0].0, result);
    let finished = host.call("ui.command.get", json!({ "run_id": run_id })).expect("the completed run is queryable");
    assert_eq!(finished["status"], "completed");
    assert!(finished["error"].is_null());
}

#[test]
fn null_optional_save_and_export_arguments_are_absent_for_the_handlers() {
    let mut host = Host::start();
    let canvas = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
    let mut recorder = Recorder::open(&mut host, &canvas);
    let saved = std::env::temp_dir().join(format!("designllm-null-save-{}.canvas", designllm_contracts::new_id()));
    let saved_text = saved.to_string_lossy().into_owned();
    // The agent sends null for each optional field it leaves out: the handlers must read those fields as absent.
    recorder
        .step(&mut host, "file.save", json!({ "target_path": saved_text, "overwrite": null, "expected_target_hash": null }))
        .expect("a null overwrite and a null expected hash are absent");
    assert!(saved.is_file(), "Core writes the explicit target");
    let exported = std::env::temp_dir().join(format!("designllm-null-export-{}.pdf", designllm_contracts::new_id()));
    let exported_text = exported.to_string_lossy().into_owned();
    recorder
        .step(
            &mut host,
            "file.export",
            json!({
                "format": "pdf",
                "target_path": exported_text,
                "overwrite": null,
                "expected_target_hash": null,
                "allowed_losses": null,
                "scale": null,
                "background": null,
                "scope": null,
                "pages": null,
            }),
        )
        .expect("null optional export settings are absent");
    assert!(exported.is_file(), "Core writes the export");
    let _ = std::fs::remove_file(&saved);
    let _ = std::fs::remove_file(&exported);
}

#[test]
fn null_allowed_losses_in_an_agent_export_run_is_absent_as_in_the_window() {
    let mut host = Host::start();
    let canvas = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
    let mut recorder = Recorder::open(&mut host, &canvas);
    let window_target = std::env::temp_dir().join(format!("designllm-null-losses-window-{}.pdf", designllm_contracts::new_id()));
    let window_text = window_target.to_string_lossy().into_owned();
    recorder
        .step(&mut host, "file.export", json!({ "format": "pdf", "target_path": window_text, "scope": "page", "allowed_losses": null }))
        .expect("the window reads a null allowed_losses as absent");
    assert!(window_target.is_file(), "the window's export writes its target");
    let run_target = std::env::temp_dir().join(format!("designllm-null-losses-run-{}.pdf", designllm_contracts::new_id()));
    let run_text = run_target.to_string_lossy().into_owned();
    // The agent sends the same arguments in a run: a null allowed_losses must not refuse the run either.
    let (run_id, _) = queue_ui_file_export_run(
        &mut recorder,
        &mut host,
        "file_write",
        json!({ "format": "pdf", "target_path": run_text, "scope": "page", "allowed_losses": null }),
    )
    .expect("the run with a null allowed_losses is queued");
    let requests = deliver_ui_runs_until_rpc(&mut recorder, &mut host, "artifact.export");
    assert_eq!(requests.len(), 1);
    let result = host.call("artifact.export", requests[0].1.clone());
    assert!(result.is_ok(), "Core exports the queued request");
    answer_ui_apply_reply(&mut recorder, &mut host, requests[0].0, result);
    assert_eq!(host.call("ui.command.get", json!({ "run_id": run_id })).unwrap()["status"], "completed");
    assert!(run_target.is_file(), "the run writes its target");
    let _ = std::fs::remove_file(&window_target);
    let _ = std::fs::remove_file(&run_target);
}

#[test]
fn queued_layer_navigation_completes_only_after_the_host_selection_reply() {
    let mut host = Host::start();
    let canvas = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
    let mut recorder = Recorder::open(&mut host, &canvas);
    recorder.step(&mut host, "select.node", json!({ "node_id": "a" })).expect("source selected");
    let revision = host.artifact(&canvas).revision;
    let (run_id, _) = queue_ui_selection_run(&mut recorder, &mut host, "select.next");

    let requests = deliver_ui_runs_until_rpc(&mut recorder, &mut host, "selection.set");
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].1["selection"]["node_ids"], json!(["c"]), "the run selects the next layer in UI order");
    let waiting = host.call("ui.command.get", json!({ "run_id": run_id })).expect("the run remains queryable while selection.set waits");
    assert_eq!(waiting["status"], "delivered", "enqueueing selection.set does not complete the run");
    assert_eq!(host.artifact(&canvas).revision, revision, "selection navigation is not a document edit");

    let result = host.call("selection.set", requests[0].1.clone());
    assert!(result.is_ok(), "Core accepts the current view and revision");
    answer_ui_apply_reply(&mut recorder, &mut host, requests[0].0, result);
    let finished = host.call("ui.command.get", json!({ "run_id": run_id })).expect("the completed run is queryable");
    assert_eq!(finished["status"], "completed");
    assert_eq!(finished["result"]["command"], "select.next");
    assert!(finished["error"].is_null());
    assert_eq!(recorder.app.context.as_ref().unwrap().selection.node_ids, ["c"], "the Host confirmed the accepted selection");
    assert_eq!(host.artifact(&canvas).revision, revision);
}

#[test]
fn queued_layer_navigation_refuses_a_changed_selection_before_sending_selection_set() {
    let mut host = Host::start();
    let canvas = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
    let mut recorder = Recorder::open(&mut host, &canvas);
    recorder.step(&mut host, "select.node", json!({ "node_id": "a" })).expect("source selected");
    let revision = host.artifact(&canvas).revision;
    let (run_id, queued_state_seq) = queue_ui_selection_run(&mut recorder, &mut host, "select.next");

    recorder.step(&mut host, "select.node", json!({ "node_id": "b" })).expect("selection changes before delivery");
    recorder.app.publish_ui_if_due();
    recorder.settle(&mut host);
    let current = host.call("ui.state.read", json!({ "view_id": recorder.app.ui.view_id.clone() })).expect("updated state");
    assert!(current["state_seq"].as_u64().unwrap() > queued_state_seq);
    let calls_before_delivery = recorder.calls.len();
    poll_queued_ui_runs(&mut recorder, &mut host);

    assert!(!recorder.calls[calls_before_delivery..].iter().any(|call| call == "selection.set"), "the stale queued selection is never sent");
    assert_eq!(host.artifact(&canvas).revision, revision);
    let finished = host.call("ui.command.get", json!({ "run_id": run_id })).expect("the stale run is queryable");
    assert_eq!(finished["status"], "failed");
    let refusal: designllm_contracts::DltError = serde_json::from_value(finished["error"].clone()).expect("state refusal");
    assert_eq!(refusal.code, designllm_contracts::ErrorCode::RevisionConflict);
}

#[test]
fn queued_layer_navigation_refuses_a_changed_page_and_republishes_availability() {
    use designllm_contracts::{Intent, Operation, Page, Size};

    let mut host = Host::start();
    let canvas = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
    host.apply(
        &canvas,
        vec![Operation::InsertPage {
            index: 1,
            page: Page::new("empty-page", Size::a4()),
        }],
        [Intent::Content].into_iter().collect(),
    )
    .expect("the empty second page exists before opening the window");
    let mut recorder = Recorder::open(&mut host, &canvas);
    recorder.step(&mut host, "select.node", json!({ "node_id": "a" })).expect("source selected");
    let revision = host.artifact(&canvas).revision;
    let (run_id, queued_state_seq) = queue_ui_selection_run(&mut recorder, &mut host, "select.next");

    recorder.step(&mut host, "nav.page", json!({ "page_index": 1 })).expect("the visible page changes");
    recorder.app.publish_ui_if_due();
    recorder.settle(&mut host);
    let command_list = host.call("ui.command.list", json!({ "view_id": recorder.app.ui.view_id.clone() })).expect("updated command list");
    assert!(command_list["state_seq"].as_u64().unwrap() > queued_state_seq);
    assert_eq!(
        command_list["commands"].as_array().unwrap().iter().find(|command| command["id"] == "select.next").unwrap()["enabled"],
        false,
        "an empty current page disables layer navigation"
    );
    let calls_before_delivery = recorder.calls.len();
    poll_queued_ui_runs(&mut recorder, &mut host);

    assert!(!recorder.calls[calls_before_delivery..].iter().any(|call| call == "selection.set"), "the old page's selection is not sent");
    assert_eq!(host.artifact(&canvas).revision, revision);
    let finished = host.call("ui.command.get", json!({ "run_id": run_id })).expect("the stale page run is queryable");
    assert_eq!(finished["status"], "failed");
    let refusal: designllm_contracts::DltError = serde_json::from_value(finished["error"].clone()).expect("page refusal");
    assert_eq!(refusal.code, designllm_contracts::ErrorCode::RevisionConflict);
}

#[test]
fn queued_layer_navigation_reports_the_actual_selection_revision_conflict() {
    use designllm_contracts::{Intent, Operation};

    let mut host = Host::start();
    let canvas = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
    let mut recorder = Recorder::open(&mut host, &canvas);
    recorder.step(&mut host, "select.node", json!({ "node_id": "a" })).expect("source selected");
    let base_revision = host.artifact(&canvas).revision;
    let (run_id, _) = queue_ui_selection_run(&mut recorder, &mut host, "select.next");
    let requests = deliver_ui_runs_until_rpc(&mut recorder, &mut host, "selection.set");
    assert_eq!(requests.len(), 1);
    assert_eq!(host.call("ui.command.get", json!({ "run_id": run_id })).unwrap()["status"], "delivered");

    host.apply(
        &canvas,
        vec![Operation::SetNodeInfo { node_ids: vec!["a".into()], alt: None, name: Some("외부 변경".into()), hidden: None }],
        [Intent::Content].into_iter().collect(),
    )
    .expect("the document advances after Desktop has queued selection.set");
    let changed_revision = host.artifact(&canvas).revision;
    assert!(changed_revision > base_revision);
    let refusal = host.call("selection.set", requests[0].1.clone()).expect_err("the stale selection revision is rejected");
    assert_eq!(refusal.code, designllm_contracts::ErrorCode::RevisionConflict);
    answer_ui_apply_reply(&mut recorder, &mut host, requests[0].0, Err(refusal));

    let finished = host.call("ui.command.get", json!({ "run_id": run_id })).expect("the failed run is queryable");
    assert_eq!(finished["status"], "failed");
    let completion_error: designllm_contracts::DltError = serde_json::from_value(finished["error"].clone()).expect("the Core reply is reported");
    assert_eq!(completion_error.code, designllm_contracts::ErrorCode::RevisionConflict);
    let artifact = host.artifact(&canvas);
    assert_eq!(artifact.revision, changed_revision, "selection.set does not advance the document");
    assert_eq!(super::doc_tree::find_node(&artifact, "a").unwrap().ext.name.as_deref(), Some("외부 변경"));
}

#[test]
fn queued_file_save_uses_the_explicit_target_and_waits_for_real_save_results() {
    let mut host = Host::start();
    let canvas = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
    let mut recorder = Recorder::open(&mut host, &canvas);
    let target = std::env::temp_dir().join(format!("designllm-ai-save-{}.canvas", designllm_contracts::new_id()));
    let target_text = target.to_string_lossy().into_owned();

    let denied = queue_ui_file_save_run(&mut recorder, &mut host, "read", json!({ "target_path": target_text }));
    assert_eq!(denied.unwrap_err().code, designllm_contracts::ErrorCode::InvalidArgument, "Core enforces FileWrite permission");
    assert!(!target.exists(), "a read-only run never creates the target");

    let (run_id, _) = queue_ui_file_save_run(
        &mut recorder,
        &mut host,
        "file_write",
        json!({ "target_path": target_text, "overwrite": false }),
    )
    .expect("the explicit FileWrite request is queued");
    let requests = deliver_ui_runs_until_rpc(&mut recorder, &mut host, "artifact.save");
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].1["target_path"], target_text);
    assert_eq!(requests[0].1["overwrite"], false);
    assert!(requests[0].1["expected_target_hash"].is_null());
    assert_eq!(host.call("ui.command.get", json!({ "run_id": run_id })).unwrap()["status"], "delivered");
    let result = host.call("artifact.save", requests[0].1.clone());
    assert!(result.is_ok(), "Core writes the explicit new target");
    answer_ui_apply_reply(&mut recorder, &mut host, requests[0].0, result);
    let finished = host.call("ui.command.get", json!({ "run_id": run_id })).expect("save run completes");
    assert_eq!(finished["status"], "completed");
    let saved_bytes = std::fs::read(&target).expect("Core created the requested file");
    assert!(!saved_bytes.is_empty());

    let (existing_run, _) = queue_ui_file_save_run(
        &mut recorder,
        &mut host,
        "file_write",
        json!({ "target_path": target_text, "overwrite": false }),
    )
    .expect("existing-target attempt queues");
    let requests = deliver_ui_runs_until_rpc(&mut recorder, &mut host, "artifact.save");
    let refusal = host.call("artifact.save", requests[0].1.clone()).expect_err("Core refuses unapproved overwrite");
    assert_eq!(refusal.code, designllm_contracts::ErrorCode::AlreadyExists);
    answer_ui_apply_reply(&mut recorder, &mut host, requests[0].0, Err(refusal));
    let finished = host.call("ui.command.get", json!({ "run_id": existing_run })).expect("overwrite refusal is recorded");
    assert_eq!(finished["status"], "failed");
    assert_eq!(serde_json::from_value::<designllm_contracts::DltError>(finished["error"].clone()).unwrap().code, designllm_contracts::ErrorCode::AlreadyExists);
    assert_eq!(std::fs::read(&target).unwrap(), saved_bytes, "refused overwrite leaves the existing file unchanged");

    let (missing_hash_run, _) = queue_ui_file_save_run(
        &mut recorder,
        &mut host,
        "file_write",
        json!({ "target_path": target_text, "overwrite": true }),
    )
    .expect("overwrite without hash reaches Core validation");
    let requests = deliver_ui_runs_until_rpc(&mut recorder, &mut host, "artifact.save");
    let refusal = host.call("artifact.save", requests[0].1.clone()).expect_err("Core requires the expected target hash");
    assert_eq!(refusal.code, designllm_contracts::ErrorCode::InvalidArgument);
    answer_ui_apply_reply(&mut recorder, &mut host, requests[0].0, Err(refusal));
    let finished = host.call("ui.command.get", json!({ "run_id": missing_hash_run })).expect("missing-hash refusal is recorded");
    assert_eq!(finished["status"], "failed");
    assert_eq!(serde_json::from_value::<designllm_contracts::DltError>(finished["error"].clone()).unwrap().code, designllm_contracts::ErrorCode::InvalidArgument);
    assert_eq!(std::fs::read(&target).unwrap(), saved_bytes, "missing hash does not overwrite the target");

    let expected_hash = designllm_contracts::sha256(&saved_bytes);
    let (stale_hash_run, _) = queue_ui_file_save_run(
        &mut recorder,
        &mut host,
        "file_write",
        json!({ "target_path": target_text, "overwrite": true, "expected_target_hash": "0".repeat(64) }),
    )
    .expect("stale-hash overwrite queues for Core validation");
    let requests = deliver_ui_runs_until_rpc(&mut recorder, &mut host, "artifact.save");
    let refusal = host.call("artifact.save", requests[0].1.clone()).expect_err("Core rejects the stale target hash");
    assert_eq!(refusal.code, designllm_contracts::ErrorCode::SourceChanged);
    answer_ui_apply_reply(&mut recorder, &mut host, requests[0].0, Err(refusal));
    let finished = host.call("ui.command.get", json!({ "run_id": stale_hash_run })).expect("stale-hash refusal is recorded");
    assert_eq!(finished["status"], "failed");
    assert_eq!(serde_json::from_value::<designllm_contracts::DltError>(finished["error"].clone()).unwrap().code, designllm_contracts::ErrorCode::SourceChanged);
    assert_eq!(std::fs::read(&target).unwrap(), saved_bytes, "stale hash leaves the original target unchanged");

    let (overwrite_run, _) = queue_ui_file_save_run(
        &mut recorder,
        &mut host,
        "file_write",
        json!({ "target_path": target_text, "overwrite": true, "expected_target_hash": expected_hash }),
    )
    .expect("hash-checked overwrite queues");
    let requests = deliver_ui_runs_until_rpc(&mut recorder, &mut host, "artifact.save");
    let result = host.call("artifact.save", requests[0].1.clone());
    assert!(result.is_ok(), "Core accepts the current expected target hash");
    answer_ui_apply_reply(&mut recorder, &mut host, requests[0].0, result);
    assert_eq!(host.call("ui.command.get", json!({ "run_id": overwrite_run })).unwrap()["status"], "completed");

    let gui_target = std::env::temp_dir().join(format!("designllm-gui-save-{}.canvas", designllm_contracts::new_id()));
    recorder.app.save_path = gui_target.to_string_lossy().into_owned();
    recorder.step(&mut host, "file.save", json!({})).expect("the existing GUI shortcut still uses its chosen path");
    assert!(gui_target.is_file(), "the GUI save path remains functional without opening a dialog");
    let _ = std::fs::remove_file(&target);
    let _ = std::fs::remove_file(&gui_target);
}

#[test]
fn queued_file_export_preserves_and_reports_the_queued_revision() {
    use designllm_contracts::{Intent, Operation};

    let mut host = Host::start();
    let canvas = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
    let mut recorder = Recorder::open(&mut host, &canvas);
    let base_revision = host.artifact(&canvas).revision;
    let target = std::env::temp_dir().join(format!("designllm-stale-export-{}.pdf", designllm_contracts::new_id()));
    let target_text = target.to_string_lossy().into_owned();
    let (run_id, _) = queue_ui_file_export_run(
        &mut recorder,
        &mut host,
        "file_write",
        json!({ "format": "pdf", "target_path": target_text, "scope": "page", "allowed_losses": [] }),
    )
    .expect("the current state is queued for export");
    let requests = deliver_ui_runs_until_rpc(&mut recorder, &mut host, "artifact.export");
    assert_eq!(requests.len(), 1);
    assert_eq!(host.call("ui.command.get", json!({ "run_id": run_id })).unwrap()["status"], "delivered");

    host.apply(
        &canvas,
        vec![Operation::SetNodeInfo { node_ids: vec!["a".into()], alt: None, name: Some("외부 변경".into()), hidden: None }],
        [Intent::Content].into_iter().collect(),
    )
    .expect("the Host advances the artifact after export was queued");
    let current_revision = host.artifact(&canvas).revision;
    assert!(current_revision > base_revision);
    assert_eq!(requests[0].1["options"]["expected_revision"], json!(base_revision));
    let result = host.call("artifact.export", requests[0].1.clone()).expect("Core exports the exact queued historical revision");
    assert_eq!(result["revision"], json!(base_revision));
    assert!(target.is_file());
    answer_ui_apply_reply(&mut recorder, &mut host, requests[0].0, Ok(result));

    let finished = host.call("ui.command.get", json!({ "run_id": run_id })).expect("the export result is queryable");
    assert_eq!(finished["status"], "completed");
    assert_eq!(finished["result"]["revision"], json!(base_revision), "UiRun reports the actual Core export revision");
    let current = host.artifact(&canvas);
    assert_eq!(current.revision, current_revision, "exporting a snapshot does not mutate the newer workspace");
    assert_eq!(super::doc_tree::find_node(&current, "a").unwrap().ext.name.as_deref(), Some("외부 변경"));
    let _ = std::fs::remove_file(&target);
}

#[test]
fn queued_file_export_preserves_explicit_loss_consent_target_and_actual_results() {
    let mut host = Host::start();
    let fixture = std::fs::canonicalize(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/hwpx/opaque-image-preserve.hwpx"))
        .expect("shared HWPX export-loss fixture");
    let opened = host
        .call(
            "artifact.open",
            json!({ "request_id": designllm_contracts::new_id(), "actor": "test", "path": fixture.to_string_lossy(), "kind": "hwpx" }),
        )
        .expect("the HWPX fixture opens in the real Host");
    let hwpx = opened["id"].as_str().expect("opened artifact id").to_owned();
    let mut recorder = Recorder::open(&mut host, &hwpx);
    let target = std::env::temp_dir().join(format!("designllm-ai-export-{}.pdf", designllm_contracts::new_id()));
    let target_text = target.to_string_lossy().into_owned();
    let no_loss_consent = json!({ "format": "pdf", "target_path": target_text, "scope": "page", "allowed_losses": [] });

    let denied = queue_ui_file_export_run(&mut recorder, &mut host, "read", no_loss_consent.clone());
    assert_eq!(denied.unwrap_err().code, designllm_contracts::ErrorCode::InvalidArgument, "Core enforces FileWrite permission");
    assert!(!target.exists(), "a read-only run never creates an export");

    let (loss_run, _) = queue_ui_file_export_run(&mut recorder, &mut host, "file_write", no_loss_consent)
        .expect("FileWrite permission queues the one-call export");
    let requests = deliver_ui_runs_until_rpc(&mut recorder, &mut host, "artifact.export");
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].1["target_path"], target_text);
    assert_eq!(requests[0].1["dry_run"], Value::Null, "agent export is one actual write, not a dry-run dialog flow");
    assert_eq!(requests[0].1["options"]["allowed_losses"], json!([]), "loss consent is never inferred");
    assert_eq!(host.call("ui.command.get", json!({ "run_id": loss_run })).unwrap()["status"], "delivered");
    let refusal = host.call("artifact.export", requests[0].1.clone()).expect_err("Core refuses the unaccepted floating object loss");
    assert_eq!(refusal.code, designllm_contracts::ErrorCode::ExportLossRequired);
    assert!(refusal.diagnostics.iter().any(|diagnostic| diagnostic.code == "HWPX_FLOATING_OBJECT_NOT_RENDERED"));
    let mut allowed_loss_codes = Vec::new();
    for diagnostic in refusal.diagnostics.iter().filter(|diagnostic| diagnostic.blocking) {
        if !allowed_loss_codes.contains(&diagnostic.code) {
            allowed_loss_codes.push(diagnostic.code.clone());
        }
    }
    answer_ui_apply_reply(&mut recorder, &mut host, requests[0].0, Err(refusal));
    let finished = host.call("ui.command.get", json!({ "run_id": loss_run })).expect("loss refusal is recorded");
    assert_eq!(finished["status"], "failed");
    assert_eq!(serde_json::from_value::<designllm_contracts::DltError>(finished["error"].clone()).unwrap().code, designllm_contracts::ErrorCode::ExportLossRequired);
    assert!(recorder.app.export_confirmation.is_none(), "agent loss refusal never opens the GUI confirmation dialog");
    assert!(recorder.app.pending_export.is_none());
    assert!(!target.exists(), "a refused export writes no file");

    let accepted_losses = json!(allowed_loss_codes);
    let (accepted_run, _) = queue_ui_file_export_run(
        &mut recorder,
        &mut host,
        "file_write",
        json!({ "format": "pdf", "target_path": target_text, "scope": "page", "allowed_losses": accepted_losses }),
    )
    .expect("the caller explicitly allows the known loss");
    let requests = deliver_ui_runs_until_rpc(&mut recorder, &mut host, "artifact.export");
    assert_eq!(requests[0].1["options"]["allowed_losses"], accepted_losses);
    let result = host.call("artifact.export", requests[0].1.clone());
    assert!(result.is_ok(), "Core honors the explicitly allowed loss: {result:?}");
    answer_ui_apply_reply(&mut recorder, &mut host, requests[0].0, result);
    let finished = host.call("ui.command.get", json!({ "run_id": accepted_run })).expect("accepted export result");
    assert_eq!(finished["status"], "completed");
    let bytes = std::fs::read(&target).expect("the accepted PDF was written");
    assert!(bytes.starts_with(b"%PDF"));
    assert_eq!(finished["result"]["revision"], json!(host.artifact(&hwpx).revision), "the agent sees the actual export revision");
    assert_eq!(finished["result"]["target_path"], target_text);
    assert_eq!(finished["result"]["sha256"], designllm_contracts::sha256(&bytes));

    let (existing_run, _) = queue_ui_file_export_run(
        &mut recorder,
        &mut host,
        "file_write",
        json!({ "format": "pdf", "target_path": target_text, "scope": "page", "allowed_losses": accepted_losses }),
    )
    .expect("existing target attempt queues");
    let requests = deliver_ui_runs_until_rpc(&mut recorder, &mut host, "artifact.export");
    let refusal = host.call("artifact.export", requests[0].1.clone()).expect_err("Core refuses overwrite by default");
    assert_eq!(refusal.code, designllm_contracts::ErrorCode::AlreadyExists);
    answer_ui_apply_reply(&mut recorder, &mut host, requests[0].0, Err(refusal));
    assert_eq!(host.call("ui.command.get", json!({ "run_id": existing_run })).unwrap()["status"], "failed");
    assert_eq!(std::fs::read(&target).unwrap(), bytes, "a refused overwrite preserves the previous PDF");

    let target_hash = designllm_contracts::sha256(&bytes);
    let (overwrite_run, _) = queue_ui_file_export_run(
        &mut recorder,
        &mut host,
        "file_write",
        json!({ "format": "pdf", "target_path": target_text, "overwrite": true, "expected_target_hash": target_hash, "scope": "page", "allowed_losses": accepted_losses }),
    )
    .expect("the caller supplies the overwrite hash");
    let requests = deliver_ui_runs_until_rpc(&mut recorder, &mut host, "artifact.export");
    assert_eq!(requests[0].1["overwrite"], true);
    assert_eq!(requests[0].1["expected_target_hash"], designllm_contracts::sha256(&bytes));
    let result = host.call("artifact.export", requests[0].1.clone());
    assert!(result.is_ok(), "Core accepts the current target hash");
    answer_ui_apply_reply(&mut recorder, &mut host, requests[0].0, result);
    assert_eq!(host.call("ui.command.get", json!({ "run_id": overwrite_run })).unwrap()["status"], "completed");
    let _ = std::fs::remove_file(&target);
}

#[test]
fn queued_canvas_url_target_busy_completes_with_the_host_refusal_without_user_retry() {
    let mut host = Host::start();
    let canvas = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
    let mut recorder = Recorder::open(&mut host, &canvas);
    recorder.step(&mut host, "select.node", json!({ "node_id": "a" })).expect("source selected");
    let revision = host.artifact(&canvas).revision;
    let reservation = host
        .call(
            "targets.reserve",
            json!({
                "artifact_id": canvas.clone(),
                "expected_revision": revision,
                "view_id": recorder.app.view_id.clone(),
                "owner": "test reservation",
                "node_ids": ["a"],
                "ttl_ms": 60_000,
            }),
        )
        .expect("the target is reserved");
    let reservation_id = reservation["reservation_id"].as_str().expect("reservation id").to_owned();
    let (run_id, _) = queue_ui_content_run(
        &mut recorder,
        &mut host,
        "canvas.open_url",
        json!({ "url": "https://busy.example.test/" }),
    );
    let applies = deliver_ui_runs_until_apply(&mut recorder, &mut host);
    assert_eq!(applies.len(), 1);
    let result = host.call("changes.apply", applies[0].1.clone());
    assert_eq!(result.as_ref().unwrap_err().code, designllm_contracts::ErrorCode::TargetBusy);
    answer_ui_apply_reply(&mut recorder, &mut host, applies[0].0, result);
    let finished = host.call("ui.command.get", json!({ "run_id": run_id })).expect("the failed run is queryable");
    assert_eq!(finished["status"], "failed");
    let refusal: designllm_contracts::DltError = serde_json::from_value(finished["error"].clone()).expect("Core refusal");
    assert_eq!(refusal.code, designllm_contracts::ErrorCode::TargetBusy);
    assert!(recorder.app.held_edits.is_empty(), "a UI run is never retried as a direct user edit");
    assert!(node_interactions(&mut host, &canvas, "a").as_array().unwrap().is_empty());

    host.call("targets.release", json!({ "reservation_id": reservation_id })).expect("reservation released");
    assert!(node_interactions(&mut host, &canvas, "a").as_array().unwrap().is_empty(), "the failed run is not retried after release");
}

#[test]
fn two_queued_url_runs_resolve_to_their_own_apply_tickets_out_of_order() {
    let mut host = Host::start();
    let canvas = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
    let mut recorder = Recorder::open(&mut host, &canvas);
    recorder.step(&mut host, "select.many", json!({ "node_ids": ["a", "b"] })).expect("both targets selected");
    let (first_run, _) = queue_ui_content_run(
        &mut recorder,
        &mut host,
        "canvas.open_url",
        json!({ "url": "https://first.example.test/" }),
    );
    let (second_run, _) = queue_ui_content_run(
        &mut recorder,
        &mut host,
        "canvas.open_url",
        json!({ "url": "https://second.example.test/" }),
    );
    let applies = deliver_ui_runs_until_apply(&mut recorder, &mut host);
    assert_eq!(applies.len(), 2, "both delivered runs enqueue their own ChangeSet");
    let first_result = host.call("changes.apply", applies[0].1.clone());
    assert!(first_result.is_ok(), "the first ChangeSet commits");
    let second_result = host.call("changes.apply", applies[1].1.clone());
    assert_eq!(second_result.as_ref().unwrap_err().code, designllm_contracts::ErrorCode::RevisionConflict);

    // Deliver the later ticket's error before the earlier ticket's success.
    answer_ui_apply_reply(&mut recorder, &mut host, applies[1].0, second_result);
    answer_ui_apply_reply(&mut recorder, &mut host, applies[0].0, first_result);

    let first = host.call("ui.command.get", json!({ "run_id": first_run })).expect("first run is queryable");
    let second = host.call("ui.command.get", json!({ "run_id": second_run })).expect("second run is queryable");
    assert_eq!(first["status"], "completed");
    assert_eq!(second["status"], "failed");
    let refusal: designllm_contracts::DltError = serde_json::from_value(second["error"].clone()).expect("second run conflict");
    assert_eq!(refusal.code, designllm_contracts::ErrorCode::RevisionConflict);
    for node_id in ["a", "b"] {
        assert_eq!(node_interactions(&mut host, &canvas, node_id)[0]["url"], "https://first.example.test/");
    }
}

#[test]
fn a_late_apply_reply_does_not_turn_an_outcome_unknown_ui_run_into_success() {
    let mut host = Host::start();
    let canvas = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
    let mut recorder = Recorder::open(&mut host, &canvas);
    recorder.step(&mut host, "select.node", json!({ "node_id": "a" })).expect("source selected");
    let (run_id, _) = queue_ui_content_run_with_timeout(
        &mut recorder,
        &mut host,
        "canvas.open_url",
        json!({ "url": "https://late.example.test/" }),
        Some(2_000),
    );
    let applies = deliver_ui_runs_until_apply(&mut recorder, &mut host);
    assert_eq!(applies.len(), 1);
    std::thread::sleep(std::time::Duration::from_millis(2_100));

    let timed_out = host.call("ui.command.get", json!({ "run_id": run_id })).expect("the unknown outcome is recorded");
    assert_eq!(timed_out["status"], "failed");
    let unknown: designllm_contracts::DltError = serde_json::from_value(timed_out["error"].clone()).expect("timeout diagnostic");
    assert_eq!(unknown.code, designllm_contracts::ErrorCode::Unavailable);
    assert!(!unknown.retryable, "an unknown outcome is not safe to retry");
    assert!(unknown.diagnostics.iter().any(|diagnostic| diagnostic.code == "ui.outcome_unknown"));

    // Simulate the already-sent change returning after the UI run timed out.
    let late_result = host.call("changes.apply", applies[0].1.clone());
    assert!(late_result.is_ok(), "the late apply can still have committed");
    answer_ui_apply_reply(&mut recorder, &mut host, applies[0].0, late_result);
    let still_unknown = host.call("ui.command.get", json!({ "run_id": run_id })).expect("the late completion cannot rewrite the terminal run");
    assert_eq!(still_unknown["status"], "failed");
    let final_error: designllm_contracts::DltError = serde_json::from_value(still_unknown["error"].clone()).expect("unknown result remains");
    assert_eq!(final_error.code, designllm_contracts::ErrorCode::Unavailable);
    assert!(final_error.diagnostics.iter().any(|diagnostic| diagnostic.code == "ui.outcome_unknown"));
    assert_eq!(node_interactions(&mut host, &canvas, "a"), json!([{ "action": "open_url", "url": "https://late.example.test/" }]));
}

#[test]
fn direct_target_busy_retry_keeps_its_original_base_and_preserves_the_edit_after_an_interleaving_edit() {
    use designllm_contracts::{Intent, Operation};

    let mut host = Host::start();
    let canvas = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
    let base_revision = host.artifact(&canvas).revision;
    let mut recorder = Recorder::open(&mut host, &canvas);
    recorder.step(&mut host, "select.node", json!({ "node_id": "a" })).expect("source selected");
    let reservation = host
        .call(
            "targets.reserve",
            json!({
                "artifact_id": canvas.clone(),
                "expected_revision": base_revision,
                "view_id": recorder.app.view_id.clone(),
                "owner": "test job",
                "node_ids": ["a"],
                "ttl_ms": 60_000,
            }),
        )
        .expect("the node is reserved");
    let reservation_id = reservation["reservation_id"].as_str().expect("reservation id").to_owned();
    let event_seq = recorder.app.last_event_seq + 1;
    recorder.app.handle_events(json!({ "events": [{
        "event_seq": event_seq,
        "artifact_id": canvas,
        "revision": base_revision,
        "event": "targets.reserved",
        "detail": { "reservation_id": reservation_id, "job_id": "test-job", "owner": "test job", "node_ids": ["a"] },
    }] }));

    recorder.step(&mut host, "canvas.open_url", json!({ "url": "https://held.example.test/" })).expect("the edit reaches the Host reservation guard");
    assert_eq!(recorder.app.held_edits.len(), 1);
    let held = &recorder.app.held_edits[0];
    assert_eq!(held.apply.artifact_id, canvas);
    assert_eq!(held.apply.base_revision, base_revision);
    assert_eq!(held.apply.actor, "desktop");
    assert_eq!(held.apply.allowed_intents, [Intent::Content].into_iter().collect());
    assert!(held.apply.retryable);
    let held_operations = serde_json::to_value(&held.apply.operations).expect("held operations serialize");
    assert_eq!(held_operations[0]["node_id"], "a");
    assert_eq!(held_operations[0]["interactions"][0]["url"], "https://held.example.test/");

    host.call("targets.release", json!({ "reservation_id": reservation_id })).expect("reservation released");
    host.apply(
        &canvas,
        vec![Operation::SetNodeInfo { node_ids: vec!["a".into()], alt: None, name: Some("외부 수정".into()), hidden: None }],
        [Intent::Content].into_iter().collect(),
    )
    .expect("an edit interleaves before the held retry");
    let released_seq = recorder.app.last_event_seq + 1;
    recorder.app.handle_events(json!({ "events": [{
        "event_seq": released_seq,
        "artifact_id": canvas,
        "revision": base_revision + 1,
        "event": "targets.released",
        "detail": { "reservation_id": reservation_id, "job_id": "test-job", "owner": "test job", "node_ids": ["a"] },
    }] }));
    let retry_work: Vec<Work> = recorder.work.try_iter().collect();
    let retry = retry_work.iter().find_map(|item| match item {
        Work::Call { method, params, .. } if method == "changes.apply" => Some(params),
        _ => None,
    }).expect("the one retry is sent after release");
    assert_eq!(retry["artifact_id"], canvas);
    assert_eq!(retry["base_revision"], base_revision, "retry is not rebased to the interleaved revision");
    answer_recorded_work(&mut recorder, &mut host, retry_work);
    settle_recorded_work(&mut recorder, &mut host);

    let current = host.artifact(&canvas);
    assert_eq!(current.revision, base_revision + 1);
    assert_eq!(super::doc_tree::find_node(&current, "a").unwrap().ext.name.as_deref(), Some("외부 수정"));
    assert!(node_interactions(&mut host, &canvas, "a").as_array().unwrap().is_empty(), "the held URL does not overwrite the newer revision");
    assert_eq!(recorder.app.held_edits.len(), 1, "a stale retry remains available for explicit review");
    let held = &recorder.app.held_edits[0];
    assert_eq!(held.state, super::state::HeldEditState::NeedsReview);
    assert!(!held.can_retry, "a revision drift disables reapplying the old operation");
    assert_eq!(held.apply.base_revision, base_revision, "the refused operation is not silently rebased");
    let retained = serde_json::to_value(&held.apply.operations).expect("stale operation remains serializable");
    assert_eq!(retained[0]["interactions"][0]["url"], "https://held.example.test/");
}

#[test]
fn a_target_busy_apply_keeps_its_ticket_operations_when_direct_applies_reply_out_of_order() {
    let mut host = Host::start();
    let canvas = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
    let base_revision = host.artifact(&canvas).revision;
    let mut recorder = Recorder::open(&mut host, &canvas);
    recorder.step(&mut host, "select.node", json!({ "node_id": "a" })).expect("first source selected");
    let reservation = host
        .call(
            "targets.reserve",
            json!({ "artifact_id": canvas.clone(), "expected_revision": base_revision, "view_id": recorder.app.view_id.clone(), "owner": "test job", "node_ids": ["a"], "ttl_ms": 60_000 }),
        )
        .expect("node a is reserved");
    let reservation_id = reservation["reservation_id"].as_str().unwrap().to_owned();
    let seq = recorder.app.last_event_seq + 1;
    recorder.app.handle_events(json!({ "events": [{
        "event_seq": seq,
        "artifact_id": canvas.clone(),
        "revision": base_revision,
        "event": "targets.reserved",
        "detail": { "reservation_id": reservation_id.clone(), "job_id": "test-job", "owner": "test job", "node_ids": ["a"] },
    }] }));

    recorder.app.invoke("canvas.open_url", json!({ "url": "https://first-ticket.example.test/" })).expect("first edit queues");
    recorder.app.invoke("select.node", json!({ "node_id": "b" })).expect("selection changes locally between requests");
    recorder.app.invoke("canvas.open_url", json!({ "url": "https://second-ticket.example.test/" })).expect("second edit queues");

    let work: Vec<Work> = recorder.work.try_iter().collect();
    let mut apply_results = Vec::new();
    let mut selection_result = None;
    let mut other_results = Vec::new();
    for item in work {
        let Work::Call { ticket, method, params } = item else { continue };
        let result = host.call(&method, params);
        match method.as_str() {
            "changes.apply" => apply_results.push((ticket, result)),
            "selection.set" => selection_result = Some((ticket, result)),
            _ => other_results.push((ticket, result)),
        }
    }
    assert_eq!(apply_results.len(), 2);
    assert_eq!(apply_results[0].1.as_ref().unwrap_err().code, designllm_contracts::ErrorCode::TargetBusy);
    assert!(apply_results[1].1.is_ok(), "the second ticket applies to unreserved node b");

    let mut ordered_results = other_results;
    if let Some(selection_result) = selection_result {
        ordered_results.push(selection_result);
    }
    for (ticket, result) in ordered_results {
        recorder
            .replies
            .send(RpcReply { ticket, result: result.map(RpcPayload::Json) })
            .expect("the window still listens");
        recorder.app.process_replies(&recorder.ctx);
    }
    let (second_ticket, second_result) = (apply_results[1].0, apply_results[1].1.clone());
    recorder
        .replies
        .send(RpcReply { ticket: second_ticket, result: second_result.map(RpcPayload::Json) })
        .expect("the window still listens");
    recorder.app.process_replies(&recorder.ctx);
    let (first_ticket, first_error) = (apply_results[0].0, apply_results[0].1.clone().unwrap_err());
    recorder
        .replies
        .send(RpcReply { ticket: first_ticket, result: Err(first_error) })
        .expect("the window still listens");
    recorder.app.process_replies(&recorder.ctx);
    settle_recorded_work(&mut recorder, &mut host);

    assert_eq!(recorder.app.held_edits.len(), 1, "the first failed ticket has its own held edit");
    let held = &recorder.app.held_edits[0];
    assert_eq!(held.apply.base_revision, base_revision);
    assert_eq!(held.apply.artifact_id, canvas);
    let operations = serde_json::to_value(&held.apply.operations).expect("held operations serialize");
    assert_eq!(operations[0]["node_id"], "a");
    assert_eq!(operations[0]["interactions"][0]["url"], "https://first-ticket.example.test/");
    assert_eq!(node_interactions(&mut host, &canvas, "b"), json!([{ "action": "open_url", "url": "https://second-ticket.example.test/" }]));

    host.call("targets.release", json!({ "reservation_id": reservation_id })).expect("node a released");
    let seq = recorder.app.last_event_seq + 1;
    recorder.app.handle_events(json!({ "events": [{
        "event_seq": seq,
        "artifact_id": canvas.clone(),
        "revision": base_revision + 1,
        "event": "targets.released",
        "detail": { "reservation_id": reservation_id, "job_id": "test-job", "owner": "test job", "node_ids": ["a"] },
    }] }));
    let release_work: Vec<Work> = recorder.work.try_iter().collect();
    assert!(release_work.iter().all(|item| !matches!(item, Work::Call { method, .. } if method == "changes.apply")), "a stale held edit is not sent again automatically");

    assert!(node_interactions(&mut host, &canvas, "a").as_array().unwrap().is_empty(), "the stale first edit is refused");
    assert_eq!(node_interactions(&mut host, &canvas, "b"), json!([{ "action": "open_url", "url": "https://second-ticket.example.test/" }]));
    assert_eq!(recorder.app.held_edits.len(), 1, "the stale first edit stays available after the second ticket commits");
    assert_eq!(recorder.app.held_edits[0].state, super::state::HeldEditState::NeedsReview);
    assert!(!recorder.app.held_edits[0].can_retry, "the stale operation cannot be reused after its node changed");
    let retained = serde_json::to_value(&recorder.app.held_edits[0].apply.operations).expect("held first operation remains serialized");
    assert_eq!(retained[0]["interactions"][0]["url"], "https://first-ticket.example.test/");
}

#[test]
fn a_held_direct_edit_waits_for_its_original_artifact_after_an_artifact_switch() {
    let mut host = Host::start();
    let original = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
    let other = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
    let original_revision = host.artifact(&original).revision;
    let other_revision = host.artifact(&other).revision;
    let mut recorder = Recorder::open(&mut host, &original);
    recorder.step(&mut host, "select.node", json!({ "node_id": "a" })).expect("source selected");
    let original_reservation = host
        .call(
            "targets.reserve",
            json!({
                "artifact_id": original.clone(),
                "expected_revision": original_revision,
                "view_id": recorder.app.view_id.clone(),
                "owner": "original job",
                "node_ids": ["a"],
                "ttl_ms": 60_000,
            }),
        )
        .expect("the original target is reserved");
    let original_reservation_id = original_reservation["reservation_id"].as_str().unwrap().to_owned();
    let seq = recorder.app.last_event_seq + 1;
    recorder.app.handle_events(json!({ "events": [{
        "event_seq": seq,
        "artifact_id": original.clone(),
        "revision": original_revision,
        "event": "targets.reserved",
        "detail": { "reservation_id": original_reservation_id.clone(), "job_id": "original-job", "owner": "original job", "node_ids": ["a"] },
    }] }));
    recorder.step(&mut host, "canvas.open_url", json!({ "url": "https://original.example.test/" })).expect("the original edit is held");
    assert_eq!(recorder.app.held_edits.len(), 1);

    recorder.step(&mut host, "nav.open_project", json!({ "artifact_id": other.clone() })).expect("another artifact opens");
    let other_reservation = host
        .call(
            "targets.reserve",
            json!({
                "artifact_id": other.clone(),
                "expected_revision": other_revision,
                "view_id": recorder.app.view_id.clone(),
                "owner": "other job",
                "node_ids": ["a"],
                "ttl_ms": 60_000,
            }),
        )
        .expect("the other artifact has a reservation with the same node id");
    let other_reservation_id = other_reservation["reservation_id"].as_str().unwrap().to_owned();
    let seq = recorder.app.last_event_seq + 1;
    recorder.app.handle_events(json!({ "events": [{
        "event_seq": seq,
        "artifact_id": other.clone(),
        "revision": other_revision,
        "event": "targets.reserved",
        "detail": { "reservation_id": other_reservation_id.clone(), "job_id": "other-job", "owner": "other job", "node_ids": ["a"] },
    }] }));
    host.call("targets.release", json!({ "reservation_id": other_reservation_id })).expect("other reservation released");
    let seq = recorder.app.last_event_seq + 1;
    recorder.app.handle_events(json!({ "events": [{
        "event_seq": seq,
        "artifact_id": other.clone(),
        "revision": other_revision,
        "event": "targets.released",
        "detail": { "reservation_id": other_reservation_id, "job_id": "other-job", "owner": "other job", "node_ids": ["a"] },
    }] }));
    let work: Vec<Work> = recorder.work.try_iter().collect();
    assert!(work.iter().all(|item| !matches!(item, Work::Call { method, .. } if method == "changes.apply")), "the held edit is not retargeted to the other artifact");
    assert!(node_interactions(&mut host, &other, "a").as_array().unwrap().is_empty());

    host.call("targets.release", json!({ "reservation_id": original_reservation_id })).expect("original reservation released");
    let seq = recorder.app.last_event_seq + 1;
    recorder.app.handle_events(json!({ "events": [{
        "event_seq": seq,
        "artifact_id": original.clone(),
        "revision": original_revision,
        "event": "targets.released",
        "detail": { "reservation_id": original_reservation_id, "job_id": "original-job", "owner": "original job", "node_ids": ["a"] },
    }] }));
    assert!(recorder.work.try_iter().all(|item| !matches!(item, Work::Call { method, .. } if method == "changes.apply")), "inactive original release does not apply behind the other document");

    recorder.step(&mut host, "nav.open_project", json!({ "artifact_id": original.clone() })).expect("the original artifact is active again");
    assert_eq!(node_interactions(&mut host, &original, "a"), json!([{ "action": "open_url", "url": "https://original.example.test/" }]));
    assert!(node_interactions(&mut host, &other, "a").as_array().unwrap().is_empty());
    assert!(recorder.app.held_edits.is_empty());
}

#[test]
fn canvas_url_edit_preserves_other_actions_and_is_one_undoable_multi_selection_change() {
    use designllm_contracts::{Interaction, Intent, Operation};

    let mut host = Host::start();
    let canvas = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
    let page_id = match &host.artifact(&canvas).body {
        ArtifactBody::Canvas(document) => document.pages[0].id.clone(),
        _ => panic!("a Canvas"),
    };
    host.apply(
        &canvas,
        vec![
            Operation::SetInteractions {
                node_id: "a".into(),
                interactions: vec![Interaction::NavigatePage { page_id: page_id.clone() }],
            },
            Operation::SetInteractions {
                node_id: "b".into(),
                interactions: vec![Interaction::ToggleVisibility { node_id: "a".into() }],
            },
        ],
        [Intent::Content].into_iter().collect(),
    )
    .expect("existing actions are stored");
    let initial = host.artifact(&canvas);
    let initial_revision = initial.revision;
    let mut recorder = Recorder::open(&mut host, &canvas);
    recorder.step(&mut host, "select.many", json!({ "node_ids": ["a", "b", "c"] })).expect("three nodes selected");

    let frame = recorder.ctx.run(screen(1.0, 1600.0, 1200.0), |ctx| {
        recorder.app.draw_frame(ctx);
    });
    recorder.settle(&mut host);
    let shown = super::tests::drawn_text(&frame);
    assert!(shown.iter().any(|text| text == "클릭 동작"), "the selected Canvas node exposes its click action");
    assert!(shown.iter().any(|text| text == "URL"), "the URL field is visible");

    let valid = "https://example.test/path?q=1&label=hello";
    let mark = recorder.calls.len();
    recorder.step(&mut host, "canvas.open_url", json!({ "url": valid })).expect("URL action is added");
    let added = host.artifact(&canvas);
    assert_eq!(added.revision, initial_revision + 1, "three node changes share one ChangeSet");
    assert_eq!(recorder.app.selected_nodes, ["a", "b", "c"], "the multi-selection remains active after the edit");
    assert_eq!(
        node_interactions(&mut host, &canvas, "a"),
        json!([
            { "action": "navigate_page", "page_id": page_id },
            { "action": "open_url", "url": valid }
        ]),
        "the new link follows and preserves the existing page action"
    );
    assert_eq!(
        node_interactions(&mut host, &canvas, "b"),
        json!([
            { "action": "toggle_visibility", "node_id": "a" },
            { "action": "open_url", "url": valid }
        ]),
        "the new link follows and preserves the existing toggle action"
    );
    assert_eq!(node_interactions(&mut host, &canvas, "c"), json!([{ "action": "open_url", "url": valid }]));
    assert_eq!(
        recorder.calls[mark..].iter().filter(|call| call.as_str() == "changes.apply:set_interactions").count(),
        3,
        "one SetInteractions operation is sent for each selected node"
    );
    assert!(
        !recorder.calls[mark..].iter().any(|call| matches!(call.as_str(), "artifact.export" | "web.materialize")),
        "editing a link does not open the preview or export a file"
    );

    let invalid_revision = added.revision;
    let calls_before = recorder.calls.len();
    let invalid = recorder.step(&mut host, "canvas.open_url", json!({ "url": "javascript:alert(1)" }));
    assert!(invalid.is_err(), "unsafe schemes are refused before RPC");
    assert_eq!(recorder.calls.len(), calls_before);
    assert_eq!(host.artifact(&canvas).revision, invalid_revision);

    let updated = "mailto:design@example.test";
    recorder.step(&mut host, "canvas.open_url", json!({ "url": updated })).expect("URL action updates");
    assert_eq!(
        node_interactions(&mut host, &canvas, "a"),
        json!([
            { "action": "navigate_page", "page_id": page_id },
            { "action": "open_url", "url": updated }
        ]),
        "updating a URL keeps its position in the interaction order"
    );
    recorder.step(&mut host, "edit.undo", json!({})).expect("URL update undone");
    assert_eq!(node_interactions(&mut host, &canvas, "a")[1]["url"], valid);
    recorder.step(&mut host, "edit.redo", json!({})).expect("URL update redone");
    assert_eq!(node_interactions(&mut host, &canvas, "a")[1]["url"], updated);
    recorder.step(&mut host, "select.many", json!({ "node_ids": ["a", "b", "c"] })).expect("multi-selection restored");

    recorder.step(&mut host, "canvas.open_url", json!({ "url": "" })).expect("empty URL clears only OpenUrl");
    assert_eq!(node_interactions(&mut host, &canvas, "a"), json!([{ "action": "navigate_page", "page_id": page_id }]));
    assert_eq!(node_interactions(&mut host, &canvas, "b"), json!([{ "action": "toggle_visibility", "node_id": "a" }]));
    assert_eq!(node_interactions(&mut host, &canvas, "c"), json!([]), "empty interactions are a valid clear");
    recorder.step(&mut host, "edit.undo", json!({})).expect("URL clear undone");
    assert_eq!(node_interactions(&mut host, &canvas, "c"), json!([{ "action": "open_url", "url": updated }]));
}

#[test]
fn canvas_interaction_editor_keeps_action_order_when_targets_and_actions_change() {
    use designllm_contracts::{Interaction, Intent, Operation, Page, Size, StylePatch};

    let mut host = Host::start();
    let canvas = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
    let first_page = match &host.artifact(&canvas).body {
        ArtifactBody::Canvas(document) => document.pages[0].id.clone(),
        _ => panic!("a Canvas"),
    };
    let second_page = Page { name: Some("두 번째".into()), ..Page::new("page-two", Size::a4()) };
    host.apply(
        &canvas,
        vec![
            Operation::InsertPage { index: 1, page: second_page.clone() },
            Operation::SetNodeInfo { node_ids: vec!["a".into()], alt: None, name: Some("첫 화면".into()), hidden: None },
            Operation::SetNodeInfo { node_ids: vec!["b".into()], alt: None, name: Some("표시 대상".into()), hidden: None },
            Operation::SetVariantStyle { node_id: "a".into(), variant: "hover".into(), patch: StylePatch { fill: Some("#EFEFEF".into()), ..Default::default() } },
            Operation::SetVariantStyle { node_id: "b".into(), variant: "focus".into(), patch: StylePatch { fill: Some("#DDDDDD".into()), ..Default::default() } },
            Operation::SetInteractions {
                node_id: "a".into(),
                interactions: vec![
                    Interaction::NavigatePage { page_id: second_page.id.clone() },
                    Interaction::ToggleVisibility { node_id: "b".into() },
                    Interaction::SetVariant { node_id: "a".into(), variant: "hover".into() },
                    Interaction::Back,
                    Interaction::OpenUrl { url: "https://start.example.test/".into() },
                ],
            },
        ],
        [Intent::Content, Intent::Design].into_iter().collect(),
    )
    .expect("the prototype actions are stored");

    let mut recorder = Recorder::open(&mut host, &canvas);
    recorder.step(&mut host, "select.node", json!({ "node_id": "a" })).expect("one node selected");
    let frame = recorder.ctx.run(screen(1.0, 1600.0, 1200.0), |ctx| {
        recorder.app.draw_frame(ctx);
    });
    recorder.settle(&mut host);
    let shown = super::tests::drawn_text(&frame);
    for label in ["클릭 동작", "페이지 이동", "표시 전환", "상태 바꾸기", "뒤로", "URL", "두 번째"] {
        assert!(shown.iter().any(|text| text.contains(label)), "the interaction list shows {label}");
    }

    recorder.step(
        &mut host,
        "canvas.interaction.set",
        json!({ "index": 0, "action": "navigate_page", "target_id": first_page }),
    )
    .expect("page target changes");
    recorder.step(
        &mut host,
        "canvas.interaction.set",
        json!({ "index": 1, "action": "toggle_visibility", "target_id": "c" }),
    )
    .expect("visibility target changes");
    recorder.step(
        &mut host,
        "canvas.interaction.set",
        json!({ "index": 2, "action": "set_variant", "target_id": "b", "variant": "focus" }),
    )
    .expect("variant target changes");
    recorder.step(
        &mut host,
        "canvas.interaction.set",
        json!({ "index": 3, "action": "navigate_page", "target_id": first_page }),
    )
    .expect("Back can be edited to a page action");
    recorder.step(&mut host, "canvas.interaction.remove", json!({ "index": 3 })).expect("the edited action is removed by its index");
    recorder
        .step(&mut host, "canvas.interaction.set", json!({ "index": 4, "action": "back" }))
        .expect("Back can be added after the existing actions");
    let before_noop = host.artifact(&canvas).revision;
    let calls_before = recorder.calls.len();
    recorder.step(&mut host, "canvas.interaction.set", json!({ "index": 4, "action": "back" })).expect("the unchanged action is accepted");
    assert_eq!(host.artifact(&canvas).revision, before_noop, "a no-effect choice does not create a revision");
    assert_eq!(
        recorder.calls[calls_before..].iter().filter(|call| call.as_str() == "changes.apply:set_interactions").count(),
        0
    );
    recorder.step(&mut host, "canvas.interaction.remove", json!({ "index": 4 })).expect("Back can be removed by its index");
    recorder.step(&mut host, "canvas.interaction.remove", json!({ "index": 1 })).expect("one action can be removed without clearing its neighbours");

    assert_eq!(
        node_interactions(&mut host, &canvas, "a"),
        json!([
            { "action": "navigate_page", "page_id": first_page },
            { "action": "set_variant", "node_id": "b", "variant": "focus" },
            { "action": "open_url", "url": "https://start.example.test/" }
        ]),
        "target updates and index removals preserve each remaining action and its order"
    );
    recorder.step(&mut host, "edit.undo", json!({})).expect("the last action edit is one undo step");
    assert_eq!(
        node_interactions(&mut host, &canvas, "a"),
        json!([
            { "action": "navigate_page", "page_id": first_page },
            { "action": "toggle_visibility", "node_id": "c" },
            { "action": "set_variant", "node_id": "b", "variant": "focus" },
            { "action": "open_url", "url": "https://start.example.test/" }
        ]),
        "undo restores only the removed indexed action"
    );
    recorder.step(&mut host, "edit.redo", json!({})).expect("the indexed removal can be redone");
    assert_eq!(
        node_interactions(&mut host, &canvas, "a"),
        json!([
            { "action": "navigate_page", "page_id": first_page },
            { "action": "set_variant", "node_id": "b", "variant": "focus" },
            { "action": "open_url", "url": "https://start.example.test/" }
        ])
    );
}

#[test]
fn other_click_actions_require_single_selection() {
    use designllm_contracts::ArtifactBody;

    let mut host = Host::start();
    let canvas = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
    let before = host.artifact(&canvas);
    let mut recorder = Recorder::open(&mut host, &canvas);
    recorder.step(&mut host, "select.many", json!({ "node_ids": ["a", "b"] })).expect("two nodes selected");
    assert_eq!(recorder.app.selected_nodes.len(), 2);
    let mut right_panel = None;
    let _ = recorder.ctx.run(screen(1.0, 1600.0, 1200.0), |ctx| {
        right_panel = recorder.app.draw_frame(ctx).right;
    });
    recorder.settle(&mut host);
    let panel = right_panel.expect("the properties panel is open");
    let mut shown = Vec::new();
    let mut scroll_consumed = false;
    for _ in 0..20 {
        let mut scroll = screen(1.0, 1600.0, 1200.0);
        scroll.events.extend([
            eframe::egui::Event::PointerMoved(panel.center()),
            eframe::egui::Event::MouseWheel {
                unit: eframe::egui::MouseWheelUnit::Point,
                delta: eframe::egui::Vec2::new(0.0, -800.0),
                modifiers: eframe::egui::Modifiers::NONE,
            },
        ]);
        let frame = recorder.ctx.run(scroll, |ctx| {
            recorder.app.draw_frame(ctx);
        });
        recorder.settle(&mut host);
        scroll_consumed |= recorder.ctx.input(|input| input.smooth_scroll_delta.y == 0.0);
        shown = super::tests::drawn_text(&frame);
        if shown.iter().any(|text| text.contains("한 요소씩 편집하세요")) {
            break;
        }
    }
    assert!(scroll_consumed, "the properties ScrollArea receives the pointer-over-panel wheel");
    assert!(shown.iter().any(|text| text.contains("한 요소씩 편집하세요")), "repeated panel scrolling reveals the multi-selection guidance: {shown:?}");

    let calls_before = recorder.calls.len();
    let set_refusal = recorder.app.invoke("canvas.interaction.set", json!({ "index": 0, "action": "back" })).expect_err("multi-selection cannot edit one node's click action");
    assert!(set_refusal.contains("한 요소씩 편집하세요"), "{set_refusal}");
    let remove_refusal = recorder.app.invoke("canvas.interaction.remove", json!({ "index": 0 })).expect_err("multi-selection cannot remove one node's click action");
    assert!(remove_refusal.contains("한 요소씩 편집하세요"), "{remove_refusal}");
    assert_eq!(recorder.calls.len(), calls_before, "multi-selection is refused before RPC");
    let after = host.artifact(&canvas);
    assert_eq!(after.revision, before.revision);
    let ArtifactBody::Canvas(document) = after.body else { panic!("Canvas") };
    assert!(document.pages[0].nodes.iter().all(|node| node.interactions.is_empty()));
}

#[test]
fn invalid_click_targets_are_refused_without_changing_the_action_list() {
    use designllm_contracts::{Intent, Interaction, Operation, StylePatch};

    let mut host = Host::start();
    let canvas = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
    host.apply(
        &canvas,
        vec![
            Operation::SetVariantStyle {
                node_id: "b".into(),
                variant: "hover".into(),
                patch: StylePatch { fill: Some("#EFEFEF".into()), ..Default::default() },
            },
            Operation::SetInteractions {
                node_id: "a".into(),
                interactions: vec![Interaction::Back, Interaction::ToggleVisibility { node_id: "b".into() }],
            },
        ],
        [Intent::Content, Intent::Design].into_iter().collect(),
    )
    .expect("valid starting actions");
    let before_revision = host.artifact(&canvas).revision;
    let before_actions = node_interactions(&mut host, &canvas, "a");
    let mut recorder = Recorder::open(&mut host, &canvas);
    recorder.step(&mut host, "select.node", json!({ "node_id": "a" })).expect("source selected");

    for args in [
        json!({ "index": 0, "action": "navigate_page", "target_id": "missing-page" }),
        json!({ "index": 0, "action": "toggle_visibility", "target_id": "missing-node" }),
        json!({ "index": 0, "action": "set_variant", "target_id": "b", "variant": "missing-variant" }),
    ] {
        recorder.step(&mut host, "canvas.interaction.set", args).expect("target refusal is delivered through the existing UI path");
        assert_eq!(host.artifact(&canvas).revision, before_revision, "invalid target does not create a revision");
        assert_eq!(node_interactions(&mut host, &canvas, "a"), before_actions, "invalid target preserves all actions");
        assert!(recorder.app.refusal.is_some() || recorder.app.notices.current(std::time::Instant::now()).is_some());
    }
}

#[test]
fn locked_and_stale_click_action_edits_are_atomic() {
    use designllm_contracts::{ArtifactBody, Intent, Interaction, Operation};

    let mut host = Host::start();
    let canvas = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
    host.apply(
        &canvas,
        vec![
            Operation::SetInteractions { node_id: "a".into(), interactions: vec![Interaction::Back] },
            Operation::SetNodeAppearance { node_ids: vec!["a".into()], image_fit: None, locked: Some(true) },
        ],
        [Intent::Content, Intent::Design].into_iter().collect(),
    )
    .expect("locked source with an existing action");
    let before_locked = host.artifact(&canvas);
    let mut locked = Recorder::open(&mut host, &canvas);
    locked.step(&mut host, "select.node", json!({ "node_id": "a" })).expect("locked source selected");
    locked.step(&mut host, "canvas.interaction.remove", json!({ "index": 0 })).expect("remove reaches the locked-node guard");
    assert_eq!(host.artifact(&canvas).revision, before_locked.revision, "locked action edit is refused atomically");
    assert_eq!(node_interactions(&mut host, &canvas, "a"), json!([{ "action": "back" }]));
    assert!(locked.app.refusal.is_some() || locked.app.notices.current(std::time::Instant::now()).is_some());

    let mut host = Host::start();
    let canvas = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
    let page_id = match &host.artifact(&canvas).body {
        ArtifactBody::Canvas(document) => document.pages[0].id.clone(),
        _ => panic!("Canvas"),
    };
    host.apply(
        &canvas,
        vec![Operation::SetInteractions { node_id: "a".into(), interactions: vec![Interaction::Back] }],
        [Intent::Content].into_iter().collect(),
    )
    .expect("starting action");
    let mut stale = Recorder::open(&mut host, &canvas);
    stale.step(&mut host, "select.node", json!({ "node_id": "a" })).expect("source selected");
    let displayed_revision = stale.app.active.as_ref().expect("Canvas opened").revision;
    host.apply(
        &canvas,
        vec![Operation::SetNodeInfo { node_ids: vec!["a".into()], alt: None, name: Some("외부 수정".into()), hidden: None }],
        [Intent::Content].into_iter().collect(),
    )
    .expect("host advances the revision outside the window");
    stale
        .step(&mut host, "canvas.interaction.set", json!({ "index": 0, "action": "navigate_page", "target_id": page_id }))
        .expect("the stale edit reaches the revision guard");
    assert_eq!(host.artifact(&canvas).revision, displayed_revision + 1, "stale action edit adds no revision");
    assert_eq!(node_interactions(&mut host, &canvas, "a"), json!([{ "action": "back" }]));
    assert!(
        stale.app.refusal.is_some() || stale.app.notices.current(std::time::Instant::now()).is_some(),
        "refusal={:?}, notice={:?}, calls={:?}",
        stale.app.refusal,
        stale.app.notices.current(std::time::Instant::now()).map(|notice| &notice.text),
        stale.calls
    );
}

#[test]
fn a_locked_node_refuses_multi_selection_url_edit_without_partial_changes() {
    use designllm_contracts::{Intent, Operation};

    let mut host = Host::start();
    let canvas = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
    host.apply(
        &canvas,
        vec![Operation::SetNodeAppearance { node_ids: vec!["b".into()], image_fit: None, locked: Some(true) }],
        [Intent::Design].into_iter().collect(),
    )
    .expect("the second node is locked");
    let before = host.artifact(&canvas);
    let mut recorder = Recorder::open(&mut host, &canvas);
    recorder.step(&mut host, "select.many", json!({ "node_ids": ["a", "b"] })).expect("both nodes selected");
    recorder.step(&mut host, "canvas.open_url", json!({ "url": "https://example.test/" })).expect("the request reaches the Canvas guard");

    let after = host.artifact(&canvas);
    assert_eq!(after.revision, before.revision, "a locked target refuses the whole multi-selection ChangeSet");
    assert_eq!(node_interactions(&mut host, &canvas, "a"), json!([]), "the unlocked first node was not partially changed");
    assert_eq!(node_interactions(&mut host, &canvas, "b"), json!([]), "the locked node stays unchanged");
    assert!(
        recorder.app.notices.current(std::time::Instant::now()).is_some(),
        "the refusal is visible in the properties panel"
    );
}

#[test]
fn a_stale_revision_refuses_url_edit_without_changing_the_newer_artifact() {
    use designllm_contracts::{Intent, Operation};

    let mut host = Host::start();
    let canvas = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
    let mut recorder = Recorder::open(&mut host, &canvas);
    recorder.step(&mut host, "select.node", json!({ "node_id": "a" })).expect("node selected");
    let displayed_revision = recorder.app.active.as_ref().expect("open Canvas").revision;
    host.apply(
        &canvas,
        vec![Operation::SetNodeInfo { node_ids: vec!["a".into()], alt: None, name: Some("다른 수정".into()), hidden: None }],
        [Intent::Content].into_iter().collect(),
    )
    .expect("host advances while the GUI still shows the older revision");

    recorder.step(&mut host, "canvas.open_url", json!({ "url": "https://example.test/" })).expect("the stale request is sent");
    let current = host.artifact(&canvas);
    assert_eq!(current.revision, displayed_revision + 1, "the stale URL edit did not add a revision");
    assert!(node_interactions(&mut host, &canvas, "a").as_array().unwrap().is_empty());
    assert_eq!(super::doc_tree::find_node(&current, "a").unwrap().ext.name.as_deref(), Some("다른 수정"));
    assert!(recorder.calls.iter().any(|call| call == "changes.apply:set_interactions"), "the stale ChangeSet reached the host");
    assert!(
        recorder.app.refusal.is_some() || recorder.app.notices.current(std::time::Instant::now()).is_some(),
        "the revision conflict is reported: refusal={:?}, notice={:?}, calls={:?}",
        recorder.app.refusal,
        recorder.app.notices.current(std::time::Instant::now()).map(|notice| &notice.text),
        recorder.calls
    );
}

#[test]
fn typing_a_canvas_url_does_not_save_until_enter() {
    use eframe::egui::{Event, Id, Key, Modifiers};

    let mut host = Host::start();
    let canvas = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
    let mut recorder = Recorder::open(&mut host, &canvas);
    recorder.step(&mut host, "select.node", json!({ "node_id": "a" })).expect("node selected");
    recorder.ctx.memory_mut(|memory| memory.request_focus(Id::new(("canvas-open-url", "a", 0))));

    let mark = recorder.calls.len();
    let mut typing = screen(1.0, 1280.0, 800.0);
    typing.events.push(Event::Text("https://example.test/".into()));
    let _ = recorder.ctx.run(typing, |ctx| {
        recorder.app.draw_frame(ctx);
    });
    recorder.settle(&mut host);
    assert!(
        !recorder.calls[mark..].iter().any(|call| call == "changes.apply:set_interactions"),
        "typing into the field does not save"
    );
    assert!(node_interactions(&mut host, &canvas, "a").as_array().unwrap().is_empty());

    let mut enter = screen(1.0, 1280.0, 800.0);
    enter.events.push(Event::Key {
        key: Key::Enter,
        physical_key: Some(Key::Enter),
        pressed: true,
        repeat: false,
        modifiers: Modifiers::default(),
    });
    let _ = recorder.ctx.run(enter, |ctx| {
        recorder.app.draw_frame(ctx);
    });
    recorder.settle(&mut host);
    assert!(recorder.calls[mark..].iter().any(|call| call == "changes.apply:set_interactions"), "Enter applies once");
    assert_eq!(node_interactions(&mut host, &canvas, "a"), json!([{ "action": "open_url", "url": "https://example.test/" }]));
}

#[test]
fn resume_home_tile_and_template_command_create_the_fixed_a4_canvas() {
    let mut host = Host::start();
    let starting_canvas = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
    let mut recorder = Recorder::open(&mut host, &starting_canvas);
    recorder.step(&mut host, "home.tile", json!({ "tile": "resume" })).expect("resume tile selected");
    assert!(recorder.step(&mut host, "home.preset", json!({ "preset": "letter" })).is_err(), "resume has no Letter size");
    assert!(recorder.step(&mut host, "home.preset", json!({ "width": 612, "height": 792 })).is_err(), "resume cannot use a custom size");
    recorder.step(&mut host, "home.create", json!({ "tile": "resume" })).expect("resume tile creates");
    let check_resume = |recorder: &Recorder| {
        let artifact = recorder.app.artifact.as_ref().expect("resume loaded");
        let ArtifactBody::Canvas(document) = &artifact.body else { panic!("resume is a canvas") };
        assert_eq!(document.pages[0].size, designllm_contracts::Size::a4());
        assert!(super::doc_tree::find_node(artifact, "resume.name").is_some(), "the resume template is populated");
    };
    check_resume(&recorder);
    recorder.step(&mut host, "home.use_template", json!({ "template": "resume" })).expect("resume template command creates");
    check_resume(&recorder);
}

#[test]
fn layer_selection_commands_follow_tree_order_wrap_and_leave_the_document_unchanged() {
    let mut host = Host::start();
    let canvas = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
    let mut recorder = Recorder::open(&mut host, &canvas);
    let revision = host.artifact(&canvas).revision;

    recorder.step(&mut host, "select.next", json!({})).expect("an empty selection advances to the first layer row");
    assert_eq!(recorder.app.selected_node.as_deref(), Some("c"), "frontmost layer comes first");
    recorder.step(&mut host, "select.next", json!({})).expect("next layer");
    assert_eq!(recorder.app.selected_node.as_deref(), Some("b"));
    recorder.step(&mut host, "select.previous", json!({})).expect("previous layer");
    assert_eq!(recorder.app.selected_node.as_deref(), Some("c"));
    recorder.step(&mut host, "select.previous", json!({})).expect("previous wraps at the beginning");
    assert_eq!(recorder.app.selected_node.as_deref(), Some("a"));
    recorder.step(&mut host, "select.next", json!({})).expect("next wraps at the end");
    assert_eq!(recorder.app.selected_node.as_deref(), Some("c"));
    recorder.step(&mut host, "select.clear", json!({})).expect("selection clears");
    recorder.step(&mut host, "select.previous", json!({})).expect("an empty selection starts at the last row when moving backward");
    assert_eq!(recorder.app.selected_node.as_deref(), Some("a"));

    assert_eq!(host.artifact(&canvas).revision, revision, "selection navigation does not edit the artifact");
    assert_eq!(recorder.app.context.as_ref().unwrap().selection.node_ids, ["a"], "the host confirms the final selection");
}

#[test]
fn home_template_command_accepts_and_names_every_new_canvas_template() {
    let mut host = Host::start();
    let canvas = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
    let mut recorder = Recorder::open(&mut host, &canvas);
    for (template, title) in [("login", "로그인"), ("dashboard", "대시보드"), ("invitation", "초대장"), ("banner", "배너")] {
        recorder.step(&mut host, "home.use_template", json!({ "template": template })).expect("template accepted");
        assert_eq!(recorder.app.active.as_ref().unwrap().title, title, "{template}");
        assert!(matches!(&recorder.app.artifact.as_ref().unwrap().body, ArtifactBody::Canvas(_)), "{template}");
    }
}

#[test]
fn home_template_command_creates_the_webcode_animation_with_its_kind() {
    let mut host = Host::start();
    let starting_canvas = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
    let mut recorder = Recorder::open(&mut host, &starting_canvas);
    recorder.step(&mut host, "home.use_template", json!({ "template": "webgl_animation", "kind": "web_code" }))
        .expect("WebCode template command creates through artifact.create");

    let summary = recorder.app.active.as_ref().expect("WebCode template is active");
    assert_eq!(summary.kind, designllm_contracts::ArtifactKind::WebCode);
    let artifact = recorder.app.artifact.as_ref().expect("WebCode artifact is loaded");
    let ArtifactBody::WebCode(document) = &artifact.body else { panic!("the selected template creates WebCode") };
    assert!(document.files["index.html"].content.contains("./animation.js"));
    assert!(document.files["animation.js"].content.contains("getContext('webgl'"));
}

#[test]
fn home_animation_tile_uses_the_selected_frame_and_web_tile_stays_blank() {
    let mut host = Host::start();
    let starting_canvas = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
    let mut recorder = Recorder::open(&mut host, &starting_canvas);

    recorder.step(&mut host, "home.tile", json!({ "tile": "animation" })).expect("select animation tile");
    recorder.step(&mut host, "home.preset", json!({ "preset": "slide_16_9" })).expect("select 16:9 output size");
    recorder.step(&mut host, "home.create", json!({ "tile": "animation", "prompt": "" })).expect("create the WebCode animation");
    let slide_html = {
        let animation = recorder.app.artifact.as_ref().expect("animation artifact is active");
        assert_eq!(animation.kind, designllm_contracts::ArtifactKind::WebCode);
        let ArtifactBody::WebCode(document) = &animation.body else { panic!("animation tile creates WebCode") };
        assert!(document.files.contains_key("animation.js"), "animation tile selects webgl_animation");
        document.files["index.html"].content.clone()
    };
    assert!(slide_html.contains("data-artboard-width-emu=\"12192000\" data-artboard-height-emu=\"6858000\""));
    assert!(slide_html.contains("style=\"width:min(1280px,100%);aspect-ratio:1280 / 720;min-height:0\""));

    recorder.step(&mut host, "home.tile", json!({ "tile": "animation" })).expect("select animation tile again");
    recorder.step(&mut host, "home.preset", json!({ "preset": "social_square" })).expect("select square output size");
    recorder.step(&mut host, "home.create", json!({ "tile": "animation", "prompt": "" })).expect("create the square WebCode animation");
    let square_html = {
        let animation = recorder.app.artifact.as_ref().expect("square animation is active");
        let ArtifactBody::WebCode(document) = &animation.body else { panic!("animation tile creates WebCode") };
        document.files["index.html"].content.clone()
    };
    let square = designllm_contracts::frame_preset("social_square").expect("square preset").size;
    assert!(square_html.contains(&format!("data-artboard-width-emu=\"{}\" data-artboard-height-emu=\"{}\"", square.width, square.height)));
    assert!(square_html.contains("style=\"width:min(1080px,100%);aspect-ratio:1080 / 1080;min-height:0\""));
    assert_ne!(slide_html, square_html, "the selected output size changes generated HTML dimensions");

    recorder.step(&mut host, "home.tile", json!({ "tile": "web" })).expect("select regular web tile");
    recorder.step(&mut host, "home.create", json!({ "tile": "web", "prompt": "" })).expect("create blank WebCode");
    let web = recorder.app.artifact.as_ref().expect("blank web artifact is active");
    assert_eq!(web.kind, designllm_contracts::ArtifactKind::WebCode);
    let ArtifactBody::WebCode(document) = &web.body else { panic!("web tile creates WebCode") };
    assert!(!document.files.contains_key("animation.js"), "the regular web tile remains blank");
}

#[test]
fn text_list_command_sets_and_clears_every_selected_empty_canvas_text_node() {
    use designllm_contracts::TextListKind;

    let mut host = Host::start();
    let canvas = canvas_with_text_nodes(&mut host, false);
    let mut recorder = Recorder::open(&mut host, &canvas);
    let mut revision = host.artifact(&canvas).revision;
    for (mode, expected) in [
        ("bullet", Some(TextListKind::Bullet)),
        ("numbered", Some(TextListKind::Numbered)),
        ("none", None),
    ] {
        recorder
            .step(&mut host, "select.many", json!({ "node_ids": ["list-text-1", "list-text-2"] }))
            .expect("both text nodes stay selected");
        recorder.step(&mut host, "text.list", json!({ "list": mode })).expect("list mode applies");
        let artifact = host.artifact(&canvas);
        assert_eq!(artifact.revision, revision + 1, "one change set for {mode}");
        for id in ["list-text-1", "list-text-2"] {
            let node = super::doc_tree::find_node(&artifact, id).expect("selected text node");
            assert_eq!(node.text, "", "empty text nodes remain empty");
            assert_eq!(node.ext.list, expected, "the list setting applies to {id}");
        }
        revision = artifact.revision;
    }
}

#[test]
fn text_list_command_refuses_a_mixed_selection_with_a_locked_node_atomically() {
    use crate::notice::Severity;

    let mut host = Host::start();
    let canvas = canvas_with_text_nodes(&mut host, true);
    let mut recorder = Recorder::open(&mut host, &canvas);
    recorder
        .step(&mut host, "select.many", json!({ "node_ids": ["list-text-1", "list-text-2"] }))
        .expect("both text nodes are selected");
    let revision = host.artifact(&canvas).revision;

    recorder.step(&mut host, "text.list", json!({ "list": "bullet" })).expect("the change is sent to the host");

    let unchanged = host.artifact(&canvas);
    assert_eq!(unchanged.revision, revision, "the selected nodes change as one atomic operation");
    assert_eq!(super::doc_tree::find_node(&unchanged, "list-text-1").unwrap().ext.list, None);
    assert_eq!(super::doc_tree::find_node(&unchanged, "list-text-2").unwrap().ext.list, None);
    assert_eq!(recorder.app.notices.current(std::time::Instant::now()).map(|notice| notice.severity), Some(Severity::Error));
}

#[test]
fn figure_link_calls_match_the_registry_for_page_and_flow_placement() {
    let mut host = Host::start();
    let figure = super::link_flow_tests::drawn_figure(&mut host, "그림", 1);
    for (kind, other_placement) in [("canvas", "changes.apply:insert_flow_block"), ("docx", "changes.apply:add_node")] {
        let doc = host.create(kind, "링크");
        let mut window = Recorder::open(&mut host, &doc);
        let mark = window.calls.len();
        window.step(&mut host, "add.figure_link", json!({ "figure_id": figure })).unwrap();
        let declared: Vec<_> = commands::core_calls("add.figure_link").unwrap().iter().copied()
            .filter(|call| *call != other_placement).collect();
        assert_eq!(distinct_in_order(&window.calls[mark..]), declared, "{kind}");
    }
}

#[test]
fn table_merge_runs_through_the_agent_registry_path_and_unmerges_the_same_region() {
    let mut host = Host::start();
    let canvas = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
    let mut recorder = Recorder::open(&mut host, &canvas);
    recorder.step(&mut host, "canvas.table.add", json!({ "rows": 2, "columns": 2 })).expect("a Canvas table is ready");
    let table_id = recorder.app.selected_node.clone().expect("the new table is selected");
    recorder.step(&mut host, "canvas.table.set_header", json!({ "table_id": table_id, "enabled": false })).expect("the header is off");
    complete_queued_table_change(
        &mut recorder,
        &mut host,
        "table.merge_cells",
        json!({ "table_id": table_id, "row": 1, "column": 0, "rows": 1, "columns": 2 }),
        &canvas,
    );
    let spans = |host: &mut Host| {
        let artifact = host.artifact(&canvas);
        let ArtifactBody::Canvas(document) = &artifact.body else { panic!("a canvas") };
        document.pages[0].nodes.iter().find_map(|node| node.ext.table.clone()).expect("the table").cell_spans
    };
    assert_eq!(spans(&mut host).len(), 1, "the merge is one span through the agent path");
    complete_queued_table_change(
        &mut recorder,
        &mut host,
        "table.unmerge_cell",
        json!({ "table_id": table_id, "row": 1, "column": 1 }),
        &canvas,
    );
    assert!(spans(&mut host).is_empty(), "the unmerge through the agent path clears the span");
}

#[test]
fn every_kind_command_sends_the_core_calls_it_declares() {
    let mut host = Host::start();
    let mut problems: Vec<String> = Vec::new();
    for id in KIND_RUN {
        let spec = commands::find(id).unwrap_or_else(|| panic!("{id} is registered"));
        assert!(commands::available(id), "{id} is wired");
        let (mut recorder, args) = prepare_kind(&mut host, id);
        recorder.app.notices.clear();
        let mark = recorder.calls.len();
        let result = recorder.app.invoke(spec.id, args);
        recorder.settle(&mut host);
        if let Err(message) = result {
            problems.push(format!("{id}: refused: {message}"));
        }
        if let Some(notice) = recorder.app.notices.current(std::time::Instant::now())
            .filter(|notice| notice.severity == crate::notice::Severity::Error)
        {
            problems.push(format!("{id}: the host refused: {}", notice.text));
        }
        let sent = distinct_in_order(&recorder.calls[mark..]);
        let sent_refs: Vec<&str> = sent.iter().map(String::as_str).collect();
        match commands::core_calls(id) {
            Some(declared) if declared == sent_refs.as_slice() => {}
            declared => problems.push(format!("{id}: declared {declared:?}, sent {sent_refs:?}")),
        }
    }
    assert!(problems.is_empty(), "{} kind commands differ:\n{}", problems.len(), problems.join("\n"));
}

#[test]
fn image_fit_and_alt_updates_preserve_node_data_and_empty_alt_clears() {
    use designllm_contracts::ImageFit;

    let mut host = Host::start();
    let figure = super::link_flow_tests::drawn_figure(&mut host, "그림", 1);
    let canvas = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
    let mut recorder = Recorder::open(&mut host, &canvas);
    recorder.step(&mut host, "add.figure_link", json!({ "figure_id": figure })).expect("the image is inserted");
    let image_id = recorder.app.selected_node.clone().expect("the image is selected");
    let original_artifact = host.artifact(&canvas);
    let original = super::doc_tree::find_node(&original_artifact, &image_id).expect("the image is stored").clone();
    assert_eq!(original.ext.alt, None);

    recorder.step(&mut host, "props.image_fit", json!({ "fit": "cover" })).expect("fit changes");
    let fitted_artifact = host.artifact(&canvas);
    let fitted = super::doc_tree::find_node(&fitted_artifact, &image_id).expect("the same image remains");
    assert_eq!(fitted.id, original.id);
    assert_eq!(fitted.bounds, original.bounds);
    assert_eq!(fitted.asset, original.asset);
    assert_eq!(fitted.ext.image_fit, Some(ImageFit::Cover));
    assert_eq!(fitted.ext.alt, None, "unspecified alt stays unspecified until edited");

    recorder.step(&mut host, "props.image_alt", json!({ "alt": "장면 설명" })).expect("alt is saved");
    let described_artifact = host.artifact(&canvas);
    let described = super::doc_tree::find_node(&described_artifact, &image_id).expect("the same image remains");
    assert_eq!(described.ext.alt.as_deref(), Some("장면 설명"));

    recorder.step(&mut host, "props.image_alt", json!({ "alt": "" })).expect("empty alt clears the field");
    let edited_artifact = host.artifact(&canvas);
    let edited = super::doc_tree::find_node(&edited_artifact, &image_id).expect("the same image remains");
    assert_eq!(edited.id, original.id);
    assert_eq!(edited.bounds, original.bounds);
    assert_eq!(edited.asset, original.asset);
    assert_eq!(edited.ext.image_fit, Some(ImageFit::Cover));
    assert_eq!(edited.ext.alt, None);
}

#[test]
fn queued_local_token_commands_define_bind_and_switch_the_selected_canvas_style() {
    let mut host = Host::start();
    let canvas = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
    let mut recorder = Recorder::open(&mut host, &canvas);
    recorder.app.publish_ui_if_due();
    recorder.settle(&mut host);
    let listed = host
        .call("ui.command.list", json!({ "view_id": recorder.app.ui.view_id.clone() }))
        .expect("the Desktop command list is available to MCP");
    for command in ["canvas.token.put", "canvas.token.mode", "canvas.token.remove"] {
        assert!(listed["commands"].as_array().unwrap().iter().any(|item| item["id"] == command && item["enabled"] == true), "{command}");
    }
    // Binding takes the selection, so with nothing selected it is not offered (backlog desktop 29); it is once a node is.
    for command in ["canvas.token.bind", "canvas.token.unbind"] {
        assert!(listed["commands"].as_array().unwrap().iter().any(|item| item["id"] == command && item["enabled"] == false), "{command}");
    }

    let (put_run, _) = queue_ui_design_run(
        &mut recorder,
        &mut host,
        "canvas.token.put",
        json!({
            "token_id": "brand.ink", "name": "Brand ink",
            "values": { "kind": "color", "light": "#112233", "dark": "#445566" }
        }),
    );
    let applies = deliver_ui_runs_until_apply(&mut recorder, &mut host);
    assert_eq!(applies.len(), 1);
    assert_eq!(applies[0].1["operations"][0]["op"], "put_token");
    let result = host.call("changes.apply", applies[0].1.clone());
    assert!(result.is_ok(), "the token definition is stored by the normal change path");
    answer_ui_apply_reply(&mut recorder, &mut host, applies[0].0, result);
    let put_finished = host.call("ui.command.get", json!({ "run_id": put_run })).expect("the completed token definition run");
    assert_eq!(put_finished["status"], "completed");
    assert_eq!(put_finished["result"]["core_result"]["revision"], host.artifact(&canvas).revision);
    assert!(put_finished["result"]["core_result"]["diagnostics"].is_array());

    recorder.step(&mut host, "select.node", json!({ "node_id": "a" })).expect("the target node is selected");
    let (bind_run, _) = queue_ui_design_run(
        &mut recorder,
        &mut host,
        "canvas.token.bind",
        json!({ "token_id": "brand.ink", "target": "style_fill" }),
    );
    let applies = deliver_ui_runs_until_apply(&mut recorder, &mut host);
    assert_eq!(applies.len(), 1);
    assert_eq!(applies[0].1["operations"][0]["op"], "bind_token");
    let result = host.call("changes.apply", applies[0].1.clone());
    assert!(result.is_ok(), "the selected node binding is stored");
    answer_ui_apply_reply(&mut recorder, &mut host, applies[0].0, result);
    assert_eq!(host.call("ui.command.get", json!({ "run_id": bind_run })).unwrap()["status"], "completed");

    let (mode_run, _) = queue_ui_design_run(&mut recorder, &mut host, "canvas.token.mode", json!({ "mode": "dark" }));
    let applies = deliver_ui_runs_until_apply(&mut recorder, &mut host);
    assert_eq!(applies.len(), 1);
    assert_eq!(applies[0].1["operations"][0]["op"], "set_token_mode");
    let result = host.call("changes.apply", applies[0].1.clone());
    assert!(result.is_ok(), "the selected token mode is applied");
    answer_ui_apply_reply(&mut recorder, &mut host, applies[0].0, result);
    let mode_finished = host.call("ui.command.get", json!({ "run_id": mode_run })).expect("the completed mode run");
    assert_eq!(mode_finished["status"], "completed");
    assert_eq!(mode_finished["result"]["core_result"]["revision"], host.artifact(&canvas).revision);
    assert_eq!(super::doc_tree::find_node(&host.artifact(&canvas), "a").unwrap().style.fill, "#445566");

    let (unbind_run, _) = queue_ui_design_run(&mut recorder, &mut host, "canvas.token.unbind", json!({ "target": "style_fill" }));
    let applies = deliver_ui_runs_until_apply(&mut recorder, &mut host);
    assert_eq!(applies.len(), 1);
    assert_eq!(applies[0].1["operations"][0]["op"], "unbind_token");
    let result = host.call("changes.apply", applies[0].1.clone());
    assert!(result.is_ok(), "the selected node binding is removed");
    answer_ui_apply_reply(&mut recorder, &mut host, applies[0].0, result);
    assert_eq!(host.call("ui.command.get", json!({ "run_id": unbind_run })).unwrap()["status"], "completed");
    let after_unbind = host.artifact(&canvas);
    let ArtifactBody::Canvas(document) = after_unbind.body else { panic!("the Canvas remains open") };
    assert!(document.bindings.is_empty());

    let (remove_run, _) = queue_ui_design_run(&mut recorder, &mut host, "canvas.token.remove", json!({ "token_id": "brand.ink" }));
    let applies = deliver_ui_runs_until_apply(&mut recorder, &mut host);
    assert_eq!(applies.len(), 1);
    assert_eq!(applies[0].1["operations"][0]["op"], "remove_token");
    let result = host.call("changes.apply", applies[0].1.clone());
    assert!(result.is_ok(), "the unused local token can be removed");
    answer_ui_apply_reply(&mut recorder, &mut host, applies[0].0, result);
    assert_eq!(host.call("ui.command.get", json!({ "run_id": remove_run })).unwrap()["status"], "completed");
    let after_remove = host.artifact(&canvas);
    let ArtifactBody::Canvas(document) = after_remove.body else { panic!("the Canvas remains open") };
    assert!(document.tokens.is_empty());

    let (size_run, _) = queue_ui_design_run(
        &mut recorder,
        &mut host,
        "canvas.token.put",
        json!({
            "token_id": "type.heading", "name": "Heading size",
            "values": { "kind": "font_size", "light": 152400, "dark": 190500 }
        }),
    );
    let applies = deliver_ui_runs_until_apply(&mut recorder, &mut host);
    assert_eq!(applies.len(), 1);
    let result = host.call("changes.apply", applies[0].1.clone());
    assert!(result.is_ok(), "the documented EMU values are accepted as-is");
    answer_ui_apply_reply(&mut recorder, &mut host, applies[0].0, result);
    assert_eq!(host.call("ui.command.get", json!({ "run_id": size_run })).unwrap()["status"], "completed");
    let stored = host.artifact(&canvas);
    let ArtifactBody::Canvas(document) = stored.body else { panic!("the Canvas remains open") };
    assert_eq!(
        document.tokens["type.heading"].values,
        designllm_contracts::TokenValues::FontSize { light: 152_400, dark: 190_500 },
        "16 px is represented as 152400 EMU by the GUI form; the MCP command's EMU integer is not converted again"
    );
}

#[test]
fn queued_token_binding_on_a_mixed_locked_selection_is_refused_without_partial_changes() {
    use designllm_contracts::{Intent, Operation, TokenValues};

    let mut host = Host::start();
    let canvas = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
    host.apply(
        &canvas,
        vec![
            Operation::PutToken {
                token_id: "brand.ink".into(),
                name: "Brand ink".into(),
                values: TokenValues::Color { light: "#112233".into(), dark: "#445566".into() },
            },
            Operation::SetNodeAppearance { node_ids: vec!["a".into()], image_fit: None, locked: Some(true) },
        ],
        [Intent::Design].into_iter().collect(),
    )
    .expect("the token and locked node are prepared");
    let mut recorder = Recorder::open(&mut host, &canvas);
    recorder.step(&mut host, "select.many", json!({ "node_ids": ["a", "b"] })).expect("locked and unlocked nodes are selected");
    let before = host.artifact(&canvas);

    let (run_id, _) = queue_ui_design_run(
        &mut recorder,
        &mut host,
        "canvas.token.bind",
        json!({ "token_id": "brand.ink", "target": "style_fill" }),
    );
    let applies = deliver_ui_runs_until_apply(&mut recorder, &mut host);
    assert_eq!(applies.len(), 1);
    let result = host.call("changes.apply", applies[0].1.clone());
    assert!(result.is_err(), "a locked selected node refuses the whole token binding");
    answer_ui_apply_reply(&mut recorder, &mut host, applies[0].0, result);

    let after = host.artifact(&canvas);
    assert_eq!(after.revision, before.revision);
    assert_eq!(serde_json::to_value(&after.body).unwrap(), serde_json::to_value(&before.body).unwrap());
    let finished = host.call("ui.command.get", json!({ "run_id": run_id })).expect("the refused run is queryable");
    assert_eq!(finished["status"], "failed");
}

#[test]
fn stale_token_form_preserves_draft_and_requires_explicit_reload_after_external_edits() {
    use designllm_contracts::{Intent, Operation, TokenValues};

    let mut host = Host::start();
    let canvas = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
    host.apply(
        &canvas,
        vec![Operation::PutToken {
            token_id: "brand.ink".into(),
            name: "Brand ink".into(),
            values: TokenValues::Color { light: "#112233".into(), dark: "#445566".into() },
        }],
        [Intent::Design].into_iter().collect(),
    )
    .expect("the original token is stored");
    let mut recorder = Recorder::open(&mut host, &canvas);
    let original = host.artifact(&canvas);
    let ArtifactBody::Canvas(document) = &original.body else { panic!("a Canvas") };
    recorder.app.token_panel.load("brand.ink", document.tokens.get("brand.ink").unwrap(), original.revision);
    recorder.app.token_panel.set_name_for_test("Local draft");
    assert!(recorder.app.token_panel.has_local_draft(), "the user has unsaved input before the remote update");

    host.apply(
        &canvas,
        vec![Operation::PutToken {
            token_id: "brand.ink".into(),
            name: "External update".into(),
            values: TokenValues::Color { light: "#334455".into(), dark: "#667788".into() },
        }],
        [Intent::Design].into_iter().collect(),
    )
    .expect("another client updates the token");
    recorder.app.refresh_active();
    recorder.settle(&mut host);
    assert!(recorder.app.canvas_token_form_stale(), "the refreshed revision makes the open editor stale");
    assert_eq!(recorder.app.token_panel.name_for_test(), "Local draft", "the unsaved input stays in the form");
    recorder.app.publish_ui_if_due();
    recorder.settle(&mut host);
    let command_list = host
        .call("ui.command.list", json!({ "view_id": recorder.app.ui.view_id.clone() }))
        .expect("the refreshed command list");
    assert!(command_list["commands"].as_array().unwrap().iter().any(|item| item["id"] == "canvas.token.put" && item["enabled"] == false));
    let mark = recorder.calls.len();
    assert!(recorder
        .app
        .invoke(
            "canvas.token.put",
            json!({
                "token_id": "brand.ink", "name": "Local draft",
                "values": { "kind": "color", "light": "#AABBCC", "dark": "#DDEEFF" }
            }),
        )
        .is_err(), "a stale token form cannot overwrite the external update");
    recorder.settle(&mut host);
    assert!(!recorder.calls[mark..].iter().any(|call| call == "changes.apply:put_token"));
    let after_refusal = host.artifact(&canvas);
    assert_eq!(after_refusal.revision, original.revision + 1);
    let ArtifactBody::Canvas(external) = after_refusal.body else { panic!("a Canvas") };
    assert_eq!(external.tokens["brand.ink"].name, "External update");

    let latest = host.artifact(&canvas);
    let ArtifactBody::Canvas(document) = &latest.body else { panic!("a Canvas") };
    recorder.app.token_panel.load("brand.ink", document.tokens.get("brand.ink").unwrap(), latest.revision);
    assert!(!recorder.app.canvas_token_form_stale(), "explicit reload establishes the latest baseline");
    recorder.app.token_panel.set_name_for_test("After reload");
    recorder
        .app
        .invoke(
            "canvas.token.put",
            json!({
                "token_id": "brand.ink", "name": "After reload",
                "values": { "kind": "color", "light": "#AABBCC", "dark": "#DDEEFF" }
            }),
        )
        .expect("saving after explicit reload is accepted");
    recorder.settle(&mut host);
    let after_save = host.artifact(&canvas);
    let ArtifactBody::Canvas(saved) = &after_save.body else { panic!("a Canvas") };
    assert_eq!(saved.tokens["brand.ink"].name, "After reload");
    assert!(!recorder.app.canvas_token_form_stale(), "a successful local save advances the baseline after its reply");
    recorder.app.token_panel.set_name_for_test("Do not restore old token");
    assert!(recorder.app.token_panel.has_local_draft(), "there is a real local draft before the external deletion");

    host.apply(
        &canvas,
        vec![Operation::RemoveToken { token_id: "brand.ink".into() }],
        [Intent::Design].into_iter().collect(),
    )
    .expect("an external client removes the token");
    host.apply(
        &canvas,
        vec![Operation::PutToken {
            token_id: "brand.ink".into(),
            name: "Recreated externally".into(),
            values: TokenValues::Color { light: "#010203".into(), dark: "#040506".into() },
        }],
        [Intent::Design].into_iter().collect(),
    )
    .expect("the same id is recreated externally");
    recorder.app.refresh_active();
    recorder.settle(&mut host);
    assert!(recorder.app.canvas_token_form_stale(), "same-id recreation does not revive the old edit session");
    assert_eq!(recorder.app.token_panel.name_for_test(), "Do not restore old token", "the pending input survives the external replacement");
    let mark = recorder.calls.len();
    assert!(recorder
        .app
        .invoke(
            "canvas.token.put",
            json!({
                "token_id": "brand.ink", "name": "Do not restore old token",
                "values": { "kind": "color", "light": "#AABBCC", "dark": "#DDEEFF" }
            }),
        )
        .is_err(), "a stale form cannot overwrite a recreated token with the same id");
    recorder.settle(&mut host);
    assert!(!recorder.calls[mark..].iter().any(|call| call == "changes.apply:put_token"));
    let after_recreate = host.artifact(&canvas);
    let ArtifactBody::Canvas(recreated) = after_recreate.body else { panic!("a Canvas") };
    assert_eq!(recreated.tokens["brand.ink"].name, "Recreated externally");
}

#[test]
fn clean_token_panel_reloads_external_values_without_prompting() {
    use designllm_contracts::{Intent, Operation, TokenValues};

    let mut host = Host::start();
    let canvas = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
    host.apply(
        &canvas,
        vec![Operation::PutToken {
            token_id: "brand.ink".into(),
            name: "Brand ink".into(),
            values: TokenValues::Color { light: "#112233".into(), dark: "#445566".into() },
        }],
        [Intent::Design].into_iter().collect(),
    )
    .expect("the original token is stored");
    let mut recorder = Recorder::open(&mut host, &canvas);
    let original = host.artifact(&canvas);
    let ArtifactBody::Canvas(document) = &original.body else { panic!("a Canvas") };
    recorder.app.token_panel.load("brand.ink", document.tokens.get("brand.ink").unwrap(), original.revision);
    assert!(!recorder.app.token_panel.has_local_draft());

    host.apply(
        &canvas,
        vec![Operation::PutToken {
            token_id: "brand.ink".into(),
            name: "External clean update".into(),
            values: TokenValues::Color { light: "#334455".into(), dark: "#667788".into() },
        }],
        [Intent::Design].into_iter().collect(),
    )
    .expect("the other client changes the token");
    recorder.app.refresh_active();
    recorder.settle(&mut host);
    let _ = recorder.ctx.run(screen(1.0, 1280.0, 800.0), |ctx| {
        recorder.app.draw_frame(ctx);
    });
    recorder.settle(&mut host);

    assert_eq!(recorder.app.token_panel.name_for_test(), "External clean update");
    assert!(!recorder.app.canvas_token_form_stale(), "a clean form adopts the current token after rendering");
}

#[test]
fn token_commands_wait_for_matching_context_and_inspect_revisions() {
    use designllm_contracts::{Intent, Operation, TokenValues};

    let mut host = Host::start();
    let canvas = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
    host.apply(
        &canvas,
        vec![Operation::PutToken {
            token_id: "brand.ink".into(),
            name: "Brand ink".into(),
            values: TokenValues::Color { light: "#112233".into(), dark: "#445566".into() },
        }],
        [Intent::Design].into_iter().collect(),
    )
    .expect("the token is ready");
    let mut recorder = Recorder::open(&mut host, &canvas);
    let old_inspect = host.call("artifact.inspect", json!({ "artifact_id": canvas })).expect("the earlier inspect snapshot");
    let old_revision = old_inspect["revision"].as_u64().expect("the earlier revision");
    let ArtifactBody::Canvas(document) = &recorder.app.artifact.as_ref().unwrap().body else { panic!("a Canvas") };
    let baseline = document.tokens.get("brand.ink").unwrap().clone();
    recorder.app.token_panel.load("brand.ink", &baseline, old_revision);
    recorder.app.token_panel.set_name_for_test("Draft from old view");

    host.apply(
        &canvas,
        vec![Operation::PutToken {
            token_id: "brand.ink".into(),
            name: "Latest remote value".into(),
            values: TokenValues::Color { light: "#334455".into(), dark: "#667788".into() },
        }],
        [Intent::Design].into_iter().collect(),
    )
    .expect("another client changes the token");
    let view_id = recorder.app.view_id.clone();
    let context_params = json!({ "artifact_id": canvas, "view_id": view_id, "include_render": false });
    let current_context = host.call("context.read", context_params.clone()).expect("the newer context");
    let context_ticket = recorder
        .app
        .call("context.read", context_params, super::PendingAction::Context)
        .expect("context request queued");
    let inspect_ticket = recorder
        .app
        .call("artifact.inspect", json!({ "artifact_id": canvas }), super::PendingAction::Inspect)
        .expect("inspect request queued");
    let _queued = recorder.work.try_iter().collect::<Vec<_>>();

    recorder
        .replies
        .send(RpcReply { ticket: context_ticket, result: Ok(RpcPayload::Json(current_context)) })
        .expect("the window receives the current context first");
    recorder.app.process_replies(&recorder.ctx);
    assert_eq!(recorder.app.context.as_ref().unwrap().revision, old_revision + 1);
    assert_eq!(recorder.app.artifact.as_ref().unwrap().revision, old_revision, "the old inspect is still visible");
    assert!(recorder.app.canvas_token_form_stale(), "mismatched context/artifact snapshots disable token edits");

    recorder
        .replies
        .send(RpcReply { ticket: inspect_ticket, result: Ok(RpcPayload::Json(old_inspect)) })
        .expect("the delayed old inspect arrives");
    recorder.app.process_replies(&recorder.ctx);
    assert!(recorder.app.canvas_token_form_stale(), "the delayed inspect cannot make the newer context editable");
    let mark = recorder.calls.len();
    assert!(recorder
        .app
        .invoke(
            "canvas.token.put",
            json!({
                "token_id": "brand.ink", "name": "Draft from old view",
                "values": { "kind": "color", "light": "#AABBCC", "dark": "#DDEEFF" }
            }),
        )
        .is_err());
    assert!(!recorder.calls[mark..].iter().any(|call| call == "changes.apply:put_token"));

    recorder.app.fetch_inspect();
    let fresh_inspect_ticket = recorder.work.try_iter().find_map(|item| match item {
        Work::Call { ticket, method, .. } if method == "artifact.inspect" => Some(ticket),
        _ => None,
    }).expect("a fresh inspect is queued");
    let fresh_inspect = host.call("artifact.inspect", json!({ "artifact_id": canvas })).expect("the current inspect");
    recorder
        .replies
        .send(RpcReply { ticket: fresh_inspect_ticket, result: Ok(RpcPayload::Json(fresh_inspect)) })
        .expect("the current inspect arrives");
    recorder.app.process_replies(&recorder.ctx);
    recorder.settle(&mut host);
    assert!(recorder.app.canvas_token_form_stale(), "matching snapshots still require reloading the changed token baseline");
    let final_artifact = host.artifact(&canvas);
    let ArtifactBody::Canvas(final_document) = final_artifact.body else { panic!("a Canvas") };
    assert_eq!(final_document.tokens["brand.ink"].name, "Latest remote value");
    assert_eq!(recorder.app.token_panel.name_for_test(), "Draft from old view");
}

#[test]
fn nested_group_property_and_multi_selection_use_shallow_views_but_ungroup_keeps_children() {
    use designllm_contracts::{Intent, Node, Operation};

    let mut host = Host::start();
    let canvas = host.create("canvas", "nested property projection");
    let page_id = match &host.artifact(&canvas).body {
        ArtifactBody::Canvas(document) => document.pages[0].id.clone(),
        _ => panic!("a Canvas"),
    };
    let leaves = (0..128)
        .map(|index| serde_json::json!({
            "id": format!("leaf-{index}"),
            "kind": "rectangle",
            "bounds": { "x": index * 1000, "y": 0, "width": 900, "height": 900 },
        }))
        .collect::<Vec<_>>();
    let group: Node = serde_json::from_value(serde_json::json!({
        "id": "outer.group", "kind": "group",
        "bounds": { "x": 0, "y": 0, "width": 500000, "height": 500000 },
        "children": [{
            "id": "inner.frame", "kind": "rectangle", "style": { "fill": "#123456" },
            "bounds": { "x": 1000, "y": 1000, "width": 400000, "height": 400000 },
            "layout": { "mode": "grid", "grid_columns": 2 },
            "children": leaves,
        }],
    }))
    .expect("a nested group with many descendants parses");
    host.apply(
        &canvas,
        vec![Operation::AddNode { page_id: page_id.clone(), parent_id: None, node: group }],
        [Intent::Content, Intent::Design].into_iter().collect(),
    )
    .expect("the nested group is added");

    let mut recorder = Recorder::open(&mut host, &canvas);
    recorder.step(&mut host, "select.node", json!({ "node_id": "inner.frame" })).expect("the nested frame is selected");
    let frame = recorder.app.selected_node_properties().expect("a shallow selected-node projection");
    assert_eq!(frame.id, "inner.frame");
    assert!(frame.has_children, "the property panel retains the child-presence flag");
    assert_eq!(frame.style.fill, "#123456");
    for _ in 0..2 {
        let _ = recorder.ctx.run(screen(1.0, 1280.0, 800.0), |ctx| {
            recorder.app.draw_frame(ctx);
        });
        recorder.settle(&mut host);
    }

    recorder
        .step(&mut host, "select.many", json!({ "node_ids": ["leaf-0", "leaf-1"] }))
        .expect("nested siblings are multi-selected");
    let selected = recorder.app.selected_nodes_properties();
    assert_eq!(selected.len(), 2);
    assert!(selected.iter().any(|node| node.id == "leaf-0"));
    assert!(selected.iter().any(|node| node.id == "leaf-1"));
    assert!(selected.iter().all(|node| !node.has_children));
    for _ in 0..2 {
        let _ = recorder.ctx.run(screen(1.0, 1280.0, 800.0), |ctx| {
            recorder.app.draw_frame(ctx);
        });
        recorder.settle(&mut host);
    }

    recorder.step(&mut host, "select.node", json!({ "node_id": "outer.group" })).expect("the parent group is selected");
    recorder.step(&mut host, "edit.ungroup", json!({})).expect("ungroup preserves the nested frame and all children");
    let after = host.artifact(&canvas);
    let ArtifactBody::Canvas(document) = after.body else { panic!("a Canvas") };
    let promoted = document.pages[0].nodes.iter().find(|node| node.id == "inner.frame").expect("the frame was promoted");
    assert_eq!(promoted.children.len(), 128, "the operation path still retains the complete child subtree");
    assert!(document.pages[0].nodes.iter().all(|node| node.id != "outer.group"), "only the wrapper is removed");
}

#[test]
fn ungroup_keeps_each_childs_style_layout_text_and_page_position() {
    use designllm_contracts::{Intent, Node, Operation};

    let mut host = Host::start();
    let canvas = host.create("canvas", "ungroup keeps properties");
    let page_id = match &host.artifact(&canvas).body {
        ArtifactBody::Canvas(document) => document.pages[0].id.clone(),
        _ => panic!("a Canvas"),
    };
    let group: Node = serde_json::from_value(json!({
        "id": "wrap", "kind": "group",
        "bounds": { "x": 50000, "y": 70000, "width": 500000, "height": 500000 },
        "children": [
            {
                "id": "styled", "kind": "rectangle", "style": { "fill": "#123456" },
                "bounds": { "x": 1000, "y": 2000, "width": 100000, "height": 50000 },
                "layout": { "mode": "grid", "grid_columns": 2 }
            },
            {
                "id": "plain", "kind": "text", "text": "가",
                "bounds": { "x": 200000, "y": 300000, "width": 150000, "height": 40000 }
            }
        ],
    }))
    .expect("a group with two children parses");
    host.apply(
        &canvas,
        vec![Operation::AddNode { page_id, parent_id: None, node: group }],
        [Intent::Content, Intent::Design].into_iter().collect(),
    )
    .expect("the group is added");

    let mut recorder = Recorder::open(&mut host, &canvas);
    recorder.step(&mut host, "select.node", json!({ "node_id": "wrap" })).expect("the group is selected");
    recorder.step(&mut host, "edit.ungroup", json!({})).expect("the group is ungrouped");

    let after = host.artifact(&canvas);
    let ArtifactBody::Canvas(document) = after.body else { panic!("a Canvas") };
    let nodes = &document.pages[0].nodes;
    assert!(nodes.iter().all(|node| node.id != "wrap"), "only the wrapper is removed");
    let styled = nodes.iter().find(|node| node.id == "styled").expect("the styled child is back on the page");
    assert_eq!(serde_json::to_value(&styled.kind).expect("kind"), json!("rectangle"));
    assert_eq!(styled.style.fill, "#123456", "the child keeps its own fill");
    assert_eq!(styled.layout.as_ref().expect("the child keeps its layout").mode, "grid");
    assert_eq!(serde_json::to_value(&styled.layout).expect("layout")["grid_columns"], json!(2));
    assert_eq!(
        (styled.bounds.x, styled.bounds.y, styled.bounds.width, styled.bounds.height),
        (51_000, 72_000, 100_000, 50_000),
        "the child's frame is its offset inside the group added to the group's origin",
    );
    let plain = nodes.iter().find(|node| node.id == "plain").expect("the text child is back on the page");
    assert_eq!(serde_json::to_value(plain).expect("node")["text"], json!("가"));
    assert_eq!((plain.bounds.x, plain.bounds.y, plain.bounds.width, plain.bounds.height), (250_000, 370_000, 150_000, 40_000));
}

fn canvas_with_second_page_for_drag(host: &mut Host) -> (String, String) {
    use designllm_contracts::{ArtifactBody, Intent, Operation, Page, Size};

    let canvas = canvas_with_three_boxes(host, [0, 1_000_000, 3_000_000]);
    let source_page_size = match &host.artifact(&canvas).body {
        ArtifactBody::Canvas(document) => document.pages[0].size,
        _ => panic!("a Canvas"),
    };
    let target_page_size = if source_page_size == Size::a4() { Size::slide() } else { Size::a4() };
    let target_page_id = designllm_contracts::new_id();
    host.apply(
        &canvas,
        vec![Operation::InsertPage {
            index: 1,
            page: Page::new(target_page_id.clone(), target_page_size),
        }],
        [Intent::Content].into_iter().collect(),
    )
    .expect("the second page is inserted");
    (canvas, target_page_id)
}

fn slides_with_second_page_for_drag(host: &mut Host) -> (String, String) {
    use designllm_contracts::{ArtifactBody, Intent, Node, Operation, Page};

    let slides = host.create("slides", "Cross-page refusal");
    let (source_page_id, page_size) = match &host.artifact(&slides).body {
        ArtifactBody::Slides(document) => (document.pages[0].id.clone(), document.pages[0].size),
        _ => panic!("Slides"),
    };
    let target_page_id = designllm_contracts::new_id();
    let node: Node = serde_json::from_value(json!({
        "id": "slide-node", "kind": "rectangle",
        "bounds": { "x": 100_000, "y": 100_000, "width": 500_000, "height": 300_000 },
        "style": { "fill": "#FFFFFF" }
    }))
    .expect("a slide shape");
    host.apply(
        &slides,
        vec![
            Operation::InsertPage {
                index: 1,
                page: Page::new(target_page_id.clone(), page_size),
            },
            Operation::AddNode { page_id: source_page_id, parent_id: None, node },
        ],
        [Intent::Content].into_iter().collect(),
    )
    .expect("the two-page Slides document is prepared");
    (slides, target_page_id)
}

fn render_drag_pages(recorder: &mut Recorder, host: &mut Host) -> Vec<(u32, eframe::egui::Rect)> {
    recorder.read_assets = true;
    recorder.app.pending.retain(|_, pending| pending.method != "asset.read");
    recorder.app.requested_pages.clear();
    recorder.app.page_textures.clear();
    recorder.app.page_transforms.clear();

    let mut layout = None;
    for _ in 0..3 {
        let _ = recorder.ctx.run(screen(1.0, 1600.0, 1000.0), |ctx| {
            layout = Some(recorder.app.draw_frame(ctx));
        });
        recorder.settle(host);
    }
    let board_rect = layout.expect("the board is laid out").center;
    let kind = recorder.app.active.as_ref().expect("an active artifact").kind;
    let sizes = recorder.app.page_sizes();
    let view = recorder.app.view_ref();
    let slots = crate::board::arrange(kind, &sizes, view.zoom);
    let origin = board_rect.min + view.pan;
    slots
        .iter()
        .map(|slot| (slot.page_index, crate::board::to_screen(slot.world, origin, view.zoom)))
        .collect()
}

fn begin_test_drag(recorder: &mut Recorder, host: &mut Host, node_id: &str, page_index: u32, page_rect: eframe::egui::Rect) -> eframe::egui::Pos2 {
    begin_test_drag_at_fraction(recorder, host, node_id, page_index, page_rect, (0.5, 0.5))
}

fn begin_test_drag_at_fraction(recorder: &mut Recorder, host: &mut Host, node_id: &str, page_index: u32, page_rect: eframe::egui::Rect, grab_fraction: (f64, f64)) -> eframe::egui::Pos2 {
    begin_test_drag_at_scale(recorder, host, node_id, page_index, page_rect, grab_fraction, 1.0)
}

/// `begin_test_drag_at_fraction` in a window whose display scale is `pixels_per_point`.
fn begin_test_drag_at_scale(recorder: &mut Recorder, host: &mut Host, node_id: &str, page_index: u32, page_rect: eframe::egui::Rect, grab_fraction: (f64, f64), pixels_per_point: f32) -> eframe::egui::Pos2 {
    let (page_size, node_bounds, frame) = {
        let artifact = recorder.app.artifact.as_ref().expect("the artifact is loaded");
        let page_size = match &artifact.body {
            ArtifactBody::Canvas(document) => document.pages[page_index as usize].size,
            ArtifactBody::Slides(document) => document.pages[page_index as usize].size,
            _ => panic!("a paged design document"),
        };
        let node_bounds = super::doc_tree::find_node(artifact, node_id).expect("the node is present").bounds;
        let frame = recorder
            .app
            .page_textures
            .get(&page_index)
            .and_then(|page| page.frames.get(node_id))
            .copied()
            .expect("the rendered node frame is available");
        (page_size, node_bounds, frame)
    };
    let center_x = (frame.x as f64 + frame.width as f64 * grab_fraction.0) / page_size.width as f64;
    let center_y = (frame.y as f64 + frame.height as f64 * grab_fraction.1) / page_size.height as f64;
    let start = eframe::egui::Pos2::new(
        page_rect.left() + center_x as f32 * page_rect.width(),
        page_rect.top() + center_y as f32 * page_rect.height(),
    );
    assert!(page_rect.contains(start), "the test begins inside the source page");
    recorder.app.start_drag_at_scale(node_id.to_owned(), page_index, node_bounds, page_size, page_rect, start, pixels_per_point);
    recorder.settle(host);
    assert!(recorder.app.drag.as_ref().is_some_and(|drag| drag.reservation_id.is_some()), "the Host grants the drag reservation");
    start
}

fn finish_test_drag(recorder: &mut Recorder, host: &mut Host, delta: eframe::egui::Vec2) {
    let drag = recorder.app.drag.as_mut().expect("the drag is active");
    drag.delta = delta;
    drag.released = true;
    recorder.app.commit_drag_if_reserved();
    recorder.settle(host);
}

fn canvas_page_of(host: &mut Host, artifact_id: &str, node_id: &str) -> Option<(usize, String, designllm_contracts::Rect)> {
    let artifact = host.artifact(artifact_id);
    let ArtifactBody::Canvas(document) = artifact.body else { panic!("Canvas") };
    document.pages.iter().enumerate().find_map(|(index, page)| {
        page.nodes.iter().find(|node| node.id == node_id).map(|node| (index, page.id.clone(), node.bounds))
    })
}

#[test]
fn canvas_cross_page_drag_moves_at_drop_coordinates_and_undoes_as_one_change() {
    let mut host = Host::start();
    let (canvas, target_page_id) = canvas_with_second_page_for_drag(&mut host);
    let original_location = canvas_page_of(&mut host, &canvas, "a").expect("source node on page one");
    let original_bounds = original_location.2;
    let initial_artifact = host.artifact(&canvas);
    let source_size = match &initial_artifact.body {
        ArtifactBody::Canvas(document) => document.pages[0].size,
        _ => panic!("Canvas"),
    };
    let revision_before = initial_artifact.revision;
    let mut recorder = Recorder::open(&mut host, &canvas);
    let page_rects = render_drag_pages(&mut recorder, &mut host);
    let source_rect = page_rects.iter().find(|(index, _)| *index == 0).unwrap().1;
    let target_rect = page_rects.iter().find(|(index, _)| *index == 1).unwrap().1;
    assert!(source_rect.right() < target_rect.left(), "Canvas pages have a clear drop target");
    let grab_fraction = (0.2, 0.8);
    let source_frame = recorder.app.page_textures[&0].frames["a"];
    let start = begin_test_drag_at_fraction(&mut recorder, &mut host, "a", 0, source_rect, grab_fraction);
    let before_drag_calls = recorder.calls.len();

    let drop_fraction = (0.72, 0.28);
    let drop = eframe::egui::Pos2::new(
        target_rect.left() + target_rect.width() * drop_fraction.0 as f32,
        target_rect.top() + target_rect.height() * drop_fraction.1 as f32,
    );
    finish_test_drag(&mut recorder, &mut host, drop - start);
    assert!(recorder.calls[before_drag_calls..].iter().any(|call| call == "changes.apply:move_node"));
    assert!(recorder.calls[before_drag_calls..].iter().any(|call| call == "changes.apply:set_geometry"));
    let after_drag = host.artifact(&canvas);
    assert_eq!(after_drag.revision, revision_before + 1, "MoveNode and SetGeometry are one undoable change");
    let (page_index, page_id, moved_bounds) = canvas_page_of(&mut host, &canvas, "a").expect("the node remains present");
    assert_eq!((page_index, page_id.as_str()), (1, target_page_id.as_str()));
    let target_size = match &after_drag.body {
        ArtifactBody::Canvas(document) => document.pages[1].size,
        _ => panic!("Canvas"),
    };
    assert_ne!((source_size.width, source_size.height), (target_size.width, target_size.height), "page dimensions and aspect ratios differ");
    let expected_x = (drop_fraction.0 * target_size.width as f64).round() as i64 - (grab_fraction.0 * source_frame.width as f64).round() as i64;
    let expected_y = (drop_fraction.1 * target_size.height as f64).round() as i64 - (grab_fraction.1 * source_frame.height as f64).round() as i64;
    assert!((moved_bounds.x - expected_x).abs() <= 3, "x follows the non-center grab offset: {moved_bounds:?}, expected {expected_x}");
    assert!((moved_bounds.y - expected_y).abs() <= 3, "y follows the non-center grab offset: {moved_bounds:?}, expected {expected_y}");
    assert_eq!(recorder.app.selected_node.as_deref(), Some("a"), "the moved node stays selected");

    recorder.step(&mut host, "edit.undo", json!({})).expect("the cross-page drag is undone");
    assert_eq!(host.artifact(&canvas).revision, after_drag.revision + 1);
    let restored = canvas_page_of(&mut host, &canvas, "a").expect("the node returns to its source page");
    assert_eq!(restored.0, original_location.0);
    assert_eq!(restored.1, original_location.1);
    assert_eq!(restored.2, original_bounds);
}

#[test]
fn canvas_cross_page_drag_does_not_treat_a_page_gap_as_a_target() {
    let mut host = Host::start();
    let (canvas, _) = canvas_with_second_page_for_drag(&mut host);
    let original_location = canvas_page_of(&mut host, &canvas, "a").expect("source node");
    let source_index = original_location.0;
    let original_bounds = original_location.2;
    let revision = host.artifact(&canvas).revision;
    let mut recorder = Recorder::open(&mut host, &canvas);
    let page_rects = render_drag_pages(&mut recorder, &mut host);
    let source_rect = page_rects.iter().find(|(index, _)| *index == 0).unwrap().1;
    let target_rect = page_rects.iter().find(|(index, _)| *index == 1).unwrap().1;
    let gap = eframe::egui::Pos2::new((source_rect.right() + target_rect.left()) / 2.0, source_rect.center().y);
    assert!(recorder.app.page_drop_target(gap).is_none(), "the gutter is not a page");
    let start = begin_test_drag(&mut recorder, &mut host, "a", 0, source_rect);
    let before_drag_calls = recorder.calls.len();

    finish_test_drag(&mut recorder, &mut host, gap - start);
    assert!(recorder.calls[before_drag_calls..].iter().all(|call| !call.starts_with("changes.apply:")), "an out-of-page drop sends no change set");
    assert_eq!(host.artifact(&canvas).revision, revision);
    let after = canvas_page_of(&mut host, &canvas, "a").expect("the source node remains");
    assert_eq!(after.0, source_index);
    assert_eq!(after.1, original_location.1);
    assert_eq!(after.2, original_bounds);
}

#[test]
fn canvas_cross_page_drag_of_a_locked_node_is_atomic() {
    use designllm_contracts::{Intent, Operation};

    let mut host = Host::start();
    let (canvas, _) = canvas_with_second_page_for_drag(&mut host);
    host.apply(
        &canvas,
        vec![Operation::SetNodeAppearance { node_ids: vec!["a".into()], image_fit: None, locked: Some(true) }],
        [Intent::Design].into_iter().collect(),
    )
    .expect("the source node is locked");
    let revision = host.artifact(&canvas).revision;
    let original = canvas_page_of(&mut host, &canvas, "a").expect("locked node on page one");
    let mut recorder = Recorder::open(&mut host, &canvas);
    let page_rects = render_drag_pages(&mut recorder, &mut host);
    let source_rect = page_rects.iter().find(|(index, _)| *index == 0).unwrap().1;
    let target_rect = page_rects.iter().find(|(index, _)| *index == 1).unwrap().1;
    let start = begin_test_drag(&mut recorder, &mut host, "a", 0, source_rect);
    let before_drag_calls = recorder.calls.len();

    finish_test_drag(&mut recorder, &mut host, target_rect.center() - start);
    assert!(recorder.calls[before_drag_calls..].iter().any(|call| call == "changes.apply:move_node"));
    assert!(recorder.calls[before_drag_calls..].iter().any(|call| call == "changes.apply:set_geometry"));
    assert_eq!(host.artifact(&canvas).revision, revision, "a refused operation batch changes no revision");
    assert_eq!(canvas_page_of(&mut host, &canvas, "a"), Some(original), "the locked node stays on its original page with its original frame");
    let current = host.artifact(&canvas);
    let node = super::doc_tree::find_node(&current, "a").expect("the node remains present");
    assert_eq!(node.ext.locked, Some(true));
    assert!(recorder.app.refusal.is_some(), "the Host refusal is visible in the window");
}

#[test]
fn slides_cross_page_drag_keeps_the_existing_move_node_refusal() {
    use designllm_contracts::ArtifactBody;

    let mut host = Host::start();
    let (slides, target_page_id) = slides_with_second_page_for_drag(&mut host);
    let revision = host.artifact(&slides).revision;
    let mut recorder = Recorder::open(&mut host, &slides);
    let page_rects = render_drag_pages(&mut recorder, &mut host);
    let source_rect = page_rects.iter().find(|(index, _)| *index == 0).unwrap().1;
    let target_rect = page_rects.iter().find(|(index, _)| *index == 1).unwrap().1;
    let start = begin_test_drag(&mut recorder, &mut host, "slide-node", 0, source_rect);
    let before_drag_calls = recorder.calls.len();

    finish_test_drag(&mut recorder, &mut host, target_rect.center() - start);
    assert!(recorder.calls[before_drag_calls..].iter().any(|call| call == "changes.apply:move_node"));
    assert_eq!(host.artifact(&slides).revision, revision, "Slides refuses MoveNode without a partial geometry change");
    let current = host.artifact(&slides);
    let ArtifactBody::Slides(document) = current.body else { panic!("Slides") };
    assert!(document.pages[0].nodes.iter().any(|node| node.id == "slide-node"));
    assert_eq!(document.pages[1].id, target_page_id);
    assert!(document.pages[1].nodes.is_empty());
    assert!(recorder.app.refusal.is_some(), "the existing engine refusal remains visible");
}

#[test]
fn job_bound_canvas_move_node_can_cross_pages_with_geometry() {
    use designllm_contracts::{ChangeSet, Intent, JobRef, Operation};

    let mut host = Host::start();
    let (canvas, target_page_id) = canvas_with_second_page_for_drag(&mut host);
    let original = host.artifact(&canvas);
    let (_, _, source_bounds) = canvas_page_of(&mut host, &canvas, "a").expect("source node");
    let target_size = match &original.body {
        ArtifactBody::Canvas(document) => document.pages[1].size,
        _ => panic!("Canvas"),
    };
    let target_bounds = designllm_contracts::Rect {
        x: target_size.width as i64 / 2 - source_bounds.width / 2,
        y: target_size.height as i64 / 2 - source_bounds.height / 2,
        ..source_bounds
    };
    let job: designllm_contracts::Job = serde_json::from_value(
        host.call(
            "job.start",
            json!({
                "request_id": designllm_contracts::new_id(),
                "artifact_id": &canvas,
                "base_revision": original.revision,
                "allowed_intents": ["content", "design"],
                "runtime": "mock",
                "model": "cross-page-test",
                "effort": "low",
                "prompt": "Move the node to the second page.",
            }),
        )
        .expect("a queued mock job starts"),
    )
    .expect("Job response parses");
    let change = ChangeSet {
        request_id: designllm_contracts::new_id(),
        actor: "cross-page-agent".into(),
        workspace_id: host.workspace_id.clone(),
        artifact_id: canvas.clone(),
        base_revision: original.revision,
        allowed_intents: [Intent::Content, Intent::Design].into_iter().collect(),
        job_ref: Some(JobRef { job_id: job.id, attempt_id: job.attempt_id }),
        read_dependencies: Vec::new(),
        operations: vec![
            Operation::MoveNode { node_id: "a".into(), page_id: target_page_id.clone(), parent_id: None, index: 0 },
            Operation::SetGeometry { node_id: "a".into(), bounds: target_bounds },
        ],
    };
    host.call("changes.apply", serde_json::to_value(change).expect("job change serializes")).expect("the job uses the same MoveNode and geometry operations");
    assert_eq!(canvas_page_of(&mut host, &canvas, "a"), Some((1, target_page_id, target_bounds)));
}

#[test]
fn shared_design_system_commands_list_link_bind_and_return_group_result() {
    use designllm_contracts::{Intent, Operation, TokenRef, TokenSource, TokenValues};

    let mut host = Host::start();
    let publisher = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
    let consumer = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
    host.apply(
        &publisher,
        vec![Operation::PutToken {
            token_id: "brand.ink".into(),
            name: "Brand ink".into(),
            values: TokenValues::Color { light: "#112233".into(), dark: "#445566".into() },
        }],
        [Intent::Design].into_iter().collect(),
    )
    .expect("the publisher token is defined");

    let mut recorder = Recorder::open(&mut host, &publisher);
    recorder.app.publish_ui_if_due();
    recorder.settle(&mut host);
    let view_id = recorder.app.ui.view_id.clone();
    let listed_commands = host.call("ui.command.list", json!({ "view_id": view_id.clone() })).expect("the Canvas UI registry");
    for command in ["canvas.design_system.list", "canvas.design_system.publish", "canvas.design_system.set"] {
        assert!(
            listed_commands["commands"].as_array().unwrap().iter().any(|item| item["id"] == command && item["enabled"] == true),
            "{command} is available through the GUI and MCP command channel"
        );
    }
    let set_schema = listed_commands["commands"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["id"] == "canvas.design_system.set")
        .expect("the connection command is listed")["args_schema"]
        .clone();
    assert!(
        set_schema["properties"]["design_system"]["oneOf"]
            .as_array()
            .unwrap()
            .iter()
            .any(|alternative| alternative["type"] == "null"),
        "MCP clients see the null detach case in the published command schema"
    );
    assert_eq!(set_schema["properties"]["expected_system_revision"]["type"], "integer");
    assert!(set_schema["required"].as_array().unwrap().iter().all(|field| field != "expected_system_revision"));

    let (publish_run, _) = queue_ui_design_run(&mut recorder, &mut host, "canvas.design_system.publish", json!({ "publish": true }));
    let calls = deliver_ui_runs_until_rpc(&mut recorder, &mut host, "design_system.publish");
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].1["artifact_id"], publisher);
    assert_eq!(calls[0].1["allowed_intents"], json!(["design"]));
    let published = host.call("design_system.publish", calls[0].1.clone());
    assert!(published.is_ok(), "the shared system is published through the Core API");
    answer_ui_apply_reply(&mut recorder, &mut host, calls[0].0, published);
    let finished = host.call("ui.command.get", json!({ "run_id": publish_run })).expect("the publish run completes");
    assert_eq!(finished["status"], "completed");
    assert_eq!(finished["result"]["core_result"]["revision"], host.artifact(&publisher).revision);

    recorder.app.publish_ui_if_due();
    recorder.settle(&mut host);
    let view_id = recorder.app.ui.view_id.clone();
    let registry = host.call("ui.command.list", json!({ "view_id": view_id.clone() })).expect("the updated registry");
    let state_seq = registry["state_seq"].as_u64().expect("published state sequence");
    let queued = host
        .call(
            "ui.command.run",
            json!({
                "view_id": view_id,
                "expected_state_seq": state_seq,
                "command_id": "canvas.design_system.list",
                "actor": "design-system-list-test",
                "permission": "read",
                "allowed_intents": [],
                "args": {},
            }),
        )
        .expect("the workspace list command is queued");
    let list_run = queued["run_id"].as_str().expect("run id").to_owned();
    let calls = deliver_ui_runs_until_rpc(&mut recorder, &mut host, "design_system.list");
    assert_eq!(calls.len(), 1);
    let systems = host.call("design_system.list", calls[0].1.clone()).expect("the published source list");
    assert!(systems["systems"].as_array().unwrap().iter().any(|item| item["artifact_id"] == publisher));
    answer_ui_apply_reply(&mut recorder, &mut host, calls[0].0, Ok(systems));
    let list_finished = host.call("ui.command.get", json!({ "run_id": list_run })).expect("the read run completes");
    assert_eq!(list_finished["status"], "completed");
    assert!(list_finished["result"]["core_result"]["systems"].as_array().is_some());

    recorder.step(&mut host, "nav.open_project", json!({ "artifact_id": consumer })).expect("the consumer opens");
    let stale_source_revision = recorder
        .app
        .token_panel
        .design_systems
        .iter()
        .find(|system| system.artifact_id == publisher)
        .expect("the consumer lists the published source")
        .revision;
    host.apply(
        &publisher,
        vec![Operation::PutToken {
            token_id: "brand.ink".into(),
            name: "Brand ink".into(),
            values: TokenValues::Color { light: "#667788".into(), dark: "#99AABB".into() },
        }],
        [Intent::Design].into_iter().collect(),
    )
    .expect("an external publisher update makes the cached source revision stale");
    let current_source_revision = host.artifact(&publisher).revision;
    assert_ne!(stale_source_revision, current_source_revision);

    let direct_start = recorder.calls.len();
    recorder
        .step(
            &mut host,
            "canvas.design_system.set",
            json!({ "design_system": publisher, "expected_system_revision": stale_source_revision }),
        )
        .expect("the direct UI request is sent");
    let direct_sets = recorder.calls[direct_start..].iter().filter(|method| method.as_str() == "design_system.set").count();
    assert_eq!(direct_sets, 1, "a direct stale set is refused once without an automatic retry");
    assert!(host.artifact(&consumer).design_system.is_none(), "the refused direct set does not link the consumer");
    assert_eq!(
        recorder.app.token_panel.design_systems.iter().find(|system| system.artifact_id == publisher).unwrap().revision,
        current_source_revision,
        "the direct refusal refreshes the source list for an explicit retry"
    );

    let stale_queued_revision = recorder
        .app
        .token_panel
        .design_systems
        .iter()
        .find(|system| system.artifact_id == publisher)
        .expect("the refreshed source remains visible")
        .revision;
    host.apply(
        &publisher,
        vec![Operation::PutToken {
            token_id: "brand.ink".into(),
            name: "Brand ink".into(),
            values: TokenValues::Color { light: "#778899".into(), dark: "#AABBCC".into() },
        }],
        [Intent::Design].into_iter().collect(),
    )
    .expect("another external update makes the queued source revision stale");
    let queued_current_source_revision = host.artifact(&publisher).revision;
    let (set_run, _) = queue_ui_design_run(
        &mut recorder,
        &mut host,
        "canvas.design_system.set",
        json!({ "design_system": publisher, "expected_system_revision": stale_queued_revision }),
    );
    let calls = deliver_ui_runs_until_rpc(&mut recorder, &mut host, "design_system.set");
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].1["expected_system_revision"], stale_queued_revision);
    let linked = host.call("design_system.set", calls[0].1.clone());
    assert_eq!(linked.as_ref().unwrap_err().code, designllm_contracts::ErrorCode::RevisionConflict);
    let queued_set_calls = recorder.calls.iter().filter(|method| method.as_str() == "design_system.set").count();
    answer_ui_apply_reply(&mut recorder, &mut host, calls[0].0, linked);
    assert_eq!(recorder.calls.iter().filter(|method| method.as_str() == "design_system.set").count(), queued_set_calls, "the queued refusal also has no automatic retry");
    assert_eq!(host.call("ui.command.get", json!({ "run_id": set_run })).unwrap()["status"], "failed");
    assert_eq!(
        recorder.app.token_panel.design_systems.iter().find(|system| system.artifact_id == publisher).unwrap().revision,
        queued_current_source_revision,
        "a stale source refusal refreshes the list without silently retrying the link"
    );

    let (set_run, _) = queue_ui_design_run(
        &mut recorder,
        &mut host,
        "canvas.design_system.set",
        json!({ "design_system": publisher, "expected_system_revision": queued_current_source_revision }),
    );
    let calls = deliver_ui_runs_until_rpc(&mut recorder, &mut host, "design_system.set");
    assert_eq!(calls.len(), 1);
    let linked = host.call("design_system.set", calls[0].1.clone());
    assert!(linked.is_ok(), "the source revision is checked while the consumer is linked");
    answer_ui_apply_reply(&mut recorder, &mut host, calls[0].0, linked);
    let set_finished = host.call("ui.command.get", json!({ "run_id": set_run })).expect("the set run completes");
    assert_eq!(set_finished["status"], "completed");
    assert_eq!(host.artifact(&consumer).design_system.as_ref().unwrap().artifact_id, publisher);
    assert!(recorder.app.token_panel.shared_source_loaded, "the source token snapshot is read at the pinned revision");
    assert!(recorder.app.token_panel.shared_tokens.contains_key("brand.ink"));

    recorder.step(&mut host, "select.node", json!({ "node_id": "a" })).expect("the consumer node is selected");
    let (bind_run, _) = queue_ui_design_run(
        &mut recorder,
        &mut host,
        "canvas.token.bind",
        json!({ "token_id": "brand.ink", "source": "shared", "target": "style_fill" }),
    );
    let calls = deliver_ui_runs_until_apply(&mut recorder, &mut host);
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].1["operations"][0]["token"]["source"], "shared");
    let bound = host.call("changes.apply", calls[0].1.clone());
    assert!(bound.is_ok(), "a selected node accepts a shared token binding");
    answer_ui_apply_reply(&mut recorder, &mut host, calls[0].0, bound);
    assert_eq!(host.call("ui.command.get", json!({ "run_id": bind_run })).unwrap()["status"], "completed");
    let consumer_artifact = host.artifact(&consumer);
    let designllm_contracts::ArtifactBody::Canvas(consumer_doc) = consumer_artifact.body else { panic!("the consumer is Canvas") };
    assert!(consumer_doc.bindings.iter().any(|binding| binding.node_id == "a" && binding.token == (TokenRef { source: TokenSource::Shared, token_id: "brand.ink".into() })));

    recorder.step(&mut host, "nav.open_project", json!({ "artifact_id": publisher })).expect("the publisher reopens");
    let (put_run, _) = queue_ui_design_run(
        &mut recorder,
        &mut host,
        "canvas.token.put",
        json!({
            "token_id": "brand.ink", "name": "Brand ink",
            "values": { "kind": "color", "light": "#AABBCC", "dark": "#DDEEFF" }
        }),
    );
    let calls = deliver_ui_runs_until_apply(&mut recorder, &mut host);
    assert_eq!(calls.len(), 1);
    let updated = host.call("changes.apply", calls[0].1.clone());
    assert!(updated.is_ok(), "the publisher token update synchronizes the consumer");
    let result = updated.as_ref().unwrap();
    assert_eq!(result["group"]["members"].as_array().unwrap().len(), 2);
    answer_ui_apply_reply(&mut recorder, &mut host, calls[0].0, updated);
    let put_finished = host.call("ui.command.get", json!({ "run_id": put_run })).expect("the grouped token run completes");
    assert_eq!(put_finished["status"], "completed");
    assert_eq!(put_finished["result"]["core_result"]["group"]["members"].as_array().unwrap().len(), 2);
    let consumer_after_sync = host.artifact(&consumer);
    assert_eq!(consumer_after_sync.design_system.as_ref().unwrap().revision, host.artifact(&publisher).revision);
    assert_eq!(super::doc_tree::find_node(&consumer_after_sync, "a").unwrap().style.fill, "#AABBCC");

    recorder.step(&mut host, "nav.open_project", json!({ "artifact_id": consumer })).expect("the consumer reopens");
    let (detach_run, _) = queue_ui_design_run(&mut recorder, &mut host, "canvas.design_system.set", json!({ "design_system": null }));
    let calls = deliver_ui_runs_until_rpc(&mut recorder, &mut host, "design_system.set");
    assert_eq!(calls.len(), 1);
    assert!(calls[0].1["design_system"].is_null());
    assert!(calls[0].1.get("expected_system_revision").is_none(), "detach omits the publisher-only revision guard");
    let detached = host.call("design_system.set", calls[0].1.clone());
    assert!(detached.is_ok(), "the public UI schema and dispatcher support an explicit null detach");
    answer_ui_apply_reply(&mut recorder, &mut host, calls[0].0, detached);
    assert_eq!(host.call("ui.command.get", json!({ "run_id": detach_run })).unwrap()["status"], "completed");
    let detached_artifact = host.artifact(&consumer);
    assert!(detached_artifact.design_system.is_none());
    let designllm_contracts::ArtifactBody::Canvas(detached_doc) = &detached_artifact.body else { panic!("the detached artifact is Canvas") };
    assert!(detached_doc.bindings.is_empty(), "shared bindings are detached atomically");
    assert_eq!(super::doc_tree::find_node(&detached_artifact, "a").unwrap().style.fill, "#AABBCC", "concrete styling survives detach");
}

#[test]
fn undo_and_redo_of_publish_refresh_the_shared_system_list() {
    use designllm_contracts::{Intent, Operation, TokenValues};

    let mut host = Host::start();
    let publisher = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
    host.apply(
        &publisher,
        vec![Operation::PutToken {
            token_id: "brand.ink".into(),
            name: "Brand ink".into(),
            values: TokenValues::Color { light: "#112233".into(), dark: "#445566".into() },
        }],
        [Intent::Design].into_iter().collect(),
    )
    .expect("the publisher token is defined");
    let mut recorder = Recorder::open(&mut host, &publisher);

    recorder.step(&mut host, "canvas.design_system.publish", json!({ "publish": true })).expect("the source is published");
    assert!(recorder.app.token_panel.design_systems.iter().any(|system| system.artifact_id == publisher));
    recorder.step(&mut host, "canvas.design_system.publish", json!({ "publish": false })).expect("the unused source is unpublished");
    assert!(recorder.app.token_panel.design_systems.iter().all(|system| system.artifact_id != publisher));

    recorder.step(&mut host, "edit.undo", json!({})).expect("undo republishes the source");
    assert!(recorder.app.token_panel.design_systems.iter().any(|system| system.artifact_id == publisher));
    recorder.step(&mut host, "edit.redo", json!({})).expect("redo unpublishes the source again");
    assert!(recorder.app.token_panel.design_systems.iter().all(|system| system.artifact_id != publisher));
}

fn complete_queued_table_change(recorder: &mut Recorder, host: &mut Host, command: &str, args: Value, artifact_id: &str) -> Value {
    let (run_id, _) = queue_ui_content_run(recorder, host, command, args);
    let applies = deliver_ui_runs_until_apply(recorder, host);
    assert_eq!(applies.len(), 1, "one table command is one ChangeSet");
    let (ticket, params) = applies.into_iter().next().unwrap();
    let waiting = host.call("ui.command.get", json!({ "run_id": &run_id })).expect("the table run is queryable");
    assert_eq!(waiting["status"], "delivered", "the command waits for the actual apply reply");
    let core_reply = host.call("changes.apply", params);
    assert!(core_reply.is_ok(), "Core applies {command}: {:?}", core_reply.as_ref().err());
    let core_result = core_reply.as_ref().unwrap().clone();
    answer_ui_apply_reply(recorder, host, ticket, core_reply);
    let finished = host.call("ui.command.get", json!({ "run_id": &run_id })).expect("the completed table run is queryable");
    assert_eq!(finished["status"], "completed");
    assert_eq!(finished["result"]["command"], command);
    assert_eq!(finished["result"]["core_result"], core_result, "the UI channel returns the actual ChangeResult");
    assert_eq!(host.artifact(artifact_id).revision, core_result["revision"].as_u64().unwrap());
    core_result
}

#[test]
fn queued_canvas_table_commands_wait_for_apply_and_keep_flow_table_behavior() {
    use designllm_contracts::ArtifactBody;

    // The UI channel follows each structure edit to its one reply because the registry declares it one change set, not by
    // a list of names of its own (backlog desktop 55).
    for id in ["canvas.table.add_row", "canvas.table.remove_row", "canvas.table.add_column", "canvas.table.remove_column"] {
        assert!(crate::commands::one_tracked_change(id), "{id} is declared as one change set");
    }
    let mut host = Host::start();
    let canvas = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
    let mut recorder = Recorder::open(&mut host, &canvas);
    recorder.app.publish_ui_if_due();
    recorder.settle(&mut host);
    let listed = host.call("ui.command.list", json!({ "view_id": recorder.app.ui.view_id.clone() })).expect("the Canvas registry");
    let enabled = |id: &str| listed["commands"].as_array().unwrap().iter().find(|item| item["id"] == id).and_then(|item| item["enabled"].as_bool()).unwrap_or(false);
    assert!(enabled("canvas.table.add"));
    assert!(!enabled("canvas.table.add_row"), "structure edits need a selected table");
    assert!(!enabled("add.table"), "Flow's table command is not advertised for Canvas");

    let add_result = complete_queued_table_change(&mut recorder, &mut host, "canvas.table.add", json!({ "rows": 2, "columns": 2 }), &canvas);
    let table_id = recorder.app.selected_node.clone().expect("table insertion selects the new table");
    let artifact = host.artifact(&canvas);
    let table_node = super::doc_tree::find_node(&artifact, &table_id).expect("the semantic table node");
    let table = table_node.ext.table.as_ref().expect("the new group has table metadata");
    assert_eq!(table.rows.len(), 2);
    assert_eq!(table.rows[0].cell_ids.len(), 2);
    assert_eq!(table.header_row_id.as_deref(), Some(table.rows[0].id.as_str()));
    assert_eq!(add_result["revision"], artifact.revision);

    let row_result = complete_queued_table_change(
        &mut recorder,
        &mut host,
        "canvas.table.add_row",
        json!({ "table_id": table_id, "index": 1 }),
        &canvas,
    );
    assert!(row_result["changed_ids"].as_array().unwrap().iter().any(|id| id == &table_id));
    let artifact = host.artifact(&canvas);
    let table = super::doc_tree::find_node(&artifact, &table_id).unwrap().ext.table.as_ref().unwrap();
    assert_eq!(table.rows.len(), 3);
    assert!(table.rows.iter().all(|row| row.cell_ids.len() == 2));
    assert_eq!(table.header_row_id.as_deref(), Some(table.rows[0].id.as_str()));

    complete_queued_table_change(
        &mut recorder,
        &mut host,
        "canvas.table.add_column",
        json!({ "table_id": table_id, "index": 2 }),
        &canvas,
    );
    let artifact = host.artifact(&canvas);
    assert!(super::doc_tree::find_node(&artifact, &table_id).unwrap().ext.table.as_ref().unwrap().rows.iter().all(|row| row.cell_ids.len() == 3));

    complete_queued_table_change(
        &mut recorder,
        &mut host,
        "canvas.table.set_header",
        json!({ "table_id": table_id, "enabled": false }),
        &canvas,
    );
    assert!(super::doc_tree::find_node(&host.artifact(&canvas), &table_id).unwrap().ext.table.as_ref().unwrap().header_row_id.is_none());

    complete_queued_table_change(
        &mut recorder,
        &mut host,
        "canvas.table.remove_row",
        json!({ "table_id": table_id, "index": 2 }),
        &canvas,
    );
    let artifact = host.artifact(&canvas);
    assert_eq!(super::doc_tree::find_node(&artifact, &table_id).unwrap().ext.table.as_ref().unwrap().rows.len(), 2);
    complete_queued_table_change(
        &mut recorder,
        &mut host,
        "canvas.table.remove_column",
        json!({ "table_id": table_id, "index": 2 }),
        &canvas,
    );
    let artifact = host.artifact(&canvas);
    assert!(super::doc_tree::find_node(&artifact, &table_id).unwrap().ext.table.as_ref().unwrap().rows.iter().all(|row| row.cell_ids.len() == 2));

    recorder.step(&mut host, "edit.undo", json!({})).expect("one Undo reverses the last table structure change");
    let artifact = host.artifact(&canvas);
    assert!(super::doc_tree::find_node(&artifact, &table_id).unwrap().ext.table.as_ref().unwrap().rows.iter().all(|row| row.cell_ids.len() == 3));

    let flow = document_with_paragraphs_and_table(&mut host);
    let mut flow_recorder = Recorder::open(&mut host, &flow);
    flow_recorder.app.publish_ui_if_due();
    flow_recorder.settle(&mut host);
    let flow_list = host.call("ui.command.list", json!({ "view_id": flow_recorder.app.ui.view_id.clone() })).expect("the Flow registry");
    let flow_enabled = |id: &str| flow_list["commands"].as_array().unwrap().iter().find(|item| item["id"] == id).and_then(|item| item["enabled"].as_bool()).unwrap_or(false);
    assert!(flow_enabled("add.table"));
    assert!(!flow_enabled("canvas.table.add"));
    let before_tables = match &host.artifact(&flow).body {
        ArtifactBody::Flow(document) => document.sections.iter().flat_map(|section| &section.blocks).filter(|block| matches!(block, designllm_contracts::FlowBlock::Table(_))).count(),
        _ => unreachable!(),
    };
    flow_recorder.step(&mut host, "add.table", json!({})).expect("the existing Flow table command remains connected");
    let after_tables = match &host.artifact(&flow).body {
        ArtifactBody::Flow(document) => document.sections.iter().flat_map(|section| &section.blocks).filter(|block| matches!(block, designllm_contracts::FlowBlock::Table(_))).count(),
        _ => unreachable!(),
    };
    assert_eq!(after_tables, before_tables + 1);

    let slides = host.create("slides", "슬라이드");
    let mut slide_recorder = Recorder::open(&mut host, &slides);
    slide_recorder.app.publish_ui_if_due();
    slide_recorder.settle(&mut host);
    let slide_list = host.call("ui.command.list", json!({ "view_id": slide_recorder.app.ui.view_id.clone() })).expect("the Slides registry");
    for id in ["canvas.table.add", "canvas.table.add_row", "canvas.table.remove_row", "canvas.table.add_column", "canvas.table.remove_column", "canvas.table.set_header"] {
        assert!(!slide_list["commands"].as_array().unwrap().iter().find(|item| item["id"] == id).unwrap()["enabled"].as_bool().unwrap(), "Slides must not advertise {id}");
    }
}

fn complete_queued_table_design_change(recorder: &mut Recorder, host: &mut Host, args: Value, artifact_id: &str) -> Value {
    let (run_id, _) = queue_ui_design_run(recorder, host, "canvas.table.set_column_widths", args);
    let applies = deliver_ui_runs_until_apply(recorder, host);
    assert_eq!(applies.len(), 1, "one width edit is one ChangeSet");
    let (ticket, params) = applies.into_iter().next().unwrap();
    let waiting = host.call("ui.command.get", json!({ "run_id": &run_id })).expect("the width run is queryable");
    assert_eq!(waiting["status"], "delivered", "the width run waits for Core");
    let core_reply = host.call("changes.apply", params);
    assert!(core_reply.is_ok(), "Core applies the width edit: {:?}", core_reply.as_ref().err());
    let core_result = core_reply.as_ref().unwrap().clone();
    answer_ui_apply_reply(recorder, host, ticket, core_reply);
    let finished = host.call("ui.command.get", json!({ "run_id": &run_id })).expect("the completed width run is queryable");
    assert_eq!(finished["status"], "completed");
    assert_eq!(finished["result"]["command"], "canvas.table.set_column_widths");
    assert_eq!(finished["result"]["core_result"], core_result, "the UI channel returns the actual ChangeResult");
    assert_eq!(host.artifact(artifact_id).revision, core_result["revision"].as_u64().unwrap());
    core_result
}

#[test]
fn canvas_table_widths_command_and_column_structure_preserve_relative_weights() {
    use designllm_contracts::{ChangeSet, Intent};

    let mut host = Host::start();
    let canvas = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
    let mut recorder = Recorder::open(&mut host, &canvas);
    recorder.step(&mut host, "canvas.table.add", json!({ "rows": 2, "columns": 2 })).expect("add Canvas table");
    let table_id = recorder.app.selected_node.clone().expect("new table selected");
    recorder.app.publish_ui_if_due();
    recorder.settle(&mut host);
    let listed = host.call("ui.command.list", json!({ "view_id": recorder.app.ui.view_id.clone() })).expect("Canvas table command registry");
    let width_command = listed["commands"].as_array().unwrap().iter().find(|command| command["id"] == "canvas.table.set_column_widths").unwrap();
    assert_eq!(width_command["permission"], "change");
    assert_eq!(width_command["intents"], json!(["design"]));
    assert_eq!(width_command["enabled"], true);
    assert_eq!(width_command["args_schema"]["properties"]["column_widths"]["type"], json!(["array", "null"]));
    assert_eq!(width_command["args_schema"]["properties"]["column_widths"]["items"]["minimum"], 1);
    let before_invalid = host.artifact(&canvas).revision;
    for invalid in [
        json!({ "table_id": table_id, "column_widths": "1,2" }),
        json!({ "table_id": table_id, "column_widths": [] }),
        json!({ "table_id": table_id, "column_widths": [1, 0] }),
        json!({ "table_id": table_id, "column_widths": [1] }),
    ] {
        let calls_before = recorder.calls.len();
        assert!(recorder.app.invoke("canvas.table.set_column_widths", invalid).is_err());
        assert!(!recorder.calls[calls_before..].iter().any(|call| call.starts_with("changes.apply")));
        assert_eq!(host.artifact(&canvas).revision, before_invalid, "invalid widths leave the document unchanged");
    }

    complete_queued_table_design_change(&mut recorder, &mut host, json!({ "table_id": table_id, "column_widths": [1, 2] }), &canvas);
    let artifact = host.artifact(&canvas);
    assert_eq!(super::doc_tree::find_node(&artifact, &table_id).unwrap().ext.table.as_ref().unwrap().column_widths.as_deref(), Some(&[1, 2][..]));

    let width_revision = artifact.revision;
    let content_only = host.call("changes.apply", serde_json::to_value(ChangeSet {
        request_id: designllm_contracts::new_id(),
        actor: "table-width-content-only".into(),
        workspace_id: host.workspace_id.clone(),
        artifact_id: canvas.clone(),
        base_revision: width_revision,
        allowed_intents: [Intent::Content].into_iter().collect(),
        job_ref: None,
        read_dependencies: Vec::new(),
        operations: vec![designllm_contracts::Operation::SetCanvasTableColumnWidths {
            table_id: table_id.clone(), expected: Some(vec![1, 2]), column_widths: Some(vec![2, 1]),
        }],
    }).unwrap()).unwrap_err();
    assert_eq!(content_only.code, designllm_contracts::ErrorCode::ContentLocked);
    assert_eq!(host.artifact(&canvas).revision, width_revision, "Content-only permission cannot change table widths");
    assert_eq!(super::doc_tree::find_node(&host.artifact(&canvas), &table_id).unwrap().ext.table.as_ref().unwrap().column_widths.as_deref(), Some(&[1, 2][..]));

    recorder.step(&mut host, "edit.undo", json!({})).expect("Design-only width edit can be undone");
    assert!(super::doc_tree::find_node(&host.artifact(&canvas), &table_id).unwrap().ext.table.as_ref().unwrap().column_widths.is_none());
    recorder.step(&mut host, "edit.redo", json!({})).expect("Design-only width edit can be redone");
    assert_eq!(super::doc_tree::find_node(&host.artifact(&canvas), &table_id).unwrap().ext.table.as_ref().unwrap().column_widths.as_deref(), Some(&[1, 2][..]));
    recorder.step(&mut host, "select.node", json!({ "node_id": table_id })).expect("table reselected after undo/redo");

    complete_queued_table_change(&mut recorder, &mut host, "canvas.table.add_column", json!({ "table_id": table_id, "index": 1 }), &canvas);
    let artifact = host.artifact(&canvas);
    assert_eq!(super::doc_tree::find_node(&artifact, &table_id).unwrap().ext.table.as_ref().unwrap().column_widths.as_deref(), Some(&[1, 1, 4][..]));

    complete_queued_table_change(&mut recorder, &mut host, "canvas.table.remove_column", json!({ "table_id": table_id, "index": 1 }), &canvas);
    let artifact = host.artifact(&canvas);
    assert_eq!(super::doc_tree::find_node(&artifact, &table_id).unwrap().ext.table.as_ref().unwrap().column_widths.as_deref(), Some(&[2, 4][..]));

    complete_queued_table_design_change(&mut recorder, &mut host, json!({ "table_id": table_id, "column_widths": null }), &canvas);
    assert!(super::doc_tree::find_node(&host.artifact(&canvas), &table_id).unwrap().ext.table.as_ref().unwrap().column_widths.is_none());
}

#[test]
fn canvas_table_commands_require_current_snapshot_capability_and_selected_target() {
    use designllm_contracts::{Feature, Support};

    let mut host = Host::start();
    let canvas = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
    let mut recorder = Recorder::open(&mut host, &canvas);
    recorder.step(&mut host, "canvas.table.add", json!({ "rows": 2, "columns": 2 })).expect("first table");
    let first_table = recorder.app.selected_node.clone().unwrap();
    recorder.step(&mut host, "canvas.table.add", json!({ "rows": 2, "columns": 2 })).expect("second table");
    let second_table = recorder.app.selected_node.clone().unwrap();
    assert_ne!(first_table, second_table);

    recorder.step(&mut host, "select.node", json!({ "node_id": first_table })).expect("the first table is selected");
    recorder.app.publish_ui_if_due();
    recorder.settle(&mut host);
    let listed = host.call("ui.command.list", json!({ "view_id": recorder.app.ui.view_id.clone() })).expect("the selected table enables structure commands");
    assert!(listed["commands"].as_array().unwrap().iter().any(|item| item["id"] == "canvas.table.add_row" && item["enabled"] == true));
    let revision = host.artifact(&canvas).revision;
    let run = queue_ui_content_run(
        &mut recorder,
        &mut host,
        "canvas.table.add_row",
        json!({ "table_id": second_table, "index": 1 }),
    ).0;
    let calls_before_delivery = recorder.calls.len();
    poll_queued_ui_runs(&mut recorder, &mut host);
    assert!(!recorder.calls[calls_before_delivery..].iter().any(|call| call == "changes.apply"), "a run cannot retarget another table by id");
    assert_eq!(host.artifact(&canvas).revision, revision);
    let refused = host.call("ui.command.get", json!({ "run_id": run })).expect("the mismatched target refusal");
    assert_eq!(refused["status"], "failed");
    let error: designllm_contracts::DltError = serde_json::from_value(refused["error"].clone()).unwrap();
    assert_eq!(error.code, designllm_contracts::ErrorCode::InvalidArgument);

    let before_calls = recorder.calls.len();
    recorder.app.context.as_mut().unwrap().revision = revision.saturating_add(1);
    let stale = recorder.app.invoke("canvas.table.add", json!({ "rows": 1, "columns": 1 }));
    assert!(stale.is_err(), "direct table insertion requires matching artifact/context revisions");
    assert!(!recorder.calls[before_calls..].iter().any(|call| call.starts_with("changes.apply")));
    assert_eq!(host.artifact(&canvas).revision, revision);

    let context = recorder.app.context.as_mut().unwrap();
    context.revision = revision;
    let tables = context.capabilities.get_mut(&Feature::Tables).expect("the Canvas table capability");
    tables.write = Support::Unsupported;
    tables.edit = Support::Supported;
    recorder.app.publish_ui_if_due();
    recorder.settle(&mut host);
    let capability_list = host.call("ui.command.list", json!({ "view_id": recorder.app.ui.view_id.clone() })).expect("capability-adjusted registry");
    assert!(!capability_list["commands"].as_array().unwrap().iter().find(|item| item["id"] == "canvas.table.add").unwrap()["enabled"].as_bool().unwrap());
    assert!(capability_list["commands"].as_array().unwrap().iter().find(|item| item["id"] == "canvas.table.add_row").unwrap()["enabled"].as_bool().unwrap());
    let before_calls = recorder.calls.len();
    assert!(recorder.app.invoke("canvas.table.add", json!({ "rows": 1, "columns": 1 })).is_err());
    assert!(!recorder.calls[before_calls..].iter().any(|call| call.starts_with("changes.apply")));

    let context = recorder.app.context.as_mut().unwrap();
    let tables = context.capabilities.get_mut(&Feature::Tables).unwrap();
    tables.write = Support::Supported;
    tables.edit = Support::Unsupported;
    recorder.app.publish_ui_if_due();
    recorder.settle(&mut host);
    let capability_list = host.call("ui.command.list", json!({ "view_id": recorder.app.ui.view_id.clone() })).expect("updated capability registry");
    assert!(capability_list["commands"].as_array().unwrap().iter().find(|item| item["id"] == "canvas.table.add").unwrap()["enabled"].as_bool().unwrap());
    assert!(!capability_list["commands"].as_array().unwrap().iter().find(|item| item["id"] == "canvas.table.add_row").unwrap()["enabled"].as_bool().unwrap());
    let before_calls = recorder.calls.len();
    assert!(recorder.app.invoke("canvas.table.add_row", json!({ "table_id": first_table, "index": 1 })).is_err());
    assert!(!recorder.calls[before_calls..].iter().any(|call| call.starts_with("changes.apply")));
}
#[test]
fn a_pending_canvas_table_add_does_not_replace_a_newer_user_selection() {
    let mut host = Host::start();
    let canvas = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
    let mut recorder = Recorder::open(&mut host, &canvas);
    recorder.step(&mut host, "select.node", json!({ "node_id": "a" })).expect("the original selection is set");
    let original_generation = recorder.app.selection_generation;
    let before_revision = host.artifact(&canvas).revision;
    recorder.app.invoke("canvas.table.add", json!({ "rows": 2, "columns": 2 })).expect("the table apply is queued");
    let queued: Vec<Work> = recorder.work.try_iter().collect();
    let (ticket, params) = queued.into_iter().find_map(|work| match work {
        Work::Call { ticket, method, params } if method == "changes.apply" => Some((ticket, params)),
        _ => None,
    }).expect("the table AddNode request is held");

    recorder.app.select_ids(vec!["b".into()]);
    assert!(recorder.app.selection_generation > original_generation, "a newer user selection advances the local generation");
    let selection_work: Vec<Work> = recorder.work.try_iter().collect();
    assert!(selection_work.iter().any(|work| matches!(work, Work::Call { method, .. } if method == "selection.set")));
    answer_recorded_work(&mut recorder, &mut host, selection_work);
    assert_eq!(host.call("context.read", json!({ "artifact_id": canvas, "view_id": recorder.app.view_id.clone() })).unwrap()["selection"]["node_ids"], json!(["b"]));

    let applied = host.call("changes.apply", params);
    assert!(applied.is_ok(), "the table insertion itself commits");
    assert_eq!(host.artifact(&canvas).revision, before_revision + 1);
    recorder.replies.send(RpcReply { ticket, result: applied.map(RpcPayload::Json) }).expect("the window is still listening");
    recorder.app.process_replies(&recorder.ctx);
    recorder.settle(&mut host);

    assert_eq!(recorder.app.selected_node.as_deref(), Some("b"), "the table insertion must not overwrite the newer selection");
    assert_eq!(recorder.app.selected_nodes, ["b"]);
    assert_eq!(recorder.app.context.as_ref().unwrap().selection.node_ids, ["b"]);
    let artifact = host.artifact(&canvas);
    assert!(match &artifact.body {
        ArtifactBody::Canvas(document) => document.pages[0].nodes.iter().any(|node| node.ext.table.is_some()),
        _ => false,
    }, "the user selection remains while the table is committed");
}

/// Drops a node so that its left edge lies `physical_px` physical pixels right of the page's middle guide, in a window of
/// display scale `pixels_per_point`. The node is `a` of a two-column canvas, or with `grouped` the node `b`, a child of a group
/// of `b` and `c` (its bounds are relative to the group). `snap` leaves the view's snapping on, or turns it off first. Returns
/// the committed left edge on the page, the edge the snap gives (the drop itself when the view does not snap), the guide, and
/// the left edge the drop asked for.
fn drop_beside_a_guide(host: &mut Host, pixels_per_point: f32, physical_px: f64, snap: bool, grouped: bool) -> (i64, i64, i64, i64) {
    // The group starts at x 2 000 000, not at the width of its child, so an uncorrected frame cannot land on the guide by chance.
    let canvas = canvas_with_three_boxes(host, [0, 2_000_000, 3_000_000]);
    let mut recorder = Recorder::open(host, &canvas);
    let node_id = if grouped { "b" } else { "a" };
    if grouped {
        recorder.step(host, "select.many", json!({ "node_ids": ["b", "c"] })).expect("the two boxes are selected");
        recorder.step(host, "edit.group", json!({})).expect("the boxes group");
    }
    recorder
        .step(host, "canvas.guides.set", json!({ "axis": "columns", "count": 2, "spacing_emu": 0, "leading_margin_emu": 0, "trailing_margin_emu": 0 }))
        .expect("the columns are set");
    if !snap {
        recorder.step(host, "view.toggle_snap", json!({})).expect("the snapping turns off");
    }
    let source_rect = render_drag_pages(&mut recorder, host).into_iter().find(|(index, _)| *index == 0).expect("the page is drawn").1;
    // The page position of the node: its bounds plus the origin of its parent, the group, when it has one.
    let page_x = |artifact: &designllm_contracts::Artifact| -> i64 {
        let ArtifactBody::Canvas(document) = &artifact.body else { panic!("Canvas") };
        let parent = document.pages[0]
            .nodes
            .iter()
            .find(|group| group.children.iter().any(|child| child.id == node_id))
            .map_or(0, |group| group.bounds.x);
        parent + super::doc_tree::find_node(artifact, node_id).expect("the node is on the page").bounds.x
    };
    let (page_size, layout) = {
        let artifact = host.artifact(&canvas);
        let ArtifactBody::Canvas(document) = &artifact.body else { panic!("Canvas") };
        let page = &document.pages[0];
        (page.size, super::page_guides::page_guide_layout(document, page).expect("the guides fit the page"))
    };
    let guide = layout.vertical_lines()[1];
    begin_test_drag_at_scale(&mut recorder, host, node_id, 0, source_rect, (0.5, 0.5), pixels_per_point);
    let zoom = recorder.app.view_ref().zoom;
    let distance = (physical_px * 9_525.0 / (f64::from(zoom) * f64::from(pixels_per_point))).round() as i64;
    let wanted = guide + distance;
    let before = page_x(&host.artifact(&canvas));
    let delta = eframe::egui::Vec2::new(((wanted - before) as f64 / page_size.width as f64 * f64::from(source_rect.width())) as f32, 0.0);
    finish_test_drag(&mut recorder, host, delta);
    let artifact = host.artifact(&canvas);
    let committed = page_x(&artifact);
    let node = super::doc_tree::find_node(&artifact, node_id).expect("the node remains").bounds;
    let frame = designllm_contracts::layout_guides::EmuRect { x_emu: wanted, y_emu: node.y, width_emu: node.width, height_emu: node.height };
    let expected = if snap {
        super::page_guides::move_snap(Some(&layout), frame, zoom, pixels_per_point).expect("a valid snap").rect.x_emu
    } else {
        wanted
    };
    (committed, expected, guide, wanted)
}

/// A free move that ends within 8 physical pixels of a page guide snaps to it, and one that ends farther does not. The window
/// is a physical distance, so at display scales 1.0, 1.5 and 2.0 the same 6 px snaps and the same 12 px does not.
#[test]
fn a_free_move_snaps_by_the_same_physical_distance_at_every_display_scale() {
    for pixels_per_point in [1.0_f32, 1.5, 2.0] {
        for (physical_px, snaps) in [(6.0_f64, true), (12.0, false)] {
            let mut host = Host::start();
            let (committed, expected, guide, _) = drop_beside_a_guide(&mut host, pixels_per_point, physical_px, true, false);
            assert_eq!(committed, expected, "the commit is the snap result at scale {pixels_per_point}");
            assert_eq!(committed == guide, snaps, "{physical_px} physical px from the guide at scale {pixels_per_point}");
        }
    }
}

/// With the view's snapping off (`view.toggle_snap`), a drop 6 physical pixels from a guide stays where it was dropped.
#[test]
fn a_drop_stays_where_it_is_dropped_when_the_view_does_not_snap() {
    let mut host = Host::start();
    let (committed, _, _, wanted) = drop_beside_a_guide(&mut host, 1.0, 6.0, false, false);
    assert_eq!(committed, wanted, "6 physical px is inside the window, but the view does not snap");
}

/// A drop on another page snaps to the guides of that page: the commit uses the target page's layout.
#[test]
fn a_drop_on_another_page_snaps_to_the_guides_of_that_page() {
    let pixels_per_point = 1.5_f32;
    let mut host = Host::start();
    let (canvas, _) = canvas_with_second_page_for_drag(&mut host);
    let mut recorder = Recorder::open(&mut host, &canvas);
    recorder
        .step(&mut host, "canvas.guides.set", json!({ "page_index": 1, "axis": "columns", "count": 2, "spacing_emu": 0, "leading_margin_emu": 0, "trailing_margin_emu": 0 }))
        .expect("the target page's columns are set");
    let page_rects = render_drag_pages(&mut recorder, &mut host);
    let rect_of = |index: u32| page_rects.iter().find(|(page, _)| *page == index).expect("the page is drawn").1;
    let (source_rect, target_rect) = (rect_of(0), rect_of(1));
    let source_frame = recorder.app.page_textures[&0].frames["a"];
    let (source_width, target_size, guide) = {
        let artifact = host.artifact(&canvas);
        let ArtifactBody::Canvas(document) = &artifact.body else { panic!("Canvas") };
        let layout = super::page_guides::page_guide_layout(document, &document.pages[1]).expect("the guides fit the page");
        (document.pages[0].size.width, document.pages[1].size, layout.vertical_lines()[1])
    };
    let start = begin_test_drag_at_scale(&mut recorder, &mut host, "a", 0, source_rect, (0.2, 0.8), pixels_per_point);
    // The left edge should land 6 physical pixels right of the guide. The grab point is kept as the commit keeps it.
    let zoom = f64::from(recorder.app.view_ref().zoom);
    let distance = 6.0 * 9_525.0 / (zoom * f64::from(pixels_per_point));
    let source_x = f64::from(start.x - source_rect.left()) / f64::from(source_rect.width()) * source_width as f64;
    let wanted_x = guide as f64 + distance + (source_x - source_frame.x as f64);
    let drop = eframe::egui::Pos2::new(
        target_rect.left() + (wanted_x / target_size.width as f64 * f64::from(target_rect.width())) as f32,
        target_rect.top() + target_rect.height() * 0.5,
    );
    finish_test_drag(&mut recorder, &mut host, drop - start);
    let (page_index, _, bounds) = canvas_page_of(&mut host, &canvas, "a").expect("the node remains");
    assert_eq!(page_index, 1, "the node moved to the other page");
    assert_eq!(bounds.x, guide, "the drop snaps to the guide of the target page");
}

/// A child of a group snaps by its page position: the guide is on the page, not in the group's coordinates. A node outside a
/// group snaps as before.
#[test]
fn a_group_child_snaps_by_its_page_position() {
    let mut host = Host::start();
    let (committed, expected, guide, _) = drop_beside_a_guide(&mut host, 1.0, 6.0, true, true);
    assert_eq!(committed, expected, "the commit is the snap result on the page");
    assert_eq!(committed, guide, "the group child lands on the page guide");
    let mut host = Host::start();
    let (committed, _, guide, _) = drop_beside_a_guide(&mut host, 1.0, 6.0, true, false);
    assert_eq!(committed, guide, "a node outside a group snaps as before");
}

/// The GUI's Ctrl+S sends `file.save` with no arguments. A document with no save location used to succeed without writing
/// anything; the window must now say why nothing was written.
#[test]
fn file_save_without_a_save_location_says_why_nothing_was_written() {
    let mut host = Host::start();
    let canvas = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
    let mut recorder = Recorder::open(&mut host, &canvas);
    assert!(recorder.app.save_path.is_empty(), "a document never saved has no location");
    let mark = recorder.calls.len();
    // The test has no native dialog, so the location is not chosen here.
    recorder.app.invoke_ui("file.save", json!({}));
    recorder.settle(&mut host);
    assert!(recorder.calls[mark..].iter().all(|call| call != "artifact.save"), "nothing is written without a location");
    let notice = recorder.app.notices.current(std::time::Instant::now()).expect("the window says why nothing was saved");
    assert!(notice.text.contains("저장 위치"), "{}", notice.text);
}

/// `view.zoom_selection` zooms to the selected box and centres it on the board. It used to be the same as "fit" and left
/// the view where it was.
#[test]
fn zoom_to_selection_centres_the_selected_node_on_the_board() {
    let mut host = Host::start();
    let canvas = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
    let mut recorder = Recorder::open(&mut host, &canvas);
    recorder.step(&mut host, "select.node", json!({ "node_id": "c" })).expect("box c is selected");
    let board = recorder.app.board_rect.expect("the board is drawn");
    let zoom_before = recorder.app.view_ref().zoom;
    recorder.step(&mut host, "view.zoom_selection", json!({})).expect("the selection zooms");
    let view = recorder.app.view_ref();
    assert!(view.zoom > zoom_before, "a small box zooms in: {zoom_before} -> {}", view.zoom);
    let frame = recorder
        .app
        .node_frames(board.min + view.pan, view.zoom)
        .into_iter()
        .find_map(|(id, rect)| (id == "c").then_some(rect))
        .expect("box c has a frame");
    assert!(board.contains(frame.min) && board.contains(frame.max), "the box is inside the board: {frame:?} in {board:?}");
    assert!((frame.center() - board.center()).length() < 1.0, "the box is centred: {:?} against {:?}", frame.center(), board.center());
}

/// Asserts that the nodes `ids` fill the board's centre: their union is inside the board and centred on it.
fn assert_selection_centred(recorder: &Recorder, ids: &[&str]) {
    let board = recorder.app.board_rect.expect("the board is drawn");
    let view = recorder.app.view_ref();
    let frames = recorder.app.node_frames(board.min + view.pan, view.zoom);
    let union = frames
        .iter()
        .filter(|(id, _)| ids.contains(&id.as_str()))
        .map(|(_, frame)| *frame)
        .reduce(|a, b| a.union(b))
        .expect("the selected nodes have frames");
    assert!(board.contains(union.min) && board.contains(union.max), "the selection is inside the board: {union:?} in {board:?}");
    assert!((union.center() - board.center()).length() < 1.0, "the selection is centred: {:?} against {:?}", union.center(), board.center());
}

/// `view.zoom_selection` fits nodes chosen on two pages, with the pages placed for that zoom, and fits a Slides selection the same way.
#[test]
fn zoom_to_selection_centres_nodes_chosen_on_two_pages_and_on_slides() {
    use designllm_contracts::{Intent, Node, Operation};

    let mut host = Host::start();
    let (canvas, second_page) = canvas_with_second_page_for_drag(&mut host);
    let node: Node = serde_json::from_value(json!({
        "id": "d", "kind": "rectangle", "style": { "fill": "#2563EB" },
        "bounds": { "x": 100 * 9525, "y": 100 * 9525, "width": 200 * 9525, "height": 120 * 9525 }
    }))
    .expect("a node for the second page");
    host.apply(&canvas, vec![Operation::AddNode { page_id: second_page, parent_id: None, node }], [Intent::Content].into_iter().collect())
        .expect("the node goes on the second page");
    let mut recorder = Recorder::open(&mut host, &canvas);
    recorder.step(&mut host, "select.many", json!({ "node_ids": ["a", "d"] })).expect("a node on each page is selected");
    recorder.step(&mut host, "view.zoom_selection", json!({})).expect("the selection zooms");
    assert_selection_centred(&recorder, &["a", "d"]);

    let (slides, _) = slides_with_second_page_for_drag(&mut host);
    let mut recorder = Recorder::open(&mut host, &slides);
    recorder.step(&mut host, "select.node", json!({ "node_id": "slide-node" })).expect("the slide shape is selected");
    recorder.step(&mut host, "view.zoom_selection", json!({})).expect("the Slides selection zooms");
    assert_selection_centred(&recorder, &["slide-node"]);
}

/// `view.zoom_selection` keeps the zoom and place its six settle rounds give (`settle_zoom`, 3 rounds before `43`): one box, nodes
/// on two pages whose gaps change with the zoom, and a Slides shape.
#[test]
fn zoom_to_selection_keeps_the_zoom_and_place_of_its_settle_rounds() {
    use designllm_contracts::{Intent, Node, Operation};

    // Captured with the six rounds (unchanged code): zoom and pan of each fit, in shortest f32 form.
    const EXPECTED: &str =
        "one box: zoom 5.6388 pan -1728.0 -332.40002\ntwo pages: zoom 0.35322195 pan 48.0 321.4845\nslides: zoom 8.0 pan 50.036743 168.02625";
    let mut host = Host::start();
    let boxes = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
    let (two_pages, second_page) = canvas_with_second_page_for_drag(&mut host);
    let node: Node = serde_json::from_value(json!({
        "id": "d", "kind": "rectangle", "style": { "fill": "#2563EB" },
        "bounds": { "x": 100 * 9525, "y": 100 * 9525, "width": 200 * 9525, "height": 120 * 9525 }
    }))
    .expect("a node for the second page");
    host.apply(&two_pages, vec![Operation::AddNode { page_id: second_page, parent_id: None, node }], [Intent::Content].into_iter().collect())
        .expect("the node goes on the second page");
    let (slides, _) = slides_with_second_page_for_drag(&mut host);
    let mut lines = Vec::new();
    for (name, artifact, selection) in [
        ("one box", &boxes, json!(["c"])),
        ("two pages", &two_pages, json!(["a", "d"])),
        ("slides", &slides, json!(["slide-node"])),
    ] {
        let mut recorder = Recorder::open(&mut host, artifact);
        recorder.step(&mut host, "select.many", json!({ "node_ids": selection })).expect("the nodes are selected");
        recorder.step(&mut host, "view.zoom_selection", json!({})).expect("the selection zooms");
        let view = recorder.app.view_ref();
        lines.push(format!("{name}: zoom {:?} pan {:?} {:?}", view.zoom, view.pan.x, view.pan.y));
    }
    assert_eq!(lines.join("\n"), EXPECTED);
}

/// `fit_view` puts the zoom and the place of the pages where they were before the shared settle (`settle_zoom`): a canvas and a
/// two-page Slides document, each on a wide and a small board, where the gaps between the pages change with the zoom.
#[test]
fn fit_view_keeps_its_zoom_and_place_on_fixed_boards() {
    use eframe::egui::{Pos2, Rect, Vec2};

    // Captured before the shared settle existed (unchanged code): zoom and pan of each fit, in shortest f32 form.
    const EXPECTED: &str = "canvas wide: zoom 0.9027778 pan 62.22223 76.0\ncanvas small: zoom 0.20833333 pan 76.66667 76.0\nslides small: zoom 0.13515624 pan 28.0 76.0";
    let mut host = Host::start();
    let canvas = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
    let (slides, _) = slides_with_second_page_for_drag(&mut host);
    let wide = Rect::from_min_size(Pos2::new(0.0, 0.0), Vec2::new(1280.0, 800.0));
    let small = Rect::from_min_size(Pos2::new(10.0, 20.0), Vec2::new(420.0, 300.0));
    let mut lines = Vec::new();
    for (name, artifact, board) in [("canvas wide", &canvas, wide), ("canvas small", &canvas, small), ("slides small", &slides, small)] {
        let mut recorder = Recorder::open(&mut host, artifact);
        recorder.app.fit_view(board);
        let view = recorder.app.view_ref();
        lines.push(format!("{name}: zoom {:?} pan {:?} {:?}", view.zoom, view.pan.x, view.pan.y));
    }
    assert_eq!(lines.join("\n"), EXPECTED);
}
