//! A figure inside a document, from the window's controls to the real host: the window sends its
//! requests to a real workspace (the figure, canvas, slide and Word engines, the real store and
//! renderer) and adopts the answers, exactly as it does against the product's host. A figure that
//! changes afterwards must leave the documents that show it as they were until the user asks for
//! the update.

use std::{
    sync::mpsc::{Receiver, Sender},
    time::Instant,
};

use base64::{engine::general_purpose::STANDARD, Engine as _};
use designllm_contracts::{
    Artifact, ArtifactBody, ArtifactKind, ArtifactSummary, AssetRef, Feature, FlowBlock, Intent,
    Node, NodeKind, Operation, Paragraph, ParagraphFormat, TextRun, new_id,
};
use eframe::egui::Context;
use serde_json::json;

use super::{
    figure_flow_tests::Host,
    figure_tools::{new_diagram_panel, node_addition, panel_additions},
    links,
    test_support::settle_work,
    tests::{drawn_text, screen},
    DesktopApp, PendingAction,
};
use crate::{
    rpc::{RpcPayload, RpcReply, RpcWorker, Work},
    theme,
};

/// The window with a real host behind it. `settle` plays the host's side until the window has
/// nothing left to ask.
struct Window {
    app: DesktopApp,
    replies: Sender<RpcReply>,
    work: Receiver<Work>,
    ctx: Context,
    /// Every method the window sent to the host, oldest first.
    log: Vec<String>,
    /// When the last `settle` began: `message` reads the notices as of then, so a notice the answers raised is read
    /// however long the test took since (an information notice lasts 4 s).
    settled_at: Instant,
}

impl Window {
    fn open(host: &mut Host) -> Self {
        let (rpc, replies, work) = RpcWorker::detached();
        let ctx = Context::default();
        theme::install(&ctx);
        let app = DesktopApp::new(rpc, "작업공간".into(), None, None);
        let mut window = Self { app, replies, work, ctx, log: Vec::new(), settled_at: Instant::now() };
        window.settle(host);
        window
    }

    fn settle(&mut self, host: &mut Host) {
        self.settled_at = Instant::now();
        settle_work(&self.work, &mut self.app, &self.ctx, |item| {
            if let Work::Call { ticket, method, params, .. } = item {
                self.log.push(method.clone());
                let result = host.call(&method, params).map(RpcPayload::Json);
                self.replies.send(RpcReply { ticket, result }).expect("the window listens");
            }
            // A preview image is not what these tests look at.
        });
    }

    /// The work list as the host has it now (what an `artifact.changed` event makes the window read).
    fn sync_list(&mut self, host: &mut Host) {
        self.app.call("workspace.list", json!({}), PendingAction::WorkspaceList);
        self.settle(host);
    }

    fn show(&mut self, host: &mut Host, id: &str) {
        let list = host.call("workspace.list", json!({})).expect("list");
        let summary = list["artifacts"]
            .as_array()
            .and_then(|all| all.iter().find(|item| item["id"] == id))
            .cloned()
            .expect("the artifact is in the work list");
        self.app.activate(serde_json::from_value::<ArtifactSummary>(summary).expect("summary"));
        self.settle(host);
        self.sync_list(host);
    }

    /// Draws a few frames of the whole window and returns the text of the last one. Drawing must
    /// never send anything by itself.
    fn frames(&mut self, host: &mut Host) -> Vec<String> {
        let mut shown = Vec::new();
        for _ in 0..3 {
            let output = self.ctx.run(screen(1.0, 1280.0, 800.0), |ctx| {
                self.app.draw_frame(ctx);
            });
            shown = drawn_text(&output);
        }
        self.settle(host);
        shown
    }

    fn message(&self) -> Option<String> {
        self.app.notices.current(self.settled_at).map(|notice| notice.text.clone())
    }

    fn artifact(&self) -> &Artifact {
        self.app.artifact.as_ref().expect("the document is loaded")
    }

    fn sent_since(&self, mark: usize) -> &[String] {
        &self.log[mark..]
    }
}

/// A figure with a diagram of `nodes` boxes, so that every further box changes how it looks.
pub(super) fn drawn_figure(host: &mut Host, title: &str, nodes: usize) -> String {
    let id = host.create("figure", title);
    let (figure, _) = host.figure(&id);
    host.apply(
        &id,
        panel_additions(&figure, new_diagram_panel(&figure)),
        [Intent::Content, Intent::Design].into_iter().collect(),
    )
    .expect("an empty diagram panel");
    for _ in 0..nodes {
        add_box(host, &id);
    }
    id
}

pub(super) fn add_box(host: &mut Host, figure_id: &str) {
    let (figure, _) = host.figure(figure_id);
    let panel_id = figure.panels[0].id.clone();
    let operation = node_addition(&figure, &panel_id, NodeKind::Rectangle).expect("room for another box");
    host.apply(figure_id, vec![operation], [Intent::Content].into_iter().collect())
        .expect("a box inside its panel");
}

fn render_of(host: &mut Host, figure_id: &str, revision: u64) -> (String, AssetRef) {
    let render = host
        .call(
            "figure.render_link",
            json!({ "figure_id": figure_id, "figure_revision": revision, "representation": "png", "profile": { "dpi": 300, "transparent": false } }),
        )
        .expect("the figure renders");
    (
        render["render_hash"].as_str().expect("hash").to_owned(),
        serde_json::from_value(render["asset"].clone()).expect("asset"),
    )
}

pub(super) fn bytes_of(host: &mut Host, asset: &AssetRef) -> Vec<u8> {
    let chunk = host
        .call("asset.read", json!({ "sha256": asset.sha256, "offset": 0, "max_bytes": asset.bytes }))
        .expect("the asset is stored");
    assert_eq!(chunk["eof"], true, "the whole picture comes in one read");
    STANDARD.decode(chunk["base64"].as_str().expect("payload")).expect("base64")
}

/// Width and height in pixels from the PNG header.
fn pixels(png: &[u8]) -> (i128, i128) {
    assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n", "a PNG");
    let read = |at: usize| i128::from(u32::from_be_bytes([png[at], png[at + 1], png[at + 2], png[at + 3]]));
    (read(16), read(20))
}

/// `width * pixel_height / pixel_width`, rounded half up: the height a linked picture takes.
fn proportional(width: i64, (pixel_width, pixel_height): (i128, i128)) -> i64 {
    ((2 * i128::from(width) * pixel_height + pixel_width) / (2 * pixel_width)) as i64
}

fn pictures(artifact: &Artifact) -> Vec<&Node> {
    match &artifact.body {
        ArtifactBody::Canvas(document) => document
            .pages
            .iter()
            .flat_map(|page| &page.nodes)
            .filter(|node| node.kind == NodeKind::Image)
            .collect(),
        ArtifactBody::Slides(document) => document
            .pages
            .iter()
            .flat_map(|page| &page.nodes)
            .filter(|node| node.kind == NodeKind::Image)
            .collect(),
        ArtifactBody::Flow(document) => document
            .sections
            .iter()
            .flat_map(|section| &section.blocks)
            .filter_map(|block| match block {
                FlowBlock::Image(node) => Some(node),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn kind_of(name: &str) -> ArtifactKind {
    match name {
        "slides" => ArtifactKind::Slides,
        "docx" => ArtifactKind::Docx,
        "hwpx" => ArtifactKind::Hwpx,
        "hwp" => ArtifactKind::Hwp,
        _ => ArtifactKind::Canvas,
    }
}

/// One unit of the Hangul word processors, in EMU: a picture there is a whole number of them.
const HWP_UNIT: i64 = designllm_contracts::EMU_PER_HWPUNIT;

/// The whole life of a figure picture in one kind of document. `unit` is how far a stored size may
/// differ from the exact one: 0 where the format keeps EMU, one HWP unit where it rounds to those.
fn figure_picture_follows_the_figure_only_when_asked(kind: &str, unit: i64) {
    let mut host = Host::start();
    let document = host.create(kind, "문서");
    let figure = drawn_figure(&mut host, "그림 1", 2);
    let mut window = Window::open(&mut host);
    window.show(&mut host, &document);
    assert_eq!(window.artifact().kind, kind_of(kind));
    assert!(window.app.feature_edit_supported(Feature::FigureLinks), "{kind} takes figure pictures");
    assert!(window.artifact().figure_links.is_empty());
    assert!(pictures(window.artifact()).is_empty());
    let document_revision = window.artifact().revision;

    // The picture goes in with its link, in one change set.
    let before = window.log.len();
    window.app.begin_figure_insert(&figure);
    window.settle(&mut host);
    let expected_notice = match kind {
        "hwp" => Some("일부 값이 저장 형식의 정밀도에 맞게 조정되었습니다."),
        "hwpx" => Some("일부 값이 저장 형식의 정밀도에 맞게 조정되었습니다. 문서 미리보기 텍스트를 편집 내용에 맞게 갱신했습니다."),
        _ => None,
    };
    assert_eq!(window.message().as_deref(), expected_notice, "{kind}: accepted insertion diagnostics are shown safely");
    assert_eq!(
        window.sent_since(before)[..4],
        ["artifact.inspect", "figure.render_link", "changes.apply", "workspace.list"],
        "the figure is read, rendered, and then one change set follows"
    );
    assert_eq!(
        window.sent_since(before).iter().filter(|method| *method == "changes.apply").count(),
        1,
        "picture and link are one change set"
    );
    let first_revision = host.artifact(&figure).revision;
    let (first_hash, first_asset) = render_of(&mut host, &figure, first_revision);
    let inserted = window.artifact().clone();
    assert_eq!(inserted.revision, document_revision + 1, "one revision for picture and link");
    let [picture] = pictures(&inserted)[..] else { panic!("one picture in {kind}") };
    let picture = picture.clone();
    let [link] = &inserted.figure_links[..] else { panic!("one link in {kind}") };
    assert_eq!(
        (link.figure_id.as_str(), link.figure_revision, link.target_node_id.as_str(), link.representation.as_str(), link.render_hash.as_str()),
        (figure.as_str(), first_revision, picture.id.as_str(), "png", first_hash.as_str())
    );
    assert_eq!(picture.asset.as_ref().map(|asset| asset.sha256.as_str()), Some(first_hash.as_str()), "the picture is the render");
    assert_eq!(window.app.selected_node.as_deref(), Some(picture.id.as_str()), "the new picture is selected");
    assert_eq!(window.app.linked_picture_title(&picture.id).as_deref(), Some("그림 1"));
    let shown = window.frames(&mut host);
    // Text documents show the labelled `논문 그림` button; canvases and slides list figures in the insert menu (+ ▾).
    if matches!(kind, "canvas" | "slides") {
        assert!(window.app.figure_insertion_blocked().is_none(), "{kind}: the insert menu offers figures");
    } else {
        assert!(shown.iter().any(|text| text == "논문 그림"), "{kind}: the figure control is on screen: {shown:?}");
    }
    assert!(shown.iter().all(|text| !text.starts_with("갱신")), "{kind}: the picture shows the current revision: {shown:?}");
    let first_pixels = pixels(&bytes_of(&mut host, &first_asset));
    assert!(
        (picture.bounds.height - proportional(picture.bounds.width, first_pixels)).abs() <= unit,
        "{kind}: the proportions of the render: {:?} for {first_pixels:?} pixels",
        picture.bounds
    );
    let figure_width = match &host.artifact(&figure).body {
        ArtifactBody::Figure(document) => document.size.width,
        _ => panic!("a figure"),
    };
    assert!(picture.bounds.width <= figure_width + unit, "never larger than the figure's own size");
    assert!(picture.bounds.width > 0 && picture.bounds.height > 0);

    // The figure changes. The document does not, and nothing is sent on its behalf.
    add_box(&mut host, &figure);
    let second_revision = host.artifact(&figure).revision;
    assert!(second_revision > first_revision);
    let unchanged = host.artifact(&document);
    assert_eq!(unchanged.revision, inserted.revision, "{kind}: the document did not move");
    assert_eq!(unchanged.figure_links[0].figure_revision, first_revision);
    window.sync_list(&mut host);
    let before = window.log.len();
    let shown = window.frames(&mut host);
    assert_eq!(window.sent_since(before), Vec::<String>::new().as_slice(), "drawing sends nothing");
    assert!(
        shown.iter().any(|text| *text == format!("갱신 r{first_revision} → r{second_revision}")),
        "{kind}: the update is on offer, naming both revisions: {shown:?}"
    );
    assert_eq!(window.artifact().figure_links[0].figure_revision, first_revision);
    let summary = window.app.artifacts.iter().find(|summary| summary.id == figure).expect("listed").clone();
    assert_eq!(
        links::newer_revision(&window.artifact().figure_links[0], &summary),
        Some(second_revision),
        "the picture is offered the newer revision"
    );
    assert!(window.app.selected_node.is_some());

    // The user asks: the same picture takes the render of the new revision.
    let before = window.log.len();
    window.app.begin_figure_update(&picture.id);
    window.settle(&mut host);
    assert_eq!(window.message().as_deref(), expected_notice, "{kind}: accepted update diagnostics are shown safely");
    assert_eq!(
        window.sent_since(before)[..3],
        ["artifact.inspect", "figure.render_link", "changes.apply"],
    );
    let (second_hash, second_asset) = render_of(&mut host, &figure, second_revision);
    assert_ne!(first_hash, second_hash, "another box changes the picture");
    let updated = window.artifact().clone();
    let [after] = pictures(&updated)[..] else { panic!("still one picture in {kind}") };
    assert_eq!(after.id, picture.id, "the same picture");
    assert_eq!(after.asset.as_ref().map(|asset| asset.sha256.as_str()), Some(second_hash.as_str()));
    assert_eq!(after.bounds.width, picture.bounds.width, "the width is kept");
    assert_eq!((after.bounds.x, after.bounds.y), (picture.bounds.x, picture.bounds.y), "and the place");
    let second_pixels = pixels(&bytes_of(&mut host, &second_asset));
    assert!(
        (after.bounds.height - proportional(after.bounds.width, second_pixels)).abs() <= unit,
        "{kind}: the proportions of the new render: {:?} for {second_pixels:?} pixels",
        after.bounds
    );
    let [link] = &updated.figure_links[..] else { panic!("still one link in {kind}") };
    assert_eq!((link.figure_revision, link.render_hash.as_str()), (second_revision, second_hash.as_str()));
    let summary = window.app.artifacts.iter().find(|summary| summary.id == figure).expect("listed").clone();
    assert_eq!(links::newer_revision(link, &summary), None, "nothing newer is left to offer");
    let shown = window.frames(&mut host);
    assert!(shown.iter().all(|text| !text.starts_with("갱신")), "{kind}: the offer is gone after the update: {shown:?}");
    assert!(shown.iter().filter(|text| *text == "그림 1").count() >= 1, "{kind}: the linked picture still shows its figure's title: {shown:?}");

    // Undo takes the update back; undoing the insertion removes picture and link together.
    window.app.history(false).expect("the first undo is enqueued");
    window.settle(&mut host);
    let back = window.artifact().clone();
    let [again] = pictures(&back)[..] else { panic!("the first picture is back in {kind}") };
    assert_eq!(again.asset.as_ref().map(|asset| asset.sha256.as_str()), Some(first_hash.as_str()));
    assert_eq!(back.figure_links[0].figure_revision, first_revision);
    window.app.history(false).expect("the insertion undo is enqueued");
    window.settle(&mut host);
    assert!(pictures(window.artifact()).is_empty(), "{kind}: the picture went with the insertion");
    assert!(window.artifact().figure_links.is_empty(), "{kind}: and its link with it");
    window.app.history(true).expect("redo is enqueued");
    window.settle(&mut host);
    assert_eq!(pictures(window.artifact()).len(), 1);
    assert_eq!(window.artifact().figure_links.len(), 1);
    assert_eq!(window.message(), None);
}

#[test]
fn a_slide_takes_a_figure_picture_and_follows_the_figure_only_when_asked() {
    figure_picture_follows_the_figure_only_when_asked("slides", 0);
}

#[test]
fn a_word_document_takes_a_figure_picture_and_follows_the_figure_only_when_asked() {
    figure_picture_follows_the_figure_only_when_asked("docx", 0);
}

#[test]
fn a_canvas_takes_a_figure_picture_and_follows_the_figure_only_when_asked() {
    figure_picture_follows_the_figure_only_when_asked("canvas", 0);
}

#[test]
fn a_hwpx_document_takes_a_figure_picture_and_follows_the_figure_only_when_asked() {
    figure_picture_follows_the_figure_only_when_asked("hwpx", HWP_UNIT);
}

#[test]
fn an_hwp_document_takes_a_figure_picture_and_follows_the_figure_only_when_asked() {
    figure_picture_follows_the_figure_only_when_asked("hwp", HWP_UNIT);
}

pub(super) fn paragraph(text: &str) -> FlowBlock {
    FlowBlock::Paragraph(Paragraph {
        id: new_id(),
        runs: vec![TextRun::new(new_id(), text)],
        style_ref: None,
        format: ParagraphFormat::default(),
    })
}

#[test]
fn a_picture_goes_into_the_word_document_after_the_selected_paragraph() {
    let mut host = Host::start();
    let document = host.create("docx", "문서");
    let figure = drawn_figure(&mut host, "그림 1", 1);
    let section = match &host.artifact(&document).body {
        ArtifactBody::Flow(flow) => flow.sections[0].id.clone(),
        _ => panic!("a flowing document"),
    };
    for text in ["첫째", "둘째", "셋째"] {
        let blocks = match &host.artifact(&document).body {
            ArtifactBody::Flow(flow) => flow.sections[0].blocks.len() as u32,
            _ => panic!("a flowing document"),
        };
        host.apply(
            &document,
            vec![Operation::InsertFlowBlock { section_id: section.clone(), index: blocks, block: paragraph(text) }],
            [Intent::Content].into_iter().collect(),
        )
        .expect("a paragraph");
    }
    let mut window = Window::open(&mut host);
    window.show(&mut host, &document);

    let (second_id, second_text) = {
        let ArtifactBody::Flow(flow) = &window.artifact().body else { panic!("flow") };
        let blocks = &flow.sections[0].blocks;
        let position = blocks
            .iter()
            .position(|block| matches!(block, FlowBlock::Paragraph(p) if p.runs.iter().any(|run| run.text == "둘째")))
            .expect("the second paragraph");
        let FlowBlock::Paragraph(second) = &blocks[position] else { unreachable!() };
        (second.id.clone(), "둘째".to_owned())
    };
    window.app.select_paragraph(second_id.clone(), &second_text);
    window.settle(&mut host);
    window.app.begin_figure_insert(&figure);
    window.settle(&mut host);
    assert_eq!(window.message(), None);

    let ArtifactBody::Flow(flow) = &window.artifact().body else { panic!("flow") };
    let blocks = &flow.sections[0].blocks;
    let at = blocks
        .iter()
        .position(|block| matches!(block, FlowBlock::Paragraph(p) if p.id == second_id))
        .expect("the second paragraph is still there");
    assert!(
        matches!(&blocks[at + 1], FlowBlock::Image(_)),
        "the picture follows the paragraph that was selected"
    );
    assert!(
        matches!(&blocks[at + 2], FlowBlock::Paragraph(p) if p.runs.iter().any(|run| run.text == "셋째")),
        "and the next paragraph comes after it"
    );
    // Nothing is selected afterwards but the picture, whose figure the inspector then shows.
    let [picture] = pictures(window.artifact())[..] else { panic!("one picture") };
    assert_eq!(window.app.selected_node.as_deref(), Some(picture.id.as_str()));

    // Page width: no wider than the text area (an A4 page with 20 mm margins) or the figure.
    let width = picture.bounds.width;
    assert!(width <= 170 * designllm_contracts::EMU_PER_MM, "{width}");
}

#[test]
fn the_figure_a_picture_shows_opens_from_the_picture() {
    let mut host = Host::start();
    let document = host.create("slides", "문서");
    let figure = drawn_figure(&mut host, "그림 1", 1);
    let mut window = Window::open(&mut host);
    window.show(&mut host, &document);
    window.app.begin_figure_insert(&figure);
    window.settle(&mut host);
    let [picture] = pictures(window.artifact())[..] else { panic!("one picture") };
    let picture = picture.id.clone();
    let linked = window
        .app
        .artifacts
        .iter()
        .find(|summary| Some(&summary.id) == window.artifact().figure_links.first().map(|link| &link.figure_id))
        .cloned()
        .expect("the figure the link names is in the work list");
    assert_eq!(linked.id, figure);
    window.frames(&mut host);

    // What the title button does.
    window.app.activate(linked);
    window.settle(&mut host);
    assert_eq!(window.artifact().id, figure);
    assert_eq!(window.artifact().kind, ArtifactKind::Figure);
    assert!(links::link_of(window.artifact(), &picture).is_none(), "a figure holds no links");
    assert_eq!(window.message(), None);
}

/// A canvas with three text boxes `a`, `b` and `c` on its first page, at the given x positions (EMU), 1,000,000 wide.
pub(super) fn canvas_with_three_boxes(host: &mut Host, xs: [i64; 3]) -> String {
    let canvas = host.create("canvas", "다중 선택");
    let inspected = host.call("artifact.inspect", json!({ "artifact_id": canvas })).expect("the canvas inspects");
    let artifact: designllm_contracts::Artifact = serde_json::from_value(inspected).expect("an artifact");
    let designllm_contracts::ArtifactBody::Canvas(document) = &artifact.body else {
        panic!("a canvas");
    };
    let page_id = document.pages[0].id.clone();
    let operations: Vec<designllm_contracts::Operation> = ["a", "b", "c"]
        .iter()
        .zip(xs)
        .map(|(name, x)| designllm_contracts::Operation::AddNode {
            page_id: page_id.clone(),
            parent_id: None,
            node: serde_json::from_value(json!({
                "id": name, "kind": "text", "text": "가",
                "bounds": { "x": x, "y": 1_000_000, "width": 1_000_000, "height": 400_000 },
            }))
            .expect("a text node parses"),
        })
        .collect();
    let change = designllm_contracts::ChangeSet {
        request_id: designllm_contracts::new_id(),
        actor: "desktop".into(),
        workspace_id: host.workspace_id.clone(),
        artifact_id: canvas.clone(),
        base_revision: artifact.revision,
        allowed_intents: [designllm_contracts::Intent::Content, designllm_contracts::Intent::Design].into_iter().collect(),
        job_ref: None,
        read_dependencies: Vec::new(),
        operations,
    };
    host.call("changes.apply", serde_json::to_value(change).expect("a change serialises")).expect("the boxes are added");
    canvas
}

/// The nodes of a canvas's first page as the host stores them now.
fn stored_nodes(host: &mut Host, canvas: &str) -> Vec<designllm_contracts::Node> {
    let inspected = host.call("artifact.inspect", json!({ "artifact_id": canvas })).expect("the canvas inspects");
    let artifact: designllm_contracts::Artifact = serde_json::from_value(inspected).expect("an artifact");
    let designllm_contracts::ArtifactBody::Canvas(document) = artifact.body else {
        panic!("a canvas");
    };
    document.pages[0].nodes.clone()
}

#[test]
fn select_many_selects_the_nodes_in_the_window_with_the_first_as_primary() {
    let mut host = Host::start();
    let canvas = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
    let mut window = Window::open(&mut host);
    window.show(&mut host, &canvas);
    window.app.invoke_ui("select.many", json!({ "node_ids": ["a", "c"] }));
    window.settle(&mut host);
    assert_eq!(window.app.selected_nodes, ["a", "c"]);
    assert_eq!(window.app.selected_node.as_deref(), Some("a"), "the first node is the primary one");
    assert_eq!(window.message(), None);
}

#[test]
fn distribute_spaces_the_selected_boxes_evenly_and_the_host_stores_the_new_frames() {
    let mut host = Host::start();
    let canvas = canvas_with_three_boxes(&mut host, [0, 300_000, 3_000_000]);
    let mut window = Window::open(&mut host);
    window.show(&mut host, &canvas);
    window.app.invoke_ui("select.many", json!({ "node_ids": ["a", "b", "c"] }));
    window.settle(&mut host);
    window.app.invoke_ui("props.distribute", json!({ "axis": "h" }));
    window.settle(&mut host);
    let nodes = stored_nodes(&mut host, &canvas);
    let x_of = |id: &str| nodes.iter().find(|node| node.id == id).expect("the box is stored").bounds.x;
    assert_eq!(x_of("b"), 1_500_000, "the middle box moves to the even gap between a and c");
    assert_eq!(x_of("c"), 3_000_000, "the outermost boxes stay where they are");
    assert_eq!(window.message(), None);
}

#[test]
fn align_right_and_left_move_the_selected_boxes_to_the_shared_edge_and_undo_restores_them() {
    let mut host = Host::start();
    let canvas = canvas_with_three_boxes(&mut host, [500_000, 0, 3_000_000]);
    let mut window = Window::open(&mut host);
    window.show(&mut host, &canvas);
    window.app.invoke_ui("select.many", json!({ "node_ids": ["a", "b", "c"] }));
    window.settle(&mut host);
    let xs = |host: &mut Host| {
        let nodes = stored_nodes(host, &canvas);
        ["a", "b", "c"].map(|id| nodes.iter().find(|node| node.id == id).expect("the box is stored").bounds.x)
    };

    window.app.invoke_ui("props.align", json!({ "edge": "right" }));
    window.settle(&mut host);
    assert_eq!(xs(&mut host), [3_000_000; 3], "the shared right edge is 4_000_000, so each 1_000_000-wide box starts at 3_000_000");
    assert_eq!(window.message(), None);

    window.app.invoke_ui("edit.undo", json!({}));
    window.settle(&mut host);
    assert_eq!(xs(&mut host), [500_000, 0, 3_000_000], "undo puts every box back where it was");

    window.app.invoke_ui("select.many", json!({ "node_ids": ["a", "b", "c"] }));
    window.settle(&mut host);
    window.app.invoke_ui("props.align", json!({ "edge": "left" }));
    window.settle(&mut host);
    assert_eq!(xs(&mut host), [0; 3], "the shared left edge is the smallest x");
    assert_eq!(window.message(), None);
}

#[test]
fn align_top_on_one_selected_box_moves_only_that_box() {
    let mut host = Host::start();
    let canvas = canvas_with_three_boxes(&mut host, [0, 300_000, 3_000_000]);
    let mut window = Window::open(&mut host);
    window.show(&mut host, &canvas);
    window.app.invoke_ui("select.node", json!({ "node_id": "b" }));
    window.settle(&mut host);
    window.app.invoke_ui("props.align", json!({ "edge": "top" }));
    window.settle(&mut host);
    let nodes = stored_nodes(&mut host, &canvas);
    let bounds_of = |id: &str| nodes.iter().find(|node| node.id == id).expect("the box is stored").bounds.clone();
    assert_eq!(bounds_of("b").y, 0, "the single box moves to the top of the page");
    assert_eq!(bounds_of("a").y, 1_000_000, "the other boxes keep their y");
    assert_eq!(bounds_of("c").y, 1_000_000);
    assert_eq!(bounds_of("b").x, 300_000, "only the y changes");
    assert_eq!(window.message(), None);
}
