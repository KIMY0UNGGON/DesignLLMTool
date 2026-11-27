//! D19 video insertion refuses mismatched imports before committing either reference.

use std::sync::mpsc::{Receiver, Sender};

use designllm_contracts::{
    sha256, Artifact, ArtifactBody, ArtifactKind, ArtifactSummary, AssetRef, CanvasDocument, DltError,
    Node, NodeExt, NodeKind, Page, Rect, Selection, Size, Style,
};
use eframe::egui::Context;
use serde_json::{json, Value};

use super::{
    state::VideoInsertIntent, DesktopApp, PendingAction, Screen, VideoInsertStage,
    PendingVideoInsert,
};
use crate::rpc::{RpcPayload, RpcReply, RpcWorker, Work};

type Request = (u64, String, Value);

struct TempFileCleanup(std::path::PathBuf);
impl Drop for TempFileCleanup {
    fn drop(&mut self) { let _ = std::fs::remove_file(&self.0); }
}

struct Harness {
    app: DesktopApp,
    replies: Sender<RpcReply>,
    work: Receiver<Work>,
    ctx: Context,
}

impl Harness {
    fn new() -> Self {
        let (rpc, replies, work) = RpcWorker::detached();
        let mut app = DesktopApp::new(rpc, "video insert fixture".into(), None, None);
        while work.try_recv().is_ok() {}
        let page = Page::new("page-1", Size::slide());
        let mut artifact = Artifact::new(
            ArtifactKind::Canvas,
            "video insert fixture",
            "canvas.v1",
            ArtifactBody::Canvas(CanvasDocument { pages: vec![page], ..Default::default() }),
        );
        artifact.id = "a1".into();
        artifact.revision = 1;
        app.screen = Screen::Editor;
        app.active = Some(ArtifactSummary {
            id: artifact.id.clone(),
            kind: artifact.kind,
            title: artifact.title.clone(),
            revision: 1,
            engine_id: artifact.engine_id.clone(),
            dirty: false,
            modified_at_ms: None,
            active_jobs: Vec::new(),
        });
        app.context = Some(serde_json::from_value(json!({
            "workspace_id": "w1", "artifact_id": "a1", "revision": 1, "view_id": app.view_id.clone(),
            "selection": { "node_ids": [], "text_range": null }, "visible_page": 0,
            "selected_properties": { "nodes": null, "stale_selection": null, "render_unavailable": null },
            "capabilities": {
                "images": { "read": "supported", "render": "supported", "edit": "supported", "preserve": "supported", "write": "supported", "reason": "" }
            },
            "render_snapshot": null, "view_transform": null
        })).expect("context"));
        app.artifact = Some(artifact);
        app.local_selection = Some(Selection::default());
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
        self.replies.send(RpcReply {
            ticket: request.0,
            result: result.map(RpcPayload::Json),
        }).expect("app is listening");
        self.app.process_replies(&self.ctx);
    }

    fn begin_source_import(&mut self) -> Request {
        let intent = VideoInsertIntent {
            token: "video-token".into(),
            artifact_id: "a1".into(),
            expected_revision: 1,
            page_index: 0,
            page_id: "page-1".into(),
            center: None,
            view_id: self.app.view_id.clone(),
            expected_selection: Selection::default(),
            selection_generation: 0,
        };
        self.app.video_insert = Some(PendingVideoInsert {
            intent,
            source_path: "C:\\fixture\\movie.mp4".into(),
            stage: VideoInsertStage::ImportingSource,
            source_info: Some(designllm_web_preview::VideoSourceInfo {
                mime: "video/mp4".into(),
                bytes: 100,
                sha256: "a".repeat(64),
            }),
            source_asset: None,
            poster: Some(designllm_web_preview::VideoPoster { png: Vec::new(), width: 1, height: 1 }),
            poster_asset: None,
            poster_temp_path: Some(std::env::temp_dir().join("designllm-video-insert-mismatch-no-file.png")),
            worker_cancel: None,
            cancel_requested: false,
            node_id: "video-node".into(),
            apply: None,
            resolution_retry_sent: false,
            message: None,
        });
        let ticket = self.app.call(
            "asset.import",
            json!({ "path": "C:\\fixture\\movie.mp4" }),
            PendingAction::VideoSourceImport { token: "video-token".into() },
        ).expect("source import call");
        self.sent().into_iter().find(|request| request.0 == ticket).expect("source import request")
    }
}

#[test]
fn source_import_must_match_preflight_hash_mime_and_bytes_before_poster_import() {
    for imported in [
        AssetRef { sha256: "b".repeat(64), mime: "video/mp4".into(), bytes: 100 },
        AssetRef { sha256: "a".repeat(64), mime: "video/webm".into(), bytes: 100 },
        AssetRef { sha256: "a".repeat(64), mime: "video/mp4".into(), bytes: 101 },
    ] {
        let mut harness = Harness::new();
        let request = harness.begin_source_import();
        harness.answer(&request, Ok(serde_json::to_value(imported).expect("asset ref")));

        assert!(harness.app.video_insert.is_none(), "a changed source never reaches poster import or AddNode");
        assert!(harness.sent().iter().all(|(_, method, _)| method != "asset.import" && method != "changes.apply"));
        assert!(harness.app.notices.current(std::time::Instant::now()).is_some_and(|notice| {
            notice.text.contains("파일이 바뀌었습니다")
        }));
    }
}

#[test]
fn successful_preflight_and_poster_import_queue_one_video_add_node() {
    let mut harness = Harness::new();
    let source_info = designllm_web_preview::VideoSourceInfo {
        mime: "video/mp4".into(),
        bytes: 100,
        sha256: "a".repeat(64),
    };
    harness.app.video_insert = Some(PendingVideoInsert {
        intent: VideoInsertIntent {
            token: "video-success".into(),
            artifact_id: "a1".into(),
            expected_revision: 1,
            page_index: 0,
            page_id: "page-1".into(),
            center: None,
            view_id: harness.app.view_id.clone(),
            expected_selection: Selection::default(),
            selection_generation: 0,
        },
        source_path: "C:/fixture/training-clip.mp4".into(),
        stage: VideoInsertStage::ExtractingPoster,
        source_info: Some(source_info.clone()),
        source_asset: None,
        poster: None,
        poster_asset: None,
        poster_temp_path: None,
        worker_cancel: None,
        cancel_requested: false,
        node_id: "video-node".into(),
        apply: None,
        resolution_retry_sent: false,
        message: None,
    });

    let png = designllm_render::png_from_rgba(2, 2, &[255; 16]).expect("valid poster PNG");
    harness.app.video_worker_tx.send(super::VideoPosterWorkerResult::Extracted {
        token: "video-success".into(),
        result: Ok(designllm_web_preview::VideoPoster { png: png.clone(), width: 2, height: 2 }),
    }).expect("worker result receiver");
    harness.app.poll_video_poster_workers();
    let source_imports = harness.sent();
    assert_eq!(source_imports.len(), 1);
    let source_import = &source_imports[0];
    assert_eq!(source_import.1, "asset.import");
    assert_eq!(source_import.2["path"], "C:/fixture/training-clip.mp4");
    let poster_path = harness.app.video_insert.as_ref().unwrap().poster_temp_path.clone().expect("temporary poster");
    let _cleanup = TempFileCleanup(poster_path.clone());
    assert_eq!(std::fs::read(&poster_path).expect("poster temp bytes"), png);

    let source_asset = AssetRef { sha256: source_info.sha256.clone(), mime: source_info.mime.clone(), bytes: source_info.bytes };
    harness.answer(source_import, Ok(serde_json::to_value(source_asset.clone()).unwrap()));
    let poster_imports = harness.sent();
    assert_eq!(poster_imports.len(), 1);
    let poster_import = &poster_imports[0];
    assert_eq!(poster_import.1, "asset.import");
    assert_eq!(poster_import.2["path"], poster_path.to_string_lossy().as_ref());

    let poster_asset = AssetRef { sha256: sha256(&png), mime: "image/png".into(), bytes: png.len() as u64 };
    harness.answer(poster_import, Ok(serde_json::to_value(poster_asset.clone()).unwrap()));
    let applies = harness.sent();
    assert_eq!(applies.len(), 1, "the successful pipeline queues one change request");
    let apply = &applies[0];
    assert_eq!(apply.1, "changes.apply");
    assert_eq!(apply.2["artifact_id"], "a1");
    assert_eq!(apply.2["base_revision"], 1);
    assert_eq!(apply.2["operations"].as_array().unwrap().len(), 1);
    let operation = &apply.2["operations"][0];
    assert_eq!(operation["op"], "add_node");
    assert_eq!(operation["page_id"], "page-1");
    assert_eq!(operation["node"]["id"], "video-node");
    assert_eq!(operation["node"]["kind"], "video");
    assert_eq!(operation["node"]["asset"], serde_json::to_value(source_asset).unwrap());
    assert_eq!(operation["node"]["video_poster_asset"], serde_json::to_value(poster_asset).unwrap());
    assert_eq!(operation["node"]["alt"], "training-clip");
    assert_eq!(harness.app.video_insert.as_ref().unwrap().stage, VideoInsertStage::Applying);
}

#[test]
fn projected_canvas_asset_size_deduplicates_shared_hashes_and_enforces_native_limit() {
    let existing = AssetRef {
        sha256: "a".repeat(64),
        mime: "video/mp4".into(),
        bytes: 128 * 1024 * 1024 - 50,
    };
    let node = Node {
        id: "existing-video".into(),
        kind: NodeKind::Video,
        bounds: Rect { x: 0, y: 0, width: 10, height: 10 },
        text: String::new(),
        style: Style::default(),
        asset: Some(existing.clone()),
        children: Vec::new(),
        layout: None,
        interactions: Vec::new(),
        variants: Default::default(),
        ext: NodeExt {
            video_poster_asset: Some(AssetRef {
                sha256: "p".repeat(64),
                mime: "image/png".into(),
                bytes: 10,
            }),
            image_fit: None,
            ..NodeExt::default()
        },
    };
    let document = CanvasDocument {
        pages: vec![Page { nodes: vec![node], ..Page::new("page", Size::slide()) }],
        ..Default::default()
    };
    let same_source = AssetRef {
        sha256: existing.sha256.clone(), mime: existing.mime.clone(), bytes: existing.bytes,
    };
    let poster_under_limit = AssetRef { sha256: "q".repeat(64), mime: "image/png".into(), bytes: 40 };
    assert_eq!(
        super::video_insert::projected_canvas_asset_bytes(&document, [&same_source, &poster_under_limit]),
        Ok(128 * 1024 * 1024),
        "an already-referenced source hash is counted once",
    );
    let poster_over_limit = AssetRef { bytes: 41, ..poster_under_limit };
    assert!(super::video_insert::projected_canvas_asset_bytes(&document, [&same_source, &poster_over_limit]).is_err());
}

#[test]
fn video_alt_uses_a_safe_filename_default_and_existing_command_edits_it() {
    assert_eq!(super::video_insert::video_alt_from_path(std::path::Path::new("training-clip.mp4")), "training-clip");
    assert_eq!(super::video_insert::video_alt_from_path(std::path::Path::new("   .webm")), "동영상");

    let mut harness = Harness::new();
    let video = Node {
        id: "video-node".into(),
        kind: NodeKind::Video,
        bounds: Rect { x: 0, y: 0, width: 3_048_000, height: 1_714_500 },
        text: String::new(),
        style: Style::default(),
        asset: Some(AssetRef { sha256: "a".repeat(64), mime: "video/mp4".into(), bytes: 100 }),
        children: Vec::new(),
        layout: None,
        interactions: Vec::new(),
        variants: Default::default(),
        ext: NodeExt {
            alt: Some("training-clip".into()),
            video_poster_asset: Some(AssetRef { sha256: "b".repeat(64), mime: "image/png".into(), bytes: 20 }),
            ..NodeExt::default()
        },
    };
    let ArtifactBody::Canvas(document) = &mut harness.app.artifact.as_mut().expect("artifact").body else {
        panic!("Canvas artifact");
    };
    document.pages[0].nodes.push(video);
    let selection = Selection { node_ids: vec!["video-node".into()], text_range: None };
    harness.app.selected_node = Some("video-node".into());
    harness.app.selected_nodes = vec!["video-node".into()];
    harness.app.local_selection = Some(selection.clone());
    harness.app.context.as_mut().expect("context").selection = selection;

    harness.app.invoke("props.image_alt", json!({ "alt": "교육 영상" })).expect("video alt command is available");
    let request = harness.sent().into_iter().find(|request| request.1 == "changes.apply").expect("SetNodeInfo request");
    assert_eq!(request.2["operations"][0]["op"], "set_node_info");
    assert_eq!(request.2["operations"][0]["node_ids"], json!(["video-node"]));
    assert_eq!(request.2["operations"][0]["alt"], "교육 영상");
}

#[test]
fn canceled_extractor_remains_close_and_artifact_switch_guarded_until_worker_returns() {
    let mut harness = Harness::new();
    let cancellation = designllm_web_preview::VideoPosterCancellation::default();
    harness.app.video_insert = Some(PendingVideoInsert {
        intent: VideoInsertIntent {
            token: "worker-token".into(),
            artifact_id: "a1".into(),
            expected_revision: 1,
            page_index: 0,
            page_id: "page-1".into(),
            center: None,
            view_id: harness.app.view_id.clone(),
            expected_selection: Selection::default(),
            selection_generation: 0,
        },
        source_path: "C:\\fixture\\movie.mp4".into(),
        stage: VideoInsertStage::ExtractingPoster,
        source_info: None,
        source_asset: None,
        poster: None,
        poster_asset: None,
        poster_temp_path: None,
        worker_cancel: Some(cancellation.clone()),
        cancel_requested: false,
        node_id: "video-node".into(),
        apply: None,
        resolution_retry_sent: false,
        message: None,
    });
    assert!(harness.app.video_insert.as_ref().unwrap().close_wait_required());
    harness.app.cancel_canvas_video_insert().expect("cancel request is accepted");
    assert!(cancellation.is_cancelled());
    assert!(harness.app.video_insert.is_some(), "state stays until the worker reports cleanup");
    assert!(harness.app.video_insert.as_ref().unwrap().close_wait_required());

    let other = ArtifactSummary {
        id: "a2".into(), kind: ArtifactKind::Canvas, title: "other".into(), revision: 1,
        engine_id: "canvas.v1".into(), dirty: false, modified_at_ms: None, active_jobs: Vec::new(),
    };
    harness.app.activate(other);
    assert_eq!(harness.app.active.as_ref().map(|active| active.id.as_str()), Some("a1"));
    assert!(harness.app.video_insert.is_some(), "artifact switch waits for the extractor's host/profile cleanup");
}

/// D19.5: a poster PNG that a killed window left is removed at the next start; this window's poster, and one whose window still
/// runs, stay.
#[test]
fn a_start_removes_a_poster_left_by_a_killed_window_and_keeps_running_windows_posters() {
    use std::process::{Command, Stdio};
    // The cleanup looks only at the system temporary folder itself, where the posters are written (under cargo-log.ps1 the
    // lane's own TEMP). Every file here has a fresh UUID in its name and is removed at the end.
    let directory = std::env::temp_dir();
    let quiet = |command: &mut Command| {
        // CREATE_NO_WINDOW: the helper processes open no console window.
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x0800_0000);
        }
        command.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap()
    };
    #[cfg(windows)]
    let (mut running, mut killed) =
        (quiet(Command::new("ping").args(["-n", "60", "127.0.0.1"])), quiet(Command::new("cmd").args(["/C", "exit 0"])));
    #[cfg(not(windows))]
    let (mut running, mut killed) = (quiet(Command::new("sleep").arg("60")), quiet(&mut Command::new("true")));
    let killed_pid = killed.id();
    killed.wait().unwrap();

    let own = super::video_insert::write_video_poster_temp(b"png").expect("this window's poster");
    let name = own.file_name().unwrap().to_string_lossy().into_owned();
    assert!(name.starts_with(&format!("designllm-video-poster-p{}-", std::process::id())), "{name}");
    let left = directory.join(format!("designllm-video-poster-p{killed_pid}-{}.png", designllm_contracts::new_id()));
    let older = directory.join(format!("designllm-video-poster-{}.png", designllm_contracts::new_id()));
    let other_window = directory.join(format!("designllm-video-poster-p{}-{}.png", running.id(), designllm_contracts::new_id()));
    for path in [&left, &older, &other_window] {
        std::fs::write(path, b"png").unwrap();
    }

    let removed = super::video_insert::remove_orphaned_video_posters();
    let _ = running.kill();
    let _ = running.wait();
    assert!(!left.exists() && !older.exists(), "the killed window's poster and the untagged one of an older build go");
    assert!(removed >= 2, "{removed}");
    assert!(own.exists(), "this window's poster stays");
    assert!(other_window.exists(), "a running window's poster stays");
    std::fs::remove_file(own).unwrap();
    std::fs::remove_file(other_window).unwrap();
}
