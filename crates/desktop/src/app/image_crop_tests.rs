//! Canvas image crop UI requests retain their source image and draft across stale replies.

use std::sync::mpsc::{Receiver, Sender};

use designllm_contracts::{
    Artifact, ArtifactBody, ArtifactKind, ArtifactSummary, AssetRef, CanvasDocument, DltError, ErrorCode,
    ImageFit, InstanceRef, Node, NodeExt, NodeKind, Page, PageRole, PixelRect, Rect, Selection, Size, Style,
};
use eframe::egui::Context;
use serde_json::{json, Value};

use super::{DesktopApp, ImageCropStage, PendingAction, UserReservation};
use crate::rpc::{RpcPayload, RpcReply, RpcWorker, Work};

type Request = (u64, String, Value);

struct Harness {
    app: DesktopApp,
    replies: Sender<RpcReply>,
    work: Receiver<Work>,
    ctx: Context,
}

impl Harness {
    fn new(crop: Option<PixelRect>) -> Self {
        let (rpc, replies, work) = RpcWorker::detached();
        let mut app = DesktopApp::new(rpc, "image crop fixture".into(), None, None);
        while work.try_recv().is_ok() {}
        let image = Node {
            id: "image".into(),
            kind: NodeKind::Image,
            bounds: Rect { x: 123_000, y: 234_000, width: 1_200_000, height: 800_000 },
            text: String::new(),
            style: Style::default(),
            asset: Some(AssetRef { sha256: "a".repeat(64), mime: "image/png".into(), bytes: 4096 }),
            children: Vec::new(),
            layout: None,
            interactions: Vec::new(),
            variants: Default::default(),
            ext: NodeExt { image_fit: Some(ImageFit::Contain), image_crop_pixel_rect: crop, ..NodeExt::default() },
        };
        let page = Page { nodes: vec![image], ..Page::new("page-1", Size::slide()) };
        let mut artifact = Artifact::new(
            ArtifactKind::Canvas,
            "image crop fixture",
            "canvas.v1",
            ArtifactBody::Canvas(CanvasDocument { pages: vec![page], ..Default::default() }),
        );
        artifact.id = "a1".into();
        artifact.revision = 1;
        app.screen = super::Screen::Editor;
        app.active = Some(ArtifactSummary {
            id: artifact.id.clone(), kind: artifact.kind, title: artifact.title.clone(), revision: 1,
            engine_id: artifact.engine_id.clone(), dirty: false, modified_at_ms: None, active_jobs: Vec::new(),
        });
        app.context = Some(serde_json::from_value(json!({
            "workspace_id": "w1", "artifact_id": "a1", "revision": 1, "view_id": app.view_id.clone(),
            "selection": { "node_ids": ["image"], "text_range": null }, "visible_page": 0,
            "selected_properties": { "nodes": null, "stale_selection": null, "render_unavailable": null },
            "capabilities": {
                "images": { "read": "supported", "render": "supported", "edit": "supported", "preserve": "supported", "write": "supported", "reason": "" }
            },
            "render_snapshot": null, "view_transform": null
        })).expect("context"));
        app.artifact = Some(artifact);
        app.selected_node = Some("image".into());
        app.selected_nodes = vec!["image".into()];
        app.local_selection = Some(Selection { node_ids: vec!["image".into()], text_range: None });
        app.reservations_for = Some("a1".into());
        Self { app, replies, work, ctx: Context::default() }
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

    fn open_crop(&mut self) -> Request {
        self.app.begin_canvas_image_crop("image").expect("crop lookup starts for the selected image");
        self.sent().into_iter().find(|request| request.1 == "asset.lookup").expect("asset lookup request")
    }

    fn answer_dimensions(&mut self, request: &Request) {
        self.answer(request, Ok(json!({
            "sha256": "a".repeat(64), "mime": "image/png", "bytes": 4096,
            "image": { "width_px": 640, "height_px": 480 }
        })));
    }

    fn advance_revision(&mut self, revision: u64) {
        self.app.active.as_mut().unwrap().revision = revision;
        self.app.artifact.as_mut().unwrap().revision = revision;
        self.app.context.as_mut().unwrap().revision = revision;
    }
}

#[test]
fn crop_apply_uses_source_pixel_rect_captured_revision_and_asset_hash() {
    let mut harness = Harness::new(None);
    let lookup = harness.open_crop();
    assert_eq!(lookup.2["sha256"], "a".repeat(64));
    harness.answer_dimensions(&lookup);
    assert_eq!(harness.app.image_crop_editor.as_ref().unwrap().dimensions, Some((640, 480)));

    let fields = ["4", "6", "200", "100"].map(str::to_owned);
    harness.app.update_canvas_image_crop_draft(fields);
    assert!(harness.app.has_close_work(), "unapplied pixel inputs participate in the close guard");
    let crop = PixelRect { x: 4, y: 6, width: 200, height: 100 };
    harness.app.apply_canvas_image_crop(Some(crop)).expect("valid crop applies");
    let request = harness.sent().into_iter().find(|request| request.1 == "changes.apply").expect("change request");
    assert_eq!(request.2["base_revision"], 1);
    assert_eq!(request.2["allowed_intents"], json!(["design"]));
    assert_eq!(request.2["operations"], json!([{
        "op": "set_canvas_image_crop",
        "node_id": "image",
        "expected_asset_sha256": "a".repeat(64),
        "crop_pixel_rect": { "x": 4, "y": 6, "width": 200, "height": 100 }
    }]));
    assert!(matches!(harness.app.pending.get(&request.0).map(|pending| &pending.action), Some(PendingAction::ImageCropApply { .. })));
    assert_eq!(harness.app.image_crop_editor.as_ref().unwrap().stage, ImageCropStage::Applying);

    harness.answer(&request, Ok(json!({ "revision": 2, "changed_ids": ["image"], "diagnostics": [] })));
    assert!(harness.app.image_crop_editor.is_none(), "success resolves the retained draft");
}

#[test]
fn crop_reset_is_an_explicit_none_operation() {
    let mut harness = Harness::new(Some(PixelRect { x: 10, y: 15, width: 300, height: 200 }));
    let lookup = harness.open_crop();
    harness.answer_dimensions(&lookup);
    harness.app.apply_canvas_image_crop(None).expect("reset applies");
    let request = harness.sent().into_iter().find(|request| request.1 == "changes.apply").expect("reset request");
    assert_eq!(request.2["operations"][0]["crop_pixel_rect"], Value::Null);
}

#[test]
fn refused_crop_keeps_the_user_rect_available_for_recovery() {
    let mut harness = Harness::new(None);
    let lookup = harness.open_crop();
    harness.answer_dimensions(&lookup);
    let fields = ["12", "18", "160", "140"].map(str::to_owned);
    harness.app.update_canvas_image_crop_draft(fields.clone());
    let crop = PixelRect { x: 12, y: 18, width: 160, height: 140 };
    harness.app.apply_canvas_image_crop(Some(crop)).expect("valid crop request");
    let request = harness.sent().into_iter().find(|request| request.1 == "changes.apply").expect("change request");
    harness.answer(&request, Err(DltError::new(ErrorCode::RevisionConflict, "the image changed before crop apply")));

    let editor = harness.app.image_crop_editor.as_ref().expect("refusal retains the crop editor");
    assert_eq!(editor.stage, ImageCropStage::Editing);
    assert_eq!(editor.draft.as_ref().unwrap(), &fields);
    assert_eq!(editor.requested_crop, Some(Some(crop)));
    assert!(harness.app.has_close_work(), "a refused crop still needs explicit recovery or discard");
}

#[test]
fn unknown_crop_result_is_refreshed_and_never_retried_blindly() {
    let mut harness = Harness::new(None);
    let lookup = harness.open_crop();
    harness.answer_dimensions(&lookup);
    let crop = PixelRect { x: 12, y: 18, width: 160, height: 140 };
    harness.app.update_canvas_image_crop_draft(["12", "18", "160", "140"].map(str::to_owned));
    harness.app.apply_canvas_image_crop(Some(crop)).expect("valid crop request");
    let request = harness.sent().into_iter().find(|request| request.1 == "changes.apply").expect("change request");
    harness.answer(&request, Err(DltError::new(ErrorCode::Unavailable, "crop reply is unavailable")));

    let editor = harness.app.image_crop_editor.as_ref().expect("unknown outcome preserves the crop draft");
    assert!(editor.outcome_uncertain);
    assert_eq!(editor.requested_crop, Some(Some(crop)));
    let refresh = harness.sent();
    assert!(refresh.iter().any(|request| request.1 == "artifact.inspect"));
    assert!(refresh.iter().any(|request| request.1 == "context.read"));
    assert!(harness.app.apply_canvas_image_crop(Some(crop)).is_err(), "unknown outcomes are not resent without a fresh comparison");
    assert!(harness.sent().iter().all(|request| request.1 != "changes.apply"));
}

#[test]
fn stale_revision_or_asset_keeps_crop_draft_and_sends_no_change() {
    let mut stale_revision = Harness::new(None);
    let lookup = stale_revision.open_crop();
    stale_revision.answer_dimensions(&lookup);
    let fields = ["20", "30", "250", "160"].map(str::to_owned);
    stale_revision.app.update_canvas_image_crop_draft(fields.clone());
    stale_revision.advance_revision(2);
    assert!(stale_revision.app.apply_canvas_image_crop(Some(PixelRect { x: 20, y: 30, width: 250, height: 160 })).is_err());
    assert!(stale_revision.sent().iter().all(|request| request.1 != "changes.apply"));
    assert_eq!(stale_revision.app.image_crop_editor.as_ref().unwrap().draft.as_ref().unwrap(), &fields);

    let mut changed_asset = Harness::new(None);
    let lookup = changed_asset.open_crop();
    changed_asset.answer_dimensions(&lookup);
    changed_asset.app.update_canvas_image_crop_draft(fields.clone());
    if let ArtifactBody::Canvas(document) = &mut changed_asset.app.artifact.as_mut().unwrap().body {
        document.pages[0].nodes[0].asset.as_mut().unwrap().sha256 = "b".repeat(64);
    }
    assert!(changed_asset.app.apply_canvas_image_crop(Some(PixelRect { x: 20, y: 30, width: 250, height: 160 })).is_err());
    assert!(changed_asset.sent().iter().all(|request| request.1 != "changes.apply"));
    assert_eq!(changed_asset.app.image_crop_editor.as_ref().unwrap().draft.as_ref().unwrap(), &fields);
}

#[test]
fn locked_or_figure_linked_image_crop_is_rejected_before_lookup() {
    let mut locked = Harness::new(None);
    if let ArtifactBody::Canvas(document) = &mut locked.app.artifact.as_mut().unwrap().body {
        document.pages[0].nodes[0].ext.locked = Some(true);
    }
    assert!(locked.app.begin_canvas_image_crop("image").is_err());
    assert!(locked.sent().iter().all(|request| request.1 != "asset.lookup"));

    let mut linked = Harness::new(None);
    linked.app.artifact.as_mut().unwrap().figure_links.push(designllm_contracts::FigureLink {
        figure_id: "figure".into(), figure_revision: 1, target_node_id: "image".into(),
        representation: "png".into(), render_hash: "c".repeat(64),
    });
    assert!(linked.app.begin_canvas_image_crop("image").is_err());
    assert!(linked.sent().iter().all(|request| request.1 != "asset.lookup"));

    let mut component_source = Harness::new(None);
    if let ArtifactBody::Canvas(document) = &mut component_source.app.artifact.as_mut().unwrap().body {
        document.pages[0].role = Some(PageRole::Component);
    }
    assert!(component_source.app.begin_canvas_image_crop("image").is_err());
    assert!(component_source.sent().iter().all(|request| request.1 != "asset.lookup"));

    let mut instance = Harness::new(None);
    if let ArtifactBody::Canvas(document) = &mut instance.app.artifact.as_mut().unwrap().body {
        let image = document.pages[0].nodes.remove(0);
        document.pages[0].nodes.push(Node {
            id: "instance".into(), kind: NodeKind::Group, bounds: image.bounds, text: String::new(),
            style: Style::default(), asset: None, children: vec![image], layout: None,
            interactions: Vec::new(), variants: Default::default(),
            ext: NodeExt { instance: Some(InstanceRef { component_page_id: "component-page".into(), overrides: Default::default() }), ..NodeExt::default() },
        });
    }
    assert!(instance.app.begin_canvas_image_crop("image").is_err());
    assert!(instance.sent().iter().all(|request| request.1 != "asset.lookup"));
}

#[test]
fn a_reserved_image_cannot_open_crop_editing() {
    let mut harness = Harness::new(None);
    harness.app.user_reservations.push(UserReservation { node_ids: vec!["image".into()] });
    assert!(harness.app.begin_canvas_image_crop("image").is_err());
    assert!(harness.sent().iter().all(|request| request.1 != "asset.lookup"));
}
