//! A hit test may arrive after its rendered page stopped matching the open document.

use std::sync::mpsc::{Receiver, Sender};

use designllm_contracts::{
    Artifact, ArtifactBody, ArtifactKind, ArtifactSummary, CanvasDocument, DltError, Node, NodeKind, Page, Rect,
    RenderPage, RenderProfile, RenderScene, RenderSnapshot, Size, SourceRegion, Style, ViewTransform,
};
use eframe::egui::Context;
use serde_json::{json, Value};

use super::{DesktopApp, PendingAction};
use crate::rpc::{RpcPayload, RpcReply, RpcWorker, Work};

type Request = (u64, String, Value);

struct Harness {
    app: DesktopApp,
    replies: Sender<RpcReply>,
    work: Receiver<Work>,
    ctx: Context,
}

impl Harness {
    fn new_with_text_moved_to_second_page() -> Self {
        let (rpc, replies, work) = RpcWorker::detached();
        let mut app = DesktopApp::new(rpc, "late hit-test fixture".into(), None, None);
        while work.try_recv().is_ok() {}

        let bounds = Rect { x: 100_000, y: 200_000, width: 800_000, height: 300_000 };
        let text = Node {
            id: "text-1".into(),
            kind: NodeKind::Text,
            bounds,
            text: "moved".into(),
            style: Style::default(),
            asset: None,
            children: Vec::new(),
            layout: None,
            interactions: Vec::new(),
            variants: Default::default(),
            ext: Default::default(),
        };
        let page0 = Page::new("page-0", Size::slide());
        let page1 = Page { nodes: vec![text], ..Page::new("page-1", Size::slide()) };
        let mut artifact = Artifact::new(
            ArtifactKind::Canvas,
            "moved text",
            "canvas.v1",
            ArtifactBody::Canvas(CanvasDocument { pages: vec![page0.clone(), page1], ..Default::default() }),
        );
        artifact.id = "a1".into();
        artifact.revision = 2;
        app.screen = super::Screen::Editor;
        app.active = Some(ArtifactSummary {
            id: artifact.id.clone(), kind: artifact.kind, title: artifact.title.clone(), revision: 2,
            engine_id: artifact.engine_id.clone(), dirty: false, modified_at_ms: None, active_jobs: Vec::new(),
        });
        app.context = Some(serde_json::from_value(json!({
            "workspace_id": "w1", "artifact_id": "a1", "revision": 2, "view_id": app.view_id.clone(),
            "selection": { "node_ids": [], "text_range": null }, "visible_page": 0,
            "selected_properties": { "nodes": null, "stale_selection": null, "render_unavailable": null },
            "capabilities": { "text": { "read": "supported", "render": "supported", "edit": "supported", "preserve": "supported", "write": "supported", "reason": "" } },
            "render_snapshot": null, "view_transform": null
        })).expect("context"));
        app.artifact = Some(artifact);
        app.selected_node = None;
        app.selected_nodes.clear();
        app.local_selection = Some(serde_json::from_value(json!({ "node_ids": [], "text_range": null })).expect("empty selection"));
        app.reservations_for = Some("a1".into());
        app.page_transforms.insert(0, ViewTransform {
            id: "transform-1".into(), dpi: 96, zoom_milli: 1000, offset_x_micropx: 0, offset_y_micropx: 0, crop: None,
        });

        // This is the old page-0 snapshot from before `text-1` moved to page 1.
        let old_scene = RenderSnapshot {
            id: "old-snapshot".into(), artifact_id: "a1".into(), revision: 1,
            profile: RenderProfile::default(),
            scene: RenderScene {
                pages: vec![RenderPage {
                    id: page0.id,
                    page_index: 0,
                    size: Size::slide(),
                    primitives: Vec::new(),
                    source_map: vec![SourceRegion { node_id: "text-1".into(), bounds, logical_range: None, grapheme_rects: Vec::new() }],
                }],
                font_fingerprint: "fixture".into(), diagnostics: Vec::new(),
                metadata: [("node_frames".into(), json!({ "text-1": bounds }))].into_iter().collect(),
                links: Vec::new(),
            },
            preview: None,
            preview_transform: None,
        };
        let ctx = Context::default();
        app.store_page_texture(&ctx, 0, &old_scene, 1, 1, &[255, 255, 255, 255]);
        Self { app, replies, work, ctx }
    }

    fn sent(&mut self) -> Vec<Request> {
        let mut batch = Vec::new();
        while let Ok(work) = self.work.try_recv() {
            if let Work::Call { ticket, method, params, .. } = work {
                batch.push((ticket, method, params));
            }
        }
        batch
    }

    fn answer(&mut self, request: &Request, result: Result<Value, DltError>) {
        self.replies.send(RpcReply { ticket: request.0, result: result.map(RpcPayload::Json) }).expect("app is listening");
        self.app.process_replies(&self.ctx);
    }
}

#[test]
fn stale_page_hit_reply_cannot_start_edit_after_the_node_moved() {
    let mut harness = Harness::new_with_text_moved_to_second_page();
    assert!(!harness.app.begin_inline_text_edit(&harness.ctx, "text-1", 0, None), "begin rechecks the node's current page");

    let ticket = harness.app.call(
        "render.hit_test",
        json!({ "artifact_id": "a1", "snapshot_id": "old-snapshot", "transform_id": "transform-1", "page_index": 0 }),
        PendingAction::HitTest {
            snapshot_id: "old-snapshot".into(), page_index: 0, transform_id: "transform-1".into(), additive: false, edit_text: true,
        },
    ).expect("test hit-test request");
    let request = (ticket, "render.hit_test".into(), Value::Null);
    let _ = harness.sent();
    harness.answer(&request, Ok(json!({ "hit": { "node_id": "text-1", "position": null } })));

    assert!(harness.app.inline_text_edit.is_none());
    assert!(harness.app.selected_nodes.is_empty(), "the stale hit result cannot change the current page selection");
    assert!(harness.sent().iter().all(|request| !matches!(request.1.as_str(), "selection.set" | "changes.apply" | "targets.reserve")));
}
