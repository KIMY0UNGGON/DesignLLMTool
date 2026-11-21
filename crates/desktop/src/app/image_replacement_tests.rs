//! Canvas image replacement preserves the selected target across asynchronous import.

use std::sync::mpsc::{Receiver, Sender};

use designllm_contracts::{
    Artifact, ArtifactBody, ArtifactKind, ArtifactSummary, AssetRef, CanvasDocument, DltError, ErrorCode, FigureLink,
    ImageFit, InstanceRef, Intent, Node, NodeExt, NodeKind, Operation, Page, PageRole, Rect, Selection, Size, Style,
};
use eframe::egui::{Context, Pos2, Rect as ScreenRect, Vec2};
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
    fn new() -> Self {
        let (rpc, replies, work) = RpcWorker::detached();
        let mut app = DesktopApp::new(rpc, "image replacement fixture".into(), None, None);
        while work.try_recv().is_ok() {}
        let image = image_node("image", "a".repeat(64));
        let other = Node {
            id: "other".into(), kind: NodeKind::Rectangle, bounds: Rect { x: 0, y: 0, width: 300_000, height: 200_000 },
            text: String::new(), style: Style::default(), asset: None, children: Vec::new(), layout: None,
            interactions: Vec::new(), variants: Default::default(), ext: NodeExt::default(),
        };
        let text = Node {
            id: "text-1".into(), kind: NodeKind::Text, bounds: Rect { x: 0, y: 300_000, width: 300_000, height: 100_000 },
            text: "A".into(), style: Style::default(), asset: None, children: Vec::new(), layout: None,
            interactions: Vec::new(), variants: Default::default(), ext: NodeExt::default(),
        };
        let page = Page { nodes: vec![image, other, text], ..Page::new("page-1", Size::slide()) };
        let mut artifact = Artifact::new(
            ArtifactKind::Canvas,
            "image replacement fixture",
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
                "images": { "read": "supported", "render": "supported", "edit": "supported", "preserve": "supported", "write": "supported", "reason": "" },
                "text": { "read": "supported", "render": "supported", "edit": "supported", "preserve": "supported", "write": "supported", "reason": "" },
                "geometry": { "read": "supported", "render": "supported", "edit": "supported", "preserve": "supported", "write": "supported", "reason": "" }
            },
            "render_snapshot": null, "view_transform": null
        })).expect("context"));
        app.artifact = Some(artifact);
        app.selected_node = Some("image".into());
        app.selected_nodes = vec!["image".into()];
        app.local_selection = Some(Selection { node_ids: vec!["image".into()], text_range: None });
        app.selection_generation = 4;
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

    fn start_import(&mut self, path: &str) -> Request {
        let mut intent = self.app.image_replacement_snapshot().expect("selected image is eligible");
        intent.token = designllm_contracts::new_id();
        self.app.start_canvas_image_replacement_import(intent, path.into()).expect("import starts");
        self.sent().into_iter().find(|request| request.1 == "asset.import").expect("asset import request")
    }
}

fn image_node(id: &str, sha256: String) -> Node {
    Node {
        id: id.into(),
        kind: NodeKind::Image,
        bounds: Rect { x: 123_000, y: 234_000, width: 1_200_000, height: 800_000 },
        text: String::new(),
        style: Style::default(),
        asset: Some(AssetRef { sha256, mime: "image/png".into(), bytes: 4096 }),
        children: Vec::new(),
        layout: None,
        interactions: Vec::new(),
        variants: Default::default(),
        ext: NodeExt { image_fit: Some(ImageFit::Contain), alt: Some("cover".into()), ..NodeExt::default() },
    }
}

fn imported_asset(sha256: &str, mime: &str) -> Value {
    json!({
        "sha256": sha256,
        "mime": mime,
        "bytes": 8192,
        "image": { "width_px": 640, "height_px": 480 }
    })
}

#[test]
fn bmp_picker_preflight_reads_standard_windows_header_dimensions() {
    let mut header = [0u8; 26];
    header[..2].copy_from_slice(b"BM");
    header[14..18].copy_from_slice(&40u32.to_le_bytes());
    header[18..22].copy_from_slice(&640i32.to_le_bytes());
    header[22..26].copy_from_slice(&(-480i32).to_le_bytes());
    assert_eq!(super::doc_tree::bmp_dimensions_from_header(&header), Some((640, 480)));
    assert_eq!(super::doc_tree::bmp_dimensions_from_header(&header[..10]), None);
}

#[test]
fn replacement_import_uses_the_captured_hash_and_preserves_image_properties() {
    let mut harness = Harness::new();
    let import = harness.start_import("replacement.png");
    let token = harness.app.pending_image_replacement.as_ref().unwrap().intent.token.clone();
    harness.answer(&import, Ok(imported_asset(&"b".repeat(64), "image/png")));

    let changes: Vec<_> = harness.sent().into_iter().filter(|request| request.1 == "changes.apply").collect();
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].2["base_revision"], 1);
    assert_eq!(changes[0].2["operations"], json!([{
        "op": "replace_image_asset",
        "node_id": "image",
        "expected_asset_sha256": "a".repeat(64),
        "asset_sha256": "b".repeat(64)
    }]));
    assert!(matches!(harness.app.pending.get(&changes[0].0).map(|pending| &pending.action), Some(PendingAction::ImageReplacementApply { token: pending_token }) if pending_token == &token));
    assert_eq!(harness.app.pending_image_replacement.as_ref().unwrap().stage, super::ImageReplacementStage::Applying);
    assert!(harness.app.image_property_change_pending("image"));
    harness.app.invoke_ui("props.image_fit", json!({ "fit": "cover" }));
    harness.app.invoke_ui("props.image_alt", json!({ "alt": "new description" }));
    assert!(harness.sent().iter().all(|request| request.1 != "changes.apply"), "fit/alt stay disabled while the replacement ChangeSet is in flight");

    let original = super::doc_tree::find_node(harness.app.artifact.as_ref().unwrap(), "image").unwrap();
    assert_eq!(original.asset.as_ref().unwrap().sha256, "a".repeat(64), "the GUI waits for the Core commit before changing its loaded model");
    assert_eq!(original.bounds, Rect { x: 123_000, y: 234_000, width: 1_200_000, height: 800_000 });
    assert_eq!(original.ext.image_fit, Some(ImageFit::Contain));
    assert_eq!(original.ext.alt.as_deref(), Some("cover"));
    harness.answer(&changes[0], Err(DltError::new(ErrorCode::RevisionConflict, "the image changed before apply")));
    assert!(harness.app.pending_image_replacement.is_none());
    assert_eq!(super::doc_tree::find_node(harness.app.artifact.as_ref().unwrap(), "image").unwrap().asset.as_ref().unwrap().sha256, "a".repeat(64));
}

#[test]
fn canceled_or_stale_import_reply_never_replaces_the_original_asset() {
    let mut canceled = Harness::new();
    let import = canceled.start_import("replacement.jpg");
    canceled.app.cancel_canvas_image_replacement();
    canceled.answer(&import, Ok(imported_asset(&"b".repeat(64), "image/jpeg")));
    assert!(canceled.sent().iter().all(|request| request.1 != "changes.apply"));
    assert_eq!(super::doc_tree::find_node(canceled.app.artifact.as_ref().unwrap(), "image").unwrap().asset.as_ref().unwrap().sha256, "a".repeat(64));

    let mut selection_changed = Harness::new();
    let import = selection_changed.start_import("replacement.png");
    selection_changed.app.set_selection(Selection { node_ids: vec!["other".into()], text_range: None });
    assert!(selection_changed.app.pending_image_replacement.is_none(), "a user selection transition cancels the import token");
    selection_changed.answer(&import, Ok(imported_asset(&"d".repeat(64), "image/png")));
    assert!(selection_changed.sent().iter().all(|request| request.1 != "changes.apply"));
    assert_eq!(super::doc_tree::find_node(selection_changed.app.artifact.as_ref().unwrap(), "image").unwrap().asset.as_ref().unwrap().sha256, "a".repeat(64));

    let mut locked = Harness::new();
    let import = locked.start_import("replacement.png");
    if let ArtifactBody::Canvas(document) = &mut locked.app.artifact.as_mut().unwrap().body {
        document.pages[0].nodes[0].ext.locked = Some(true);
    }
    locked.app.active.as_mut().unwrap().revision = 2;
    locked.app.artifact.as_mut().unwrap().revision = 2;
    locked.app.context.as_mut().unwrap().revision = 2;
    locked.answer(&import, Ok(imported_asset(&"e".repeat(64), "image/png")));
    assert!(locked.sent().iter().all(|request| request.1 != "changes.apply"));
    assert_eq!(super::doc_tree::find_node(locked.app.artifact.as_ref().unwrap(), "image").unwrap().asset.as_ref().unwrap().sha256, "a".repeat(64));

    let mut stale = Harness::new();
    let import = stale.start_import("replacement.jpg");
    stale.app.active.as_mut().unwrap().revision = 2;
    stale.app.artifact.as_mut().unwrap().revision = 2;
    stale.app.context.as_mut().unwrap().revision = 2;
    stale.answer(&import, Ok(imported_asset(&"c".repeat(64), "image/jpeg")));
    assert!(stale.sent().iter().all(|request| request.1 != "changes.apply"));
    assert!(stale.app.pending_image_replacement.is_none());
    assert_eq!(super::doc_tree::find_node(stale.app.artifact.as_ref().unwrap(), "image").unwrap().asset.as_ref().unwrap().sha256, "a".repeat(64));
}

#[test]
fn unsupported_linked_locked_or_component_images_are_rejected_before_import() {
    let mut unsupported = Harness::new();
    let intent = unsupported.app.image_replacement_snapshot().expect("target snapshot");
    assert!(unsupported.app.start_canvas_image_replacement_import(intent, "replacement.webp".into()).is_err());
    assert!(unsupported.sent().iter().all(|request| request.1 != "asset.import"));

    let mut locked = Harness::new();
    if let ArtifactBody::Canvas(document) = &mut locked.app.artifact.as_mut().unwrap().body {
        document.pages[0].nodes[0].ext.locked = Some(true);
    }
    assert!(locked.app.image_replacement_snapshot().is_err());

    let mut linked = Harness::new();
    linked.app.artifact.as_mut().unwrap().figure_links.push(FigureLink {
        figure_id: "figure".into(), figure_revision: 1, target_node_id: "image".into(),
        representation: "png".into(), render_hash: "c".repeat(64),
    });
    assert!(linked.app.image_replacement_snapshot().is_err());

    let mut source = Harness::new();
    if let ArtifactBody::Canvas(document) = &mut source.app.artifact.as_mut().unwrap().body {
        document.pages[0].role = Some(PageRole::Component);
    }
    assert!(source.app.image_replacement_snapshot().is_err());

    let mut instance = Harness::new();
    let ArtifactBody::Canvas(existing) = &instance.app.artifact.as_ref().unwrap().body else { panic!("Canvas body") };
    let image = existing.pages[0].nodes[0].clone();
    let ArtifactBody::Canvas(document) = &mut instance.app.artifact.as_mut().unwrap().body else { panic!("Canvas body") };
    document.pages[0].nodes = vec![Node {
        id: "instance".into(), kind: NodeKind::Group, bounds: image.bounds, text: String::new(), style: Style::default(),
        asset: None, children: vec![image], layout: None, interactions: Vec::new(), variants: Default::default(),
        ext: NodeExt { instance: Some(InstanceRef { component_page_id: "component".into(), overrides: Default::default() }), ..NodeExt::default() },
    }];
    assert!(instance.app.image_replacement_snapshot().is_err());
}

#[test]
fn fit_and_alt_mutations_are_serialized_with_an_image_replacement() {
    let mut replacing = Harness::new();
    let _import = replacing.start_import("replacement.png");
    let _ = replacing.sent();
    assert!(replacing.app.image_property_change_pending("image"));
    replacing.app.invoke_ui("props.image_fit", json!({ "fit": "cover" }));
    replacing.app.invoke_ui("props.image_alt", json!({ "alt": "new description" }));
    assert!(replacing.sent().iter().all(|request| request.1 != "changes.apply"), "property operations cannot race the replacement request");
    replacing.app.cancel_canvas_image_replacement();

    let mut fitting = Harness::new();
    fitting.app.apply_changes(
        vec![Operation::SetNodeAppearance {
            node_ids: vec!["image".into()], image_fit: Some(ImageFit::Cover), locked: None,
        }],
        [Intent::Design].into_iter().collect(),
        1,
        PendingAction::Apply,
    );
    let _ = fitting.sent();
    assert!(!fitting.app.canvas_image_replacement_available("image"));
    assert!(fitting.app.image_replacement_snapshot().is_err(), "an in-flight fit edit blocks capturing a competing replacement base");

    let mut geometry = Harness::new();
    geometry.app.apply_changes(
        vec![Operation::SetGeometry {
            node_id: "other".into(), bounds: Rect { x: 10, y: 20, width: 400_000, height: 250_000 },
        }],
        [Intent::Design].into_iter().collect(),
        1,
        PendingAction::Apply,
    );
    let _ = geometry.sent();
    assert!(geometry.app.image_replacement_snapshot().is_err(), "any in-flight same-artifact ChangeSet blocks a competing replacement base");
}

#[test]
fn direct_artifact_writers_cancel_import_or_wait_for_replacement_apply() {
    let mut importing = Harness::new();
    let _import = importing.start_import("replacement.png");
    let ticket = importing.app.call(
        "design_system.publish",
        json!({ "artifact_id": "a1" }),
        PendingAction::DesignSystemChange { refresh_systems: true },
    );
    assert!(ticket.is_some(), "an Importing request is canceled before another same-artifact mutation proceeds");
    assert!(importing.app.pending_image_replacement.is_none());
    assert!(importing.app.notices.current(std::time::Instant::now()).is_some_and(|notice| notice.text.contains("교체 요청을 취소했습니다")));
    assert!(importing.sent().iter().any(|request| request.1 == "design_system.publish"));

    let mut applying = Harness::new();
    let import = applying.start_import("replacement.png");
    applying.answer(&import, Ok(imported_asset(&"b".repeat(64), "image/png")));
    let _ = applying.sent();
    assert_eq!(applying.app.pending_image_replacement.as_ref().unwrap().stage, super::ImageReplacementStage::Applying);
    let ticket = applying.app.call(
        "design_system.set",
        json!({ "artifact_id": "a1" }),
        PendingAction::DesignSystemChange { refresh_systems: false },
    );
    assert!(ticket.is_none(), "Applying blocks direct same-artifact writers before RPC");
    assert!(applying.sent().iter().all(|request| request.1 != "design_system.set"));
    assert!(applying.app.pending_image_replacement.is_some());

    let mut ui_run = Harness::new();
    let import = ui_run.start_import("replacement.png");
    ui_run.answer(&import, Ok(imported_asset(&"f".repeat(64), "image/png")));
    let _ = ui_run.sent();
    ui_run.app.run_context = Some(super::ui_channel::RunContext {
        run_id: "run-1".into(),
        deadline: std::time::Instant::now(),
        command_id: "canvas.design_system.publish".into(),
        actor: "desktop".into(),
        allowed_intents: [Intent::Design].into_iter().collect(),
        await_rpc_reply: true,
        mutation_attempted: false,
        rpc_ticket: None,
        enqueue_error: None,
        expected: None,
        prepared_asset: None,
    });
    assert!(ui_run.app.call(
        "design_system.publish",
        json!({ "artifact_id": "a1" }),
        PendingAction::DesignSystemChange { refresh_systems: true },
    ).is_none());
    let run = ui_run.app.run_context.as_ref().unwrap();
    assert!(run.mutation_attempted);
    assert_eq!(run.enqueue_error.as_ref().map(|error| &error.code), Some(&ErrorCode::TargetBusy));
    assert!(ui_run.sent().iter().all(|request| request.1 != "design_system.publish"), "a blocked UI-run writer is reported as refused, not accepted");
    ui_run.app.run_context.as_mut().unwrap().mutation_attempted = false;
    ui_run.app.run_context.as_mut().unwrap().enqueue_error = None;
    assert!(ui_run.app.call("history.undo", json!({ "artifact_id": "a1" }), PendingAction::History).is_none());
    let run = ui_run.app.run_context.as_ref().unwrap();
    assert!(run.mutation_attempted);
    assert_eq!(run.enqueue_error.as_ref().map(|error| &error.code), Some(&ErrorCode::TargetBusy));
    assert!(ui_run.sent().iter().all(|request| request.1 != "history.undo"), "blocked undo is an explicit run refusal");
}

#[test]
fn applying_replacement_preserves_other_node_mutations_and_inline_text_queue() {
    let mut importing = Harness::new();
    let _import = importing.start_import("replacement.png");
    let _ = importing.sent();
    importing.app.apply_changes(
        vec![Operation::SetGeometry {
            node_id: "other".into(), bounds: Rect { x: 100, y: 200, width: 300_000, height: 200_000 },
        }],
        [Intent::Design].into_iter().collect(),
        1,
        PendingAction::Apply,
    );
    assert!(importing.app.pending_image_replacement.is_none(), "a new same-artifact ChangeSet cancels the in-flight import token");
    assert!(importing.sent().iter().any(|request| request.1 == "changes.apply"), "the new mutation proceeds after canceling Importing");
    assert!(importing.app.notices.current(std::time::Instant::now()).is_some_and(|notice| notice.text.contains("교체 요청을 취소했습니다")));

    let mut geometry = Harness::new();
    let import = geometry.start_import("replacement.png");
    geometry.answer(&import, Ok(imported_asset(&"b".repeat(64), "image/png")));
    let _replacement_apply = geometry.sent().into_iter().find(|request| request.1 == "changes.apply").expect("replacement apply");
    geometry.app.apply_changes(
        vec![Operation::SetGeometry {
            node_id: "other".into(), bounds: Rect { x: 100, y: 200, width: 300_000, height: 200_000 },
        }],
        [Intent::Design].into_iter().collect(),
        1,
        PendingAction::Apply,
    );
    assert!(geometry.sent().iter().all(|request| request.1 != "changes.apply"), "a concurrent same-artifact mutation is not sent");
    let held = geometry.app.held_edits.last().expect("the pending user operation is retained");
    assert_eq!(held.apply.operations.len(), 1);
    assert!(matches!(&held.apply.operations[0], Operation::SetGeometry { node_id, .. } if node_id == "other"));
    assert!(!held.can_retry, "the held operation cannot rebase onto a revision that may change under the replacement");

    let mut text = Harness::new();
    let import = text.start_import("replacement.png");
    text.answer(&import, Ok(imported_asset(&"b".repeat(64), "image/png")));
    let _replacement_apply = text.sent().into_iter().find(|request| request.1 == "changes.apply").expect("replacement apply");
    text.app.set_selected_ids(vec!["text-1".into()]);
    assert!(text.app.begin_inline_text_edit(&text.ctx, "text-1", 0, None));
    let _ = text.sent();
    let edit = crate::grapheme::TextInputEdit {
        request_id: designllm_contracts::new_id(), range: 1..1, paragraph_index: 0, paragraph_range: Some(1..1),
        caret_after: 2, paragraph_after: 0, caret_after_in_paragraph: 2, text: "x".into(),
        ime_commit: false, ime_generation: 0, after_text: None,
    };
    text.app.text_draft = "Ax".into();
    text.app.inline_text_edit.as_mut().unwrap().queue.push_back(edit);
    text.app.start_next_inline_text_change();
    let session = text.app.inline_text_edit.as_ref().unwrap();
    assert!(session.in_flight.is_none());
    assert_eq!(session.queue.len(), 1);
    assert_eq!(text.app.text_draft, "Ax");
    assert!(text.sent().iter().all(|request| request.1 != "changes.apply"), "direct inline typing stays queued until the replacement finishes");
}

#[test]
fn refused_drag_reservation_clears_only_the_blocked_drag() {
    let mut blocked = Harness::new();
    let import = blocked.start_import("replacement.png");
    blocked.answer(&import, Ok(imported_asset(&"b".repeat(64), "image/png")));
    let _ = blocked.sent();
    blocked.app.start_drag(
        "image".into(), 0,
        Rect { x: 123_000, y: 234_000, width: 1_200_000, height: 800_000 },
        Size::slide(),
        ScreenRect::from_min_size(Pos2::ZERO, Vec2::new(800.0, 600.0)),
        Pos2::new(200.0, 200.0),
    );
    assert!(blocked.app.drag.is_none(), "a reservation refused during image replacement leaves no stuck DragState");
    assert!(blocked.sent().iter().all(|request| request.1 != "targets.reserve"));
    assert!(blocked.app.notices.current(std::time::Instant::now()).is_some_and(|notice| notice.text.contains("드래그 대상 예약")));

    let mut normal = Harness::new();
    normal.app.start_drag(
        "other".into(), 0,
        Rect { x: 0, y: 0, width: 300_000, height: 200_000 },
        Size::slide(),
        ScreenRect::from_min_size(Pos2::ZERO, Vec2::new(800.0, 600.0)),
        Pos2::new(100.0, 100.0),
    );
    assert!(normal.app.drag.is_some(), "ordinary drag retains its existing reservation path");
    assert!(normal.sent().iter().any(|request| request.1 == "targets.reserve"));
}
