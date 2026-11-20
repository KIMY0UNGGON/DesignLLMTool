//! Fail-closed guards for Canvas table-cell editing while its rendered frame is stale or missing.

use std::sync::mpsc::Receiver;

use designllm_contracts::{
    Artifact, ArtifactBody, ArtifactKind, ArtifactSummary, AssetRef, AssetStore, CanvasDocument, CanvasTable,
    CanvasTableRow, CanvasTableSpan, DltError, DltResult, EngineAdapter, EngineContext, ErrorCode, InstanceRef,
    LayoutSpec, Node, NodeExt, NodeKind, NodeOverride, Operation, Page, PageRole, Rect, RenderPage, RenderProfile, RenderScene,
    RenderSnapshot, Selection, Size, SourceRegion, Style, ViewTransform,
};
use eframe::egui::{Context, Event, RawInput};
use serde_json::{json, Value};

use super::{DesktopApp, InlineCellReadiness};
use crate::{
    grapheme::TextInputEdit,
    rpc::{RpcPayload, RpcReply, RpcWorker, Work},
};

type Request = (u64, String, Value);

struct Harness {
    app: DesktopApp,
    replies: std::sync::mpsc::Sender<RpcReply>,
    work: Receiver<Work>,
    ctx: Context,
}

impl Harness {
    fn new() -> Self {
        let (rpc, replies, work) = RpcWorker::detached();
        let mut app = DesktopApp::new(rpc, "table-cell integrity fixture".into(), None, None);
        while work.try_recv().is_ok() {}

        let cell = |id: &str, x: i64| Node {
            id: id.into(),
            kind: NodeKind::Rectangle,
            bounds: Rect { x, y: 60_000, width: 500_000, height: 180_000 },
            text: String::new(),
            style: Style::default(),
            asset: None,
            children: Vec::new(),
            layout: None,
            interactions: Vec::new(),
            variants: Default::default(),
            ext: NodeExt::default(),
        };
        let anchor = cell("anchor", 50_000);
        let cell = cell("cell", 550_000);
        let table = Node {
            id: "table".into(),
            kind: NodeKind::Group,
            bounds: Rect { x: 40_000, y: 50_000, width: 1_000_000, height: 360_000 },
            text: String::new(),
            style: Style::default(),
            asset: None,
            children: vec![anchor, cell],
            layout: None,
            interactions: Vec::new(),
            variants: Default::default(),
            ext: NodeExt {
                table: Some(CanvasTable {
                    rows: vec![CanvasTableRow { id: "row".into(), cell_ids: vec!["anchor".into(), "cell".into()] }],
                    header_row_id: None,
                    column_widths: None,
                    cell_spans: Default::default(),
                }),
                ..NodeExt::default()
            },
        };
        let page = Page { nodes: vec![table], ..Page::new("page-1", Size::slide()) };
        let mut artifact = Artifact::new(
            ArtifactKind::Canvas,
            "table cell",
            "canvas.v1",
            ArtifactBody::Canvas(CanvasDocument { pages: vec![page.clone()], ..Default::default() }),
        );
        artifact.id = "a1".into();
        artifact.revision = 1;
        app.active = Some(ArtifactSummary {
            id: artifact.id.clone(), kind: artifact.kind, title: artifact.title.clone(), revision: 1,
            engine_id: artifact.engine_id.clone(), dirty: false, modified_at_ms: None, active_jobs: Vec::new(),
        });
        app.artifact = Some(artifact);
        app.context = Some(serde_json::from_value(json!({
            "workspace_id": "w1", "artifact_id": "a1", "revision": 1, "view_id": app.view_id.clone(),
            "selection": { "node_ids": ["cell"], "text_range": { "paragraph_id": "cell", "start": 0, "end": 0 } },
            "visible_page": 0,
            "selected_properties": { "nodes": null, "stale_selection": null, "render_unavailable": null },
            "capabilities": { "text": { "read": "supported", "render": "supported", "edit": "supported", "preserve": "supported", "write": "supported", "reason": "" } },
            "render_snapshot": null, "view_transform": null
        })).expect("context"));
        app.reservations_for = Some("a1".into());
        app.selected_node = Some("cell".into());
        app.selected_nodes = vec!["cell".into()];
        app.local_selection = Some(Selection {
            node_ids: vec!["cell".into()],
            text_range: Some(designllm_contracts::TextRange { paragraph_id: "cell".into(), start: 0, end: 0 }),
        });
        app.max_texture_side = 4096;

        let snapshot = snapshot(&page, 1, true);
        let ctx = Context::default();
        app.store_page_texture(&ctx, 0, &snapshot, 1, 1, &[255, 255, 255, 255]);
        Self { app, replies, work, ctx }
    }

    fn sent(&mut self) -> Vec<Request> {
        let mut batch = Vec::new();
        while let Ok(work) = self.work.try_recv() {
            if let Work::Call { ticket, method, params } = work {
                batch.push((ticket, method, params));
            }
        }
        batch
    }

    fn answer(&mut self, request: &Request, result: Result<Value, DltError>) {
        self.replies.send(RpcReply { ticket: request.0, result: result.map(RpcPayload::Json) }).expect("app is listening");
        self.app.process_replies(&self.ctx);
    }

    fn message(&self) -> Option<String> {
        self.app.notices.current(std::time::Instant::now()).map(|notice| notice.text.clone())
    }

    fn begin_with_queued_text(&mut self) {
        assert!(self.app.begin_inline_text_edit(&self.ctx, "cell", 0, None));
        let _ = self.sent();
        let edit = TextInputEdit {
            request_id: designllm_contracts::new_id(),
            range: 0..0,
            paragraph_index: 0,
            paragraph_range: Some(0..0),
            caret_after: 1,
            paragraph_after: 0,
            caret_after_in_paragraph: 1,
            text: "x".into(),
            ime_commit: false,
            ime_generation: 0,
            after_text: None,
        };
        self.app.text_draft = "x".into();
        self.app.inline_text_edit.as_mut().expect("inline session").queue.push_back(edit);
    }

    fn start_ime_and_take_reserve(&mut self) -> Request {
        assert!(self.app.begin_inline_text_edit(&self.ctx, "cell", 0, None));
        let _ = self.sent();
        self.ctx.memory_mut(|memory| memory.request_focus(self.app.text_editor_id));
        let mut preedit = RawInput::default();
        preedit.events.extend([
            Event::Ime(eframe::egui::ImeEvent::Enabled),
            Event::Ime(eframe::egui::ImeEvent::Preedit("ㅎ".into())),
        ]);
        eframe::App::raw_input_hook(&mut self.app, &self.ctx, &mut preedit);
        let reserve = self.sent().into_iter().find(|request| request.1 == "targets.reserve").expect("IME reserves the cell and table owner");
        assert_eq!(reserve.2["node_ids"], json!(["cell", "table"]));
        reserve
    }

    fn advance_revision(&mut self, revision: u64) {
        self.app.active.as_mut().unwrap().revision = revision;
        self.app.artifact.as_mut().unwrap().revision = revision;
        self.app.context.as_mut().unwrap().revision = revision;
    }

    fn install_render(&mut self, revision: u64, include_cell_frame: bool) {
        let ArtifactBody::Canvas(document) = &self.app.artifact.as_ref().unwrap().body else { panic!("Canvas body") };
        let rendered = snapshot(&document.pages[0], revision, include_cell_frame);
        self.app.store_page_texture(&self.ctx, 0, &rendered, 1, 1, &[255, 255, 255, 255]);
        self.app.resume_inline_table_cell_after_render(0);
    }

    fn install_component_instance_table(&mut self, root_locked: bool, table_locked: bool, cell_locked: bool, covered: bool) {
        let cell = |id: &str, x: i64| Node {
            id: id.into(), kind: NodeKind::Rectangle,
            bounds: Rect { x, y: 60_000, width: 500_000, height: 180_000 }, text: String::new(),
            style: Style::default(), asset: None, children: Vec::new(),
            layout: Some(LayoutSpec { width_mode: "fill".into(), height_mode: "hug".into(), ..Default::default() }),
            interactions: Vec::new(), variants: Default::default(),
            ext: NodeExt { locked: cell_locked.then_some(true), ..NodeExt::default() },
        };
        let source_cells = vec![cell("source-anchor", 50_000), cell("source-cell", 550_000)];
        let spans = if covered {
            [("source-anchor".into(), CanvasTableSpan { row_span: 1, column_span: 2 })].into_iter().collect()
        } else {
            Default::default()
        };
        let source_table = Node {
            id: "source-table".into(), kind: NodeKind::Group,
            bounds: Rect { x: 40_000, y: 50_000, width: 1_000_000, height: 360_000 }, text: String::new(),
            style: Style::default(), asset: None, children: source_cells, layout: Some(LayoutSpec {
                mode: "grid".into(), height_mode: "hug".into(), grid_columns: Some(2), gap: 0, row_gap: Some(0), ..Default::default()
            }), interactions: Vec::new(), variants: Default::default(),
            ext: NodeExt {
                table: Some(CanvasTable {
                    rows: vec![CanvasTableRow { id: "source-row".into(), cell_ids: vec!["source-anchor".into(), "source-cell".into()] }],
                    header_row_id: None, column_widths: None, cell_spans: spans,
                }),
                locked: table_locked.then_some(true), ..NodeExt::default()
            },
        };
        let mut materialized_table = source_table.clone();
        materialized_table.id = "instance::source-table".into();
        for child in &mut materialized_table.children {
            child.id = format!("instance::{}", child.id);
        }
        let table = materialized_table.ext.table.as_mut().unwrap();
        for row in &mut table.rows {
            row.id = format!("instance::{}", row.id);
            for cell_id in &mut row.cell_ids { *cell_id = format!("instance::{cell_id}"); }
        }
        table.cell_spans = table.cell_spans.iter().map(|(anchor, span)| (format!("instance::{anchor}"), *span)).collect();
        let instance = Node {
            id: "instance".into(), kind: NodeKind::Group,
            bounds: Rect { x: 250_000, y: 350_000, width: 2_000_000, height: 1_000_000 }, text: String::new(),
            style: Style::default(), asset: None, children: vec![materialized_table], layout: None,
            interactions: Vec::new(), variants: Default::default(),
            ext: NodeExt {
                instance: Some(InstanceRef { component_page_id: "source-page".into(), overrides: Default::default() }),
                locked: root_locked.then_some(true), ..NodeExt::default()
            },
        };
        let regular = Page { nodes: vec![instance], ..Page::new("page-1", Size::slide()) };
        let source = Page { nodes: vec![source_table], role: Some(PageRole::Component), ..Page::new("source-page", Size::slide()) };
        let mut artifact = Artifact::new(
            ArtifactKind::Canvas,
            "instance table",
            "canvas.v1",
            ArtifactBody::Canvas(CanvasDocument { pages: vec![regular.clone(), source], ..Default::default() }),
        );
        artifact.id = "a1".into();
        artifact.revision = 1;
        self.app.active = Some(ArtifactSummary {
            id: artifact.id.clone(), kind: artifact.kind, title: artifact.title.clone(), revision: 1,
            engine_id: artifact.engine_id.clone(), dirty: false, modified_at_ms: None, active_jobs: Vec::new(),
        });
        self.app.artifact = Some(artifact);
        self.app.context = Some(serde_json::from_value(json!({
            "workspace_id": "w1", "artifact_id": "a1", "revision": 1, "view_id": self.app.view_id.clone(),
            "selection": { "node_ids": ["instance::source-cell"], "text_range": { "paragraph_id": "instance::source-cell", "start": 0, "end": 0 } },
            "visible_page": 0,
            "selected_properties": { "nodes": null, "stale_selection": null, "render_unavailable": null },
            "capabilities": { "text": { "read": "supported", "render": "supported", "edit": "supported", "preserve": "supported", "write": "supported", "reason": "" } },
            "render_snapshot": null, "view_transform": null
        })).expect("instance context"));
        self.app.selected_node = Some("instance::source-cell".into());
        self.app.selected_nodes = vec!["instance::source-cell".into()];
        self.app.local_selection = Some(Selection {
            node_ids: vec!["instance::source-cell".into()],
            text_range: Some(designllm_contracts::TextRange { paragraph_id: "instance::source-cell".into(), start: 0, end: 0 }),
        });
        let frame = Rect { x: 320_000, y: 410_000, width: 500_000, height: 180_000 };
        let mut frames = std::collections::BTreeMap::from([
            ("instance".to_owned(), Rect { x: 250_000, y: 350_000, width: 2_000_000, height: 1_000_000 }),
            ("instance::source-table".to_owned(), Rect { x: 300_000, y: 400_000, width: 1_000_000, height: 360_000 }),
            ("instance::source-anchor".to_owned(), Rect { x: 300_000, y: 400_000, width: 500_000, height: 180_000 }),
        ]);
        if !covered { frames.insert("instance::source-cell".into(), frame); }
        let source_map = if covered { Vec::new() } else {
            vec![SourceRegion { node_id: "instance::source-cell".into(), bounds: frame, logical_range: None, grapheme_rects: Vec::new() }]
        };
        let rendered = RenderSnapshot {
            id: "instance-snapshot-1".into(), artifact_id: "a1".into(), revision: 1,
            profile: RenderProfile::default(),
            scene: RenderScene {
                pages: vec![RenderPage { id: regular.id, page_index: 0, size: Size::slide(), primitives: Vec::new(), source_map }],
                font_fingerprint: "fixture".into(), diagnostics: Vec::new(),
                metadata: [("node_frames".into(), serde_json::to_value(frames).expect("instance frames"))].into_iter().collect(),
                links: Vec::new(),
            },
            preview: None, preview_transform: None,
        };
        self.app.store_page_texture(&self.ctx, 0, &rendered, 1, 1, &[255, 255, 255, 255]);
    }
}

fn snapshot(page: &Page, revision: u64, include_cell_frame: bool) -> RenderSnapshot {
    let frame = Rect { x: 70_000, y: 80_000, width: 500_000, height: 180_000 };
    let mut frames = std::collections::BTreeMap::from([
        ("table".to_owned(), page.nodes[0].bounds),
        ("anchor".to_owned(), Rect { x: frame.x, y: frame.y, width: 500_000, height: 180_000 }),
    ]);
    if include_cell_frame { frames.insert("cell".into(), frame); }
    let source_map = if include_cell_frame {
        vec![
            SourceRegion { node_id: "anchor".into(), bounds: frames["anchor"], logical_range: None, grapheme_rects: Vec::new() },
            SourceRegion { node_id: "cell".into(), bounds: frame, logical_range: None, grapheme_rects: Vec::new() },
        ]
    } else {
        Vec::new()
    };
    let rendered_page = RenderPage {
        id: page.id.clone(), page_index: 0, size: page.size, primitives: Vec::new(), source_map,
    };
    RenderSnapshot {
        id: format!("snapshot-{revision}"), artifact_id: "a1".into(), revision,
        profile: RenderProfile::default(),
        scene: RenderScene {
            pages: vec![rendered_page], font_fingerprint: "fixture".into(), diagnostics: Vec::new(),
            metadata: [("node_frames".into(), serde_json::to_value(frames).expect("frames"))].into_iter().collect(),
            links: Vec::new(),
        },
        preview: None,
        preview_transform: None,
    }
}

#[test]
fn stale_cell_scene_waits_before_retry_and_selection_drift_keeps_queued_input() {
    let mut harness = Harness::new();
    harness.begin_with_queued_text();
    harness.install_render(0, true);

    let status = harness.app.inline_text_edit.as_ref().map(|session| harness.app.inline_table_cell_readiness(session));
    assert_eq!(status, Some(InlineCellReadiness::WaitingForRender));
    harness.app.start_next_inline_text_change();
    assert_eq!(harness.app.inline_text_edit.as_ref().unwrap().queue.len(), 1, "stale geometry keeps the queued input");
    assert!(harness.sent().iter().all(|request| request.1 != "changes.apply"));

    harness.app.request_page(0, 96);
    let render = harness.sent().into_iter().find(|request| request.1 == "render.get").expect("the current revision is rendered");
    assert_eq!(render.2["expected_revision"], 1);

    harness.app.selected_nodes = vec!["other-cell".into()];
    harness.app.selected_node = Some("other-cell".into());
    harness.app.local_selection = Some(Selection { node_ids: vec!["other-cell".into()], text_range: None });
    harness.install_render(1, true);
    harness.app.retry_inline_text_edits();
    let session = harness.app.inline_text_edit.as_ref().expect("selection drift preserves the session");
    assert!(!session.retry_allowed);
    assert_eq!(session.queue.len(), 1);
    assert_eq!(harness.app.text_draft, "x");
    assert!(session.conflict.as_deref().is_some_and(|message| message.contains("선택한 표 셀")));
    assert!(harness.sent().iter().all(|request| request.1 != "changes.apply"), "a different selection cannot consume the queued edit");
}

#[test]
fn same_revision_fresh_frame_resumes_an_unsent_cell_edit_once() {
    let mut harness = Harness::new();
    harness.begin_with_queued_text();
    harness.install_render(0, true);
    assert_eq!(
        harness.app.inline_text_edit.as_ref().map(|session| harness.app.inline_table_cell_readiness(session)),
        Some(InlineCellReadiness::WaitingForRender),
    );
    harness.app.start_next_inline_text_change();
    assert!(harness.sent().iter().all(|request| request.1 != "changes.apply"));

    harness.install_render(1, true);
    let changes: Vec<_> = harness.sent().into_iter().filter(|request| request.1 == "changes.apply").collect();
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].2["base_revision"], 1);
    assert_eq!(harness.app.text_draft, "x");
}

#[test]
fn current_revision_without_a_cell_frame_fails_closed_and_keeps_the_draft() {
    let mut harness = Harness::new();
    harness.begin_with_queued_text();
    harness.install_render(1, false);

    let session = harness.app.inline_text_edit.as_ref().expect("the unavailable frame does not discard the session");
    assert!(session.conflict.as_deref().is_some_and(|message| message.contains("배치 정보를 받지 못했습니다")));
    assert!(!session.retry_allowed);
    assert_eq!(session.queue.len(), 1);
    assert_eq!(harness.app.text_draft, "x");
    harness.app.start_next_inline_text_change();
    assert_eq!(harness.app.inline_text_edit.as_ref().unwrap().queue.len(), 1);
    assert!(harness.sent().iter().all(|request| request.1 != "changes.apply"));
}

#[test]
fn ime_commit_after_unrelated_revision_drift_is_preserved_without_rebasing() {
    let mut harness = Harness::new();
    let reserve = harness.start_ime_and_take_reserve();
    harness.answer(&reserve, Ok(json!({ "reservation_id": "ime-cell-reservation", "expires_in_ms": 300000 })));
    let _ = harness.sent();

    harness.advance_revision(2);
    let mut commit = RawInput::default();
    commit.events.extend([
        Event::Ime(eframe::egui::ImeEvent::Commit("한".into())),
        Event::Text("한".into()),
    ]);
    eframe::App::raw_input_hook(&mut harness.app, &harness.ctx, &mut commit);
    let session = harness.app.inline_text_edit.as_ref().expect("the IME session remains open");
    assert_eq!(harness.app.text_draft, "한", "the IME commit updates the preserved draft while rendering is stale");
    assert_eq!(session.queue.len(), 1);
    assert!(session.queue.front().unwrap().ime_commit);
    assert!(session.in_flight.is_none());
    assert!(harness.sent().iter().all(|request| request.1 != "changes.apply"), "the stale frame blocks the ChangeSet");

    harness.install_render(2, true);
    let changes: Vec<_> = harness.sent().into_iter().filter(|request| request.1 == "changes.apply").collect();
    assert!(changes.is_empty(), "a fresh frame cannot prove that the cell ID still names the same object");
    let session = harness.app.inline_text_edit.as_ref().expect("revision drift keeps the composed draft");
    assert_eq!(session.revision, 1, "the old base revision is not rebased");
    assert!(!session.retry_allowed);
    assert_eq!(session.queue.len(), 1);
    assert!(session.conflict.as_deref().is_some_and(|message| message.contains("같은 ID가 같은 표 셀인지 확인할 수 없습니다")));
    assert_eq!(harness.app.text_draft, "한");
}

#[test]
fn identical_recreated_cell_is_not_retried_after_revision_drift() {
    let mut harness = Harness::new();
    harness.begin_with_queued_text();
    let ArtifactBody::Canvas(document) = &mut harness.app.artifact.as_mut().unwrap().body else { panic!("Canvas body") };
    let table = document.pages[0].nodes.iter_mut().find(|node| node.id == "table").expect("table");
    let index = table.children.iter().position(|cell| cell.id == "cell").expect("cell");
    let identical_replacement = table.children[index].clone();
    table.children.remove(index);
    table.children.insert(index, identical_replacement);
    harness.advance_revision(2);
    harness.install_render(2, true);

    let status = harness.app.inline_text_edit.as_ref().map(|session| harness.app.inline_table_cell_readiness(session));
    assert_eq!(status, Some(InlineCellReadiness::RevisionDrift));
    harness.app.retry_inline_text_edits();
    let session = harness.app.inline_text_edit.as_ref().expect("the full draft remains available");
    assert_eq!(harness.app.text_draft, "x");
    assert_eq!(session.queue.len(), 1);
    assert!(!session.retry_allowed);
    assert!(session.conflict.as_deref().is_some_and(|message| message.contains("같은 ID가 같은 표 셀인지 확인할 수 없습니다")));
    assert!(harness.sent().iter().all(|request| request.1 != "changes.apply"));
}

#[test]
fn ime_disabled_commit_survives_a_merge_that_wins_the_pending_reservation() {
    let mut harness = Harness::new();
    let reserve = harness.start_ime_and_take_reserve();
    if let ArtifactBody::Canvas(document) = &mut harness.app.artifact.as_mut().unwrap().body {
        let table = document.pages[0].nodes.iter_mut().find(|node| node.id == "table").unwrap()
            .ext.table.as_mut().unwrap();
        table.cell_spans.insert("anchor".into(), CanvasTableSpan { row_span: 1, column_span: 2 });
    }
    harness.advance_revision(2);

    let mut commit = RawInput::default();
    commit.events.extend([
        Event::Text("한".into()),
        Event::Ime(eframe::egui::ImeEvent::Disabled),
    ]);
    eframe::App::raw_input_hook(&mut harness.app, &harness.ctx, &mut commit);
    let session = harness.app.inline_text_edit.as_ref().expect("the covered-cell draft remains available");
    assert_eq!(harness.app.text_draft, "한", "the delayed IME Disabled commit is not lost when the merge wins");
    assert_eq!(session.queue.len(), 1);
    assert!(session.queue.front().unwrap().ime_commit);
    assert!(session.in_flight.is_none());
    assert!(session.conflict.is_some());
    assert!(harness.sent().iter().all(|request| request.1 != "changes.apply"));

    let mut stale = DltError::new(ErrorCode::RevisionConflict, "reservation used an old revision");
    stale.current_revision = Some(2);
    harness.answer(&reserve, Err(stale));
    let session = harness.app.inline_text_edit.as_ref().expect("reserve refusal keeps the composed draft");
    assert_eq!(harness.app.text_draft, "한");
    assert_eq!(session.queue.len(), 1);
    assert!(session.retry_expected);
    assert!(harness.sent().iter().all(|request| request.1 != "changes.apply"), "a now-covered cell is never edited");
}

struct NoAssets;

impl AssetStore for NoAssets {
    fn put(&self, _bytes: &[u8], _mime: &str) -> DltResult<AssetRef> {
        Err(DltError::new(ErrorCode::NotFound, "fixture has no assets"))
    }
    fn read(&self, _asset: &AssetRef) -> DltResult<Vec<u8>> {
        Err(DltError::new(ErrorCode::NotFound, "fixture has no assets"))
    }
    fn lookup(&self, _hash: &str) -> DltResult<AssetRef> {
        Err(DltError::new(ErrorCode::NotFound, "fixture has no assets"))
    }
}

#[test]
fn component_instance_cell_uses_derived_ids_and_local_text_override_survives_source_refresh() {
    let mut harness = Harness::new();
    harness.install_component_instance_table(false, false, false, false);
    assert!(harness.app.begin_inline_text_edit(&harness.ctx, "instance::source-cell", 0, None));
    let _ = harness.sent();
    let session = harness.app.inline_text_edit.as_ref().unwrap();
    assert_eq!(session.reservation_node_ids, ["instance::source-cell", "instance::source-table", "instance"]);

    let edit = TextInputEdit {
        request_id: designllm_contracts::new_id(), range: 0..0, paragraph_index: 0, paragraph_range: Some(0..0),
        caret_after: 6, paragraph_after: 0, caret_after_in_paragraph: 6, text: "local".into(),
        ime_commit: false, ime_generation: 0, after_text: None,
    };
    harness.app.text_draft = "local".into();
    harness.app.inline_text_edit.as_mut().unwrap().queue.push_back(edit);
    harness.app.start_next_inline_text_change();
    let change = harness.sent().into_iter().find(|request| request.1 == "changes.apply").expect("derived cell edit request");
    assert_eq!(change.2["operations"], json!([{
        "op": "replace_text",
        "range": { "paragraph_id": "instance::source-cell", "start": 0, "end": 0 },
        "text": "local"
    }]));

    let base = harness.app.artifact.as_ref().unwrap().clone();
    let operations: Vec<Operation> = serde_json::from_value(change.2["operations"].clone()).expect("ReplaceText");
    let assets = NoAssets;
    let font_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets/fonts");
    let renderer = designllm_render::Renderer::bundled_only(&font_dir).expect("bundled fonts");
    let engine_context = EngineContext { assets: &assets, renderer: &renderer };
    let mut local = designllm_canvas::CanvasEngine.prepare_change(&base, &operations, &engine_context)
        .expect("Canvas captures a local text override for the derived cell").artifact;
    local.revision = 2;
    let ArtifactBody::Canvas(document) = &local.body else { panic!("Canvas body") };
    let instance = document.pages[0].nodes.iter().find(|node| node.id == "instance").unwrap();
    let overrides = &instance.ext.instance.as_ref().unwrap().overrides;
    assert_eq!(overrides.get("source-cell").and_then(|over| over.text.as_deref()), Some("local"));
    assert_eq!(super::doc_tree::find_node(&local, "instance::source-cell").unwrap().text, "local");

    let source_edit = Operation::SetText { node_id: "source-cell".into(), text: "new source text".into() };
    let mut refreshed = designllm_canvas::CanvasEngine.prepare_change(&local, &[source_edit], &engine_context)
        .expect("the source refresh keeps the instance override").artifact;
    refreshed.revision = 3;
    let ArtifactBody::Canvas(document) = &refreshed.body else { panic!("Canvas body") };
    let instance = document.pages[0].nodes.iter().find(|node| node.id == "instance").unwrap();
    assert_eq!(instance.ext.instance.as_ref().unwrap().overrides.get("source-cell").and_then(|over| over.text.as_deref()), Some("local"));
    assert_eq!(super::doc_tree::find_node(&refreshed, "source-cell").unwrap().text, "new source text");
    assert_eq!(super::doc_tree::find_node(&refreshed, "instance::source-cell").unwrap().text, "local");
}

#[test]
fn component_instance_cell_refuses_root_table_cell_lock_covered_and_bad_id_mapping() {
    for (root_locked, table_locked, cell_locked, covered) in [
        (true, false, false, false),
        (false, true, false, false),
        (false, false, true, false),
        (false, false, false, true),
    ] {
        let mut harness = Harness::new();
        harness.install_component_instance_table(root_locked, table_locked, cell_locked, covered);
        assert!(!harness.app.begin_inline_text_edit(&harness.ctx, "instance::source-cell", 0, None));
        assert!(harness.app.inline_text_edit.is_none());
        assert!(harness.sent().iter().all(|request| !matches!(request.1.as_str(), "changes.apply" | "targets.reserve")));
    }

    let mut mismatch = Harness::new();
    mismatch.install_component_instance_table(false, false, false, false);
    let ArtifactBody::Canvas(document) = &mut mismatch.app.artifact.as_mut().unwrap().body else { panic!("Canvas body") };
    let table = document.pages[0].nodes[0].children[0].ext.table.as_mut().unwrap();
    table.rows[0].cell_ids[1] = "instance::different-cell".into();
    assert!(!mismatch.app.begin_inline_text_edit(&mismatch.ctx, "instance::source-cell", 0, None));
    assert!(mismatch.app.inline_text_edit.is_none());
}

#[test]
fn component_instance_cell_revision_drift_keeps_the_local_draft_without_rebase() {
    let mut harness = Harness::new();
    harness.install_component_instance_table(false, false, false, false);
    assert!(harness.app.begin_inline_text_edit(&harness.ctx, "instance::source-cell", 0, None));
    let _ = harness.sent();
    let edit = TextInputEdit {
        request_id: designllm_contracts::new_id(), range: 0..0, paragraph_index: 0, paragraph_range: Some(0..0),
        caret_after: 1, paragraph_after: 0, caret_after_in_paragraph: 1, text: "x".into(),
        ime_commit: false, ime_generation: 0, after_text: None,
    };
    harness.app.text_draft = "x".into();
    harness.app.inline_text_edit.as_mut().unwrap().queue.push_back(edit);
    harness.advance_revision(2);
    let status = harness.app.inline_text_edit.as_ref().map(|session| harness.app.inline_table_cell_readiness(session));
    assert_eq!(status, Some(InlineCellReadiness::RevisionDrift));
    harness.app.start_next_inline_text_change();
    let session = harness.app.inline_text_edit.as_ref().unwrap();
    assert_eq!(session.queue.len(), 1);
    assert_eq!(harness.app.text_draft, "x");
    assert!(!session.retry_allowed);
    assert!(session.conflict.as_deref().is_some_and(|message| message.contains("같은 ID가 같은 표 셀인지 확인할 수 없습니다")));
    assert!(harness.sent().iter().all(|request| request.1 != "changes.apply"));
}

fn install_populated_component_cover(harness: &mut Harness, root_locked: bool) {
    harness.install_component_instance_table(root_locked, false, false, true);
    let covered_bounds = Rect { x: 320_000, y: 410_000, width: 500_000, height: 180_000 };
    {
        let ArtifactBody::Canvas(document) = &mut harness.app.artifact.as_mut().unwrap().body else { panic!("Canvas body") };
        let source_table = document.pages[1].nodes.iter_mut().find(|node| node.id == "source-table").unwrap();
        source_table.children[1].text = "source covered text".into();
        let instance_table = document.pages[0].nodes[0].children.iter_mut().find(|node| node.id == "instance::source-table").unwrap();
        instance_table.children[1].text = "source covered text".into();
        document.pages[0].nodes[0].ext.instance.as_mut().unwrap().overrides.insert(
            "source-anchor".into(),
            NodeOverride { text: Some("local anchor override".into()), ..NodeOverride::default() },
        );
    }
    let texture = harness.app.page_textures.get_mut(&0).expect("instance page texture");
    texture.frames.insert("instance::source-cell".into(), covered_bounds);
    std::sync::Arc::make_mut(&mut texture.page).source_map = vec![SourceRegion {
        node_id: "instance::source-cell".into(),
        bounds: covered_bounds,
        logical_range: Some(designllm_contracts::TextRange { paragraph_id: "instance::source-cell".into(), start: 0, end: 19 }),
        grapheme_rects: Vec::new(),
    }];
    harness.app.page_transforms.insert(0, ViewTransform {
        id: "transform-instance".into(), dpi: 96, zoom_milli: 1000,
        offset_x_micropx: 0, offset_y_micropx: 0, crop: None,
    });
    let _ = harness.sent();
}

#[test]
fn covered_component_instance_text_hit_selects_only_the_verified_derived_anchor() {
    let mut harness = Harness::new();
    install_populated_component_cover(&mut harness, true);

    harness.app.hit_test_page(0, eframe::egui::Pos2::new(30.0, 40.0), 1.0, false, true);
    let request = harness.sent();
    assert_eq!(request.len(), 1);
    assert_eq!(request[0].1, "render.hit_test");
    harness.answer(&request[0], Ok(json!({
        "hit": { "node_id": "instance::source-cell", "position": { "paragraph_id": "instance::source-cell", "grapheme_index": 4, "trailing": false } }
    })));

    assert_eq!(harness.app.selected_node.as_deref(), Some("instance::source-anchor"));
    assert_eq!(harness.app.selected_nodes, vec!["instance::source-anchor".to_owned()]);
    assert!(harness.app.inline_text_edit.is_none(), "covered instance text is not redirected into anchor editing");
    assert!(harness.app.canvas_table_span_operation().is_none(), "instance table structure edits stay on the source page");
    assert!(harness.message().is_some_and(|message| message.contains("원본 페이지")));
    let followup = harness.sent();
    assert!(followup.iter().all(|request| !matches!(request.1.as_str(), "changes.apply" | "targets.reserve")));

    let ArtifactBody::Canvas(document) = &harness.app.artifact.as_ref().unwrap().body else { panic!("Canvas body") };
    let source_table = &document.pages[1].nodes[0];
    let instance_root = &document.pages[0].nodes[0];
    assert_eq!(instance_root.ext.locked, Some(true), "selection does not change the instance lock");
    assert_eq!(source_table.children[1].id, "source-cell");
    assert_eq!(source_table.children[1].text, "source covered text");
    assert_eq!(instance_root.ext.instance.as_ref().unwrap().overrides.get("source-anchor").and_then(|over| over.text.as_deref()), Some("local anchor override"));
    assert_eq!(instance_root.children[0].children[1].id, "instance::source-cell");
    assert_eq!(instance_root.children[0].children[1].text, "source covered text");
}

#[test]
fn covered_component_text_hit_keeps_single_click_normal_and_blocks_enter_from_editing_anchor() {
    let mut harness = Harness::new();
    install_populated_component_cover(&mut harness, false);

    harness.app.hit_test_page(0, eframe::egui::Pos2::new(30.0, 40.0), 1.0, false, false);
    let request = harness.sent();
    assert_eq!(request.len(), 1);
    harness.answer(&request[0], Ok(json!({
        "hit": { "node_id": "instance::source-cell", "position": { "paragraph_id": "instance::source-cell", "grapheme_index": 4, "trailing": false } }
    })));

    assert_eq!(harness.app.selected_node.as_deref(), Some("instance::source-anchor"));
    assert!(harness.app.inline_text_edit.is_none());
    assert!(harness.message().is_none(), "single click is ordinary selection, not an edit attempt");
    assert!(harness.sent().iter().all(|work| !matches!(work.1.as_str(), "changes.apply" | "targets.reserve")));

    let page_index = super::board::node_page_index(harness.app.artifact.as_ref().expect("artifact"), "instance::source-anchor")
        .expect("selected anchor remains on the instance page");
    assert!(
        !harness.app.begin_inline_text_edit(&harness.ctx, "instance::source-anchor", page_index, None),
        "the board Enter path cannot redirect a covered-text selection into the anchor editor",
    );
    assert!(harness.app.inline_text_edit.is_none());
    assert!(harness.message().is_some_and(|message| message.contains("원본 페이지")));
    assert!(harness.sent().iter().all(|work| !matches!(work.1.as_str(), "changes.apply" | "targets.reserve")));
}
