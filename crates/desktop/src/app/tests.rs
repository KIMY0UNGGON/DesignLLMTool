//! The window's behaviour against a host that is played by the test: the test reads the requests
//! the window sends and answers them by hand, then looks at what the window shows.

use std::time::Instant;

use designllm_contracts::{Artifact, ArtifactBody, ArtifactKind, ArtifactSummary, CanvasDocument, Diagnostic, DltError, ErrorCode, FigureRevisionRef, InstanceRef, Node, NodeExt, NodeKind, Page, PageRole, Size, Style};
use eframe::egui::{Event, Modifiers, PointerButton, Pos2, RawInput, Rect, Vec2};
use serde_json::{json, Value};

use crate::{
    notice::Severity,
    rpc::{RpcPayload, RpcReply, RpcWorker, Work},
    theme,
};

pub(super) use super::test_support::{drawn_text, screen};
use super::test_support::*;

#[test]
fn nothing_is_shown_until_something_fails() {
    let mut harness = Harness::new();
    assert!(harness.message().is_none());
    harness.open_document("canvas", 1);
    assert!(harness.message().is_none(), "opening a document is not a failure");
}

#[test]
fn a_document_opens_by_creating_its_view_before_anything_reads_it() {
    let mut harness = Harness::new();
    let (opening, view_id) = harness.open_document("canvas", 1);
    assert_eq!(
        methods(&opening),
        ["targets.list", "design_themes.search", "design_system.list", "view.transform.set", "context.read", "artifact.inspect", "render.get"],
        "a read of a view the host does not know yet is NOT_FOUND"
    );
    assert_eq!(opening[3].2["view_id"], view_id);
    assert_eq!(opening[3].2["artifact_id"], "a1");
    assert_eq!(opening[6].2["expected_revision"], 1);
}

#[test]
fn web_code_has_no_page_scene_and_never_asks_for_one() {
    let mut harness = Harness::new();
    let (opening, _) = harness.open_document("web_code", 1);
    assert_eq!(methods(&opening), ["targets.list", "view.transform.set", "context.read", "artifact.inspect"]);
    assert!(harness.message().is_none());
}

#[test]
fn component_authoring_commands_are_enabled_only_for_supported_canvas_contexts() {
    let mut harness = Harness::new();
    harness.app.screen = super::Screen::Editor;
    harness.app.active = Some(serde_json::from_value(summary("canvas", "canvas", 1)).unwrap());
    let mut canvas_context = context("canvas", "view", 1);
    canvas_context["capability_extensions"] = json!({ "components": {
        "read": "supported", "render": "supported", "edit": "supported", "preserve": "supported", "write": "supported", "reason": "local only"
    }});
    harness.app.context = Some(serde_json::from_value(canvas_context.clone()).unwrap());
    for command in ["page.component", "component.instantiate", "component.detach"] {
        assert!(harness.app.command_enabled_for_current_kind(command), "{command} is enabled for supported Canvas");
    }

    harness.app.active = Some(serde_json::from_value(summary("slides", "slides", 1)).unwrap());
    assert!(!harness.app.command_enabled_for_current_kind("page.component"));
    assert!(!harness.app.command_enabled_for_current_kind("component.instantiate"));
    assert!(!harness.app.command_enabled_for_current_kind("component.detach"));

    harness.app.active = Some(serde_json::from_value(summary("canvas", "canvas", 1)).unwrap());
    canvas_context["capability_extensions"]["components"]["edit"] = json!("unsupported");
    harness.app.context = Some(serde_json::from_value(canvas_context).unwrap());
    assert!(!harness.app.command_enabled_for_current_kind("page.component"));
}

#[test]
fn home_canvas_thumbnail_inspects_page_roles_and_renders_first_regular_page() {
    let mut harness = Harness::new();
    harness.sent(); // startup calls are unrelated to the explicitly painted home thumbnail
    let summary: designllm_contracts::ArtifactSummary = serde_json::from_value(summary("canvas-with-components", "canvas", 7)).unwrap();
    let rect = Rect::from_min_size(Pos2::ZERO, Vec2::new(100.0, 120.0));
    let context = harness.ctx.clone();
    let _ = context.run(RawInput::default(), |ctx| {
        eframe::egui::CentralPanel::default().show(ctx, |ui| {
            harness.app.paint_thumb(
                ui, super::thumbs::ThumbSource::Artifact(&summary), rect,
                super::thumbs::Fit::Contain { margin: 0.0 }, eframe::egui::CornerRadius::ZERO,
            );
        });
    });
    let inspect = harness.sent();
    assert_eq!(methods(&inspect), ["artifact.inspect"]);
    assert_eq!(inspect[0].2["artifact_id"], summary.id);
    assert_eq!(inspect[0].2["revision"], 7);
    let ticket = inspect[0].0;
    harness.replies.send(RpcReply {
        ticket,
        result: Ok(RpcPayload::Json(json!({
            "revision": 7,
            "body": { "type": "canvas", "document": { "pages": [
                { "id": "component-first", "role": "component" },
                { "id": "regular-second", "role": null }
            ] } }
        }))),
    }).unwrap();
    harness.app.process_replies(&harness.ctx);
    let render = harness.sent();
    assert_eq!(methods(&render), ["render.get"]);
    assert_eq!(render[0].2["page_index"], 1);
}

#[test]
fn a_failed_action_shows_why_and_the_next_success_clears_it() {
    let mut harness = Harness::new();
    harness.open_document("canvas", 1);
    harness.sent();

    let (operations, intents) = edit();
    harness.app.apply_operations(operations, intents);
    let request = harness.sent();
    assert_eq!(methods(&request), ["changes.apply"]);
    harness.answer(
        &request,
        "changes.apply",
        Err(DltError::new(ErrorCode::ContentLocked, "protected")),
    );
    assert_eq!(
        harness.message(),
        Some((Severity::Error, "보호된 내용은 수정할 수 없습니다".to_owned()))
    );

    // The user tries again: the old message goes away at once, and stays away on success.
    let (operations, intents) = edit();
    harness.app.apply_operations(operations, intents);
    assert!(harness.message().is_none(), "a new action starts with a clean slate");
    let request = harness.sent();
    harness.answer(&request, "changes.apply", Ok(json!({ "revision": 2 })));
    assert!(harness.message().is_none());
    assert_eq!(harness.app.active.as_ref().map(|active| active.revision), Some(2));
    assert_eq!(methods(&harness.sent()), reload_methods());
}

#[test]
fn a_load_that_failed_is_reported_until_the_same_load_succeeds() {
    let mut harness = Harness::new();
    let (opening, view_id) = harness.open_document("canvas", 1);
    harness.answer(&opening, "render.get", Err(DltError::new(ErrorCode::Io, "disk")));
    let shown = harness.message().expect("a preview that cannot load is a real failure");
    assert_eq!(shown.0, Severity::Error);

    // Other calls succeeding says nothing about the preview.
    harness.app.refresh_active();
    let reload = harness.sent();
    assert_eq!(methods(&reload), reload_methods());
    harness.answer(&reload, "context.read", Ok(context("a1", &view_id, 1)));
    harness.answer(
        &reload,
        "workspace.list",
        Ok(json!({ "workspace_id": "w1", "artifacts": [summary("a1", "canvas", 1)] })),
    );
    assert!(harness.message().is_some(), "the preview is still missing");

    // The preview loading again removes the message.
    harness.answer(&reload, "render.get", Ok(snapshot("a1", 1)));
    assert!(harness.message().is_none());
}

#[test]
fn a_preview_that_is_stale_reloads_quietly_and_a_loop_is_not_hidden() {
    let mut harness = Harness::new();
    let (opening, _) = harness.open_document("canvas", 1);
    harness.sent();

    harness.answer(&opening, "render.get", Err(DltError::new(ErrorCode::StaleRender, "old")));
    assert!(harness.message().is_none(), "the host moved on; nobody did anything wrong");
    let reload = harness.sent();
    assert_eq!(methods(&reload), reload_methods());

    harness.answer(&reload, "render.get", Err(DltError::new(ErrorCode::StaleRender, "old")));
    assert!(harness.message().is_some(), "a second stale answer within a second is shown, not retried forever");
    assert!(harness.sent().is_empty());
}

#[test]
fn another_editors_commit_reloads_but_our_own_does_not() {
    let mut harness = Harness::new();
    harness.open_document("canvas", 1);
    harness.sent();
    let event = |seq: u64, revision: u64, artifact: &str| {
        json!({ "events": [{ "protocol_version": 1, "event_seq": seq, "artifact_id": artifact, "revision": revision, "event": "artifact.changed", "detail": {} }], "last_seq": seq, "resync_required": false, "artifacts": [] })
    };

    harness.app.handle_events(event(1, 1, "a1"));
    assert_eq!(methods(&harness.sent()), ["workspace.list"], "the revision on screen is already current");

    harness.app.handle_events(event(2, 2, "other"));
    assert_eq!(methods(&harness.sent()), ["workspace.list"], "another document only changes the list");

    harness.app.handle_events(event(3, 2, "a1"));
    assert_eq!(methods(&harness.sent()), reload_methods(), "an agent moved the open document on");

    harness.app.handle_events(event(3, 9, "a1"));
    assert!(harness.sent().is_empty(), "an event that was already seen is ignored");
}

#[test]
fn undo_adopts_the_revision_the_host_returns_before_reloading() {
    let mut harness = Harness::new();
    harness.open_document("canvas", 5);
    harness.sent();
    harness.app.history(false).expect("history undo is enqueued");
    let request = harness.sent();
    assert_eq!(methods(&request), ["history.undo"]);
    harness.answer(&request, "history.undo", Ok(json!({ "revision": 6 })));
    let reload = harness.sent();
    assert_eq!(methods(&reload), reload_methods());
    assert_eq!(reload[3].2["expected_revision"], 6, "the preview is requested for the new revision");
}

#[test]
fn opening_another_document_removes_the_old_message() {
    let mut harness = Harness::new();
    harness.open_document("canvas", 1);
    harness.app.set_error("입력 내용을 확인해 주세요".into());
    assert!(harness.message().is_some());
    let other = serde_json::from_value(summary("a2", "canvas", 1)).expect("summary");
    harness.app.activate(other);
    assert!(harness.message().is_none());
}

#[test]
fn figure_refusals_are_explained_by_their_diagnostic() {
    let mut error = DltError::new(ErrorCode::UnsupportedOperation, "blocking");
    error.diagnostics = vec![Diagnostic {
        code: "LABEL_OVERFLOW".into(),
        message: "label does not fit".into(),
        node_id: None,
        blocking: true,
        params: Default::default(),
    }];
    assert_eq!(
        super::user_error(&error),
        "글자가 도형 안에 들어가지 않습니다. 도형을 키우거나 글자를 줄여 주세요"
    );

    // Diagnostics that do not block, or that have no sentence, leave the general message.
    error.diagnostics[0].blocking = false;
    assert_eq!(super::user_error(&error), "이 문서에서는 이 작업을 지원하지 않습니다");
    error.diagnostics[0].blocking = true;
    error.diagnostics[0].code = "SOMETHING_NEW".into();
    assert_eq!(super::user_error(&error), "이 문서에서는 이 작업을 지원하지 않습니다");

    for (code, message) in [
        ("docx.digital_signature_unsupported", "전자 서명된 문서는 지원하지 않아 처리하지 않았습니다"),
        ("pptx.digital_signature_unsupported", "전자 서명된 문서는 지원하지 않아 처리하지 않았습니다"),
        (
            "docx.compound_container_unsupported",
            "암호화된 문서이거나 지원하지 않는 이전 Office 형식이라 열 수 없습니다",
        ),
        (
            "pptx.compound_container_unsupported",
            "암호화된 문서이거나 지원하지 않는 이전 Office 형식이라 열 수 없습니다",
        ),
        ("hwp.protected_document_unsupported", "암호나 배포·DRM 보호가 걸린 HWP 문서는 열 수 없습니다"),
        ("hwp.digital_signature_unsupported", "전자 서명된 문서는 지원하지 않아 처리하지 않았습니다"),
        ("PPTX_OPC_ERROR", "파일 구조를 확인할 수 없어 처리하지 않았습니다"),
    ] {
        error.diagnostics = vec![Diagnostic {
            code: code.into(),
            message: "internal diagnostic details".into(),
            node_id: None,
            blocking: true,
            params: Default::default(),
        }];
        assert_eq!(super::user_error(&error), message, "{code}");
    }
}

fn draw_properties_offscreen(harness: &mut Harness, input: RawInput) -> eframe::egui::FullOutput {
    let context = harness.ctx.clone();
    context.run(input, |ctx| {
        eframe::egui::CentralPanel::default().show(ctx, |ui| {
            let palette = theme::palette(ctx);
            harness.app.draw_properties(ui, &palette);
        });
    })
}

pub(super) fn find_text_center(output: &eframe::egui::FullOutput, expected: &str) -> Pos2 {
    use eframe::egui::epaint::Shape;
    fn visit(shape: &Shape, expected: &str, found: &mut Option<Pos2>) {
        match shape {
            Shape::Text(text) if text.galley.text() == expected => *found = Some(text.pos + text.galley.size() / 2.0),
            Shape::Vec(shapes) => shapes.iter().for_each(|shape| visit(shape, expected, found)),
            _ => {}
        }
    }
    let mut found = None;
    for clipped in &output.shapes { visit(&clipped.shape, expected, &mut found); }
    found.unwrap_or_else(|| panic!("the offscreen properties panel did not draw {expected:?}; text={:?}", drawn_text(output)))
}

pub(super) fn click_at(pos: Pos2) -> RawInput {
    let mut input = screen(1.0, 1000.0, 1000.0);
    input.events = vec![
        Event::PointerMoved(pos),
        Event::PointerButton { pos, button: PointerButton::Primary, pressed: true, modifiers: Modifiers::NONE },
        Event::PointerButton { pos, button: PointerButton::Primary, pressed: false, modifiers: Modifiers::NONE },
    ];
    input
}

fn component_page(id: &str, role: Option<PageRole>, nodes: Vec<Node>) -> Page {
    Page { nodes, role, ..Page::new(id, Size::slide()) }
}

fn install_canvas_properties(harness: &mut Harness, artifact: Artifact, page_index: u32, selected_node: Option<&str>) {
    let summary = ArtifactSummary {
        id: artifact.id.clone(), kind: artifact.kind, title: artifact.title.clone(), revision: artifact.revision,
        engine_id: artifact.engine_id.clone(), dirty: false, modified_at_ms: None, active_jobs: Vec::new(),
    };
    let mut canvas_context = context(&artifact.id, "component-ui-view", artifact.revision);
    canvas_context["capability_extensions"] = json!({ "components": {
        "read": "supported", "render": "supported", "edit": "supported", "preserve": "supported", "write": "supported", "reason": "local only"
    }});
    harness.app.screen = super::Screen::Editor;
    harness.app.active = Some(summary);
    harness.app.artifact = Some(artifact);
    harness.app.context = Some(serde_json::from_value(canvas_context).unwrap());
    harness.app.view_id = "component-ui-view".into();
    harness.app.view().page_index = page_index;
    harness.app.page_index = page_index;
    harness.app.selected_node = selected_node.map(str::to_owned);
    harness.app.selected_nodes = selected_node.map(|id| vec![id.to_owned()]).unwrap_or_default();
}

fn inline_text_harness() -> Harness {
    let mut harness = Harness::new();
    harness.sent();
    theme::install(&harness.ctx);
    let bounds = designllm_contracts::Rect { x: 100 * 9525, y: 100 * 9525, width: 320 * 9525, height: 48 * 9525 };
    let node: Node = serde_json::from_value(json!({
        "id": "text-1", "kind": "text", "text": "A",
        "bounds": { "x": bounds.x, "y": bounds.y, "width": bounds.width, "height": bounds.height }
    })).expect("text node");
    let mut artifact = Artifact::new(
        ArtifactKind::Canvas,
        "텍스트 입력 fixture",
        "canvas.v1",
        ArtifactBody::Canvas(CanvasDocument { pages: vec![component_page("page-1", None, vec![node])], ..Default::default() }),
    );
    artifact.id = "a1".into();
    artifact.revision = 1;
    install_canvas_properties(&mut harness, artifact, 0, Some("text-1"));
    let page = designllm_contracts::RenderPage {
        id: "page-1".into(),
        page_index: 0,
        size: Size::slide(),
        primitives: Vec::new(),
        source_map: vec![designllm_contracts::SourceRegion {
            node_id: "text-1".into(),
            bounds,
            logical_range: Some(designllm_contracts::TextRange { paragraph_id: "text-1".into(), start: 0, end: 1 }),
            grapheme_rects: Vec::new(),
        }],
    };
    let snapshot = designllm_contracts::RenderSnapshot {
        id: "text-snapshot".into(),
        artifact_id: "a1".into(),
        revision: 1,
        profile: designllm_contracts::RenderProfile::default(),
        scene: designllm_contracts::RenderScene {
            pages: vec![page], font_fingerprint: "fixture".into(), diagnostics: Vec::new(),
            metadata: Default::default(), links: Vec::new(),
        },
        preview: None,
        preview_transform: None,
    };
    harness.app.store_page_texture(&harness.ctx, 0, &snapshot, 1, 1, &[255, 255, 255, 255]);
    harness
}

fn draw_desktop_frame(harness: &mut Harness, mut input: RawInput) -> eframe::egui::FullOutput {
    let ctx = harness.ctx.clone();
    eframe::App::raw_input_hook(&mut harness.app, &ctx, &mut input);
    ctx.run(input, |ctx| { harness.app.draw_frame(ctx); })
}

#[test]
fn inline_canvas_text_commits_each_input_event_before_escape_finishes_editing() {
    let mut harness = inline_text_harness();
    draw_desktop_frame(&mut harness, screen(1.0, 1280.0, 800.0));
    let _ = harness.sent();

    let mut enter = screen(1.0, 1280.0, 800.0);
    enter.events.push(Event::Key {
        key: eframe::egui::Key::Enter,
        physical_key: Some(eframe::egui::Key::Enter),
        pressed: true,
        repeat: false,
        modifiers: Modifiers::NONE,
    });
    draw_desktop_frame(&mut harness, enter);
    assert!(harness.app.inline_text_edit.is_some(), "Enter starts editing the selected text node");
    let _ = harness.sent();

    let mut typing = screen(1.0, 1280.0, 800.0);
    typing.events.extend([Event::Text("한".into()), Event::Text("글".into())]);
    draw_desktop_frame(&mut harness, typing);
    let first = harness.sent();
    let changes: Vec<_> = first.iter().filter(|request| request.1 == "changes.apply").collect();
    assert_eq!(changes.len(), 1, "only the first input event is in flight");
    assert_eq!(changes[0].2["base_revision"], 1);
    assert_eq!(changes[0].2["actor"], format!("desktop:inline:{}", harness.app.view_id));
    assert_eq!(changes[0].2["operations"], json!([{
        "op": "replace_text",
        "range": { "paragraph_id": "text-1", "start": 1, "end": 1 },
        "text": "한"
    }]));
    harness.answer(&first, "changes.apply", Ok(json!({ "revision": 2, "changed_ids": ["text-1"], "diagnostics": [] })));

    let second = harness.sent();
    let second_change = second.iter().find(|request| request.1 == "changes.apply").expect("the second input event is serialized after revision 2");
    assert_eq!(second_change.2["base_revision"], 2);
    assert_eq!(second_change.2["operations"][0]["range"]["start"], 2);
    assert_eq!(second_change.2["operations"][0]["text"], "글");
    harness.answer(&second, "changes.apply", Ok(json!({ "revision": 3, "changed_ids": ["text-1"], "diagnostics": [] })));
    assert_eq!(harness.app.text_draft, "A한글");

    let mut escape = screen(1.0, 1280.0, 800.0);
    escape.events.push(Event::Key {
        key: eframe::egui::Key::Escape,
        physical_key: Some(eframe::egui::Key::Escape),
        pressed: true,
        repeat: false,
        modifiers: Modifiers::NONE,
    });
    draw_desktop_frame(&mut harness, escape);
    assert!(harness.app.inline_text_edit.is_none(), "Escape finishes after the input events commit");
    assert!(harness.sent().iter().all(|request| request.1 != "changes.apply"), "Escape does not create a batched final edit");
}

#[test]
fn inline_canvas_ime_commit_is_one_change_and_releases_its_target_reservation() {
    let mut harness = inline_text_harness();
    draw_desktop_frame(&mut harness, screen(1.0, 1280.0, 800.0));
    let _ = harness.sent();
    let mut enter = screen(1.0, 1280.0, 800.0);
    enter.events.push(Event::Key {
        key: eframe::egui::Key::Enter,
        physical_key: Some(eframe::egui::Key::Enter),
        pressed: true,
        repeat: false,
        modifiers: Modifiers::NONE,
    });
    draw_desktop_frame(&mut harness, enter);
    assert_eq!(harness.app.text_draft, "A", "the Enter that opens the editor is consumed, not inserted as a newline");
    let _ = harness.sent();

    let mut preedit = screen(1.0, 1280.0, 800.0);
    preedit.events.extend([
        Event::Ime(eframe::egui::ImeEvent::Enabled),
        Event::Ime(eframe::egui::ImeEvent::Preedit("ㅎ".into())),
    ]);
    draw_desktop_frame(&mut harness, preedit);
    let reserve = harness.sent();
    let request = reserve.iter().find(|request| request.1 == "targets.reserve").expect("IME composition reserves its text node");
    assert_eq!(request.2["node_ids"], json!(["text-1"]));
    let inline_actor = harness.app.inline_text_edit.as_ref().expect("inline session").actor.clone();
    assert_eq!(request.2["owner"], inline_actor);
    assert_eq!(request.2["view_id"], harness.app.view_id);
    harness.answer(&reserve, "targets.reserve", Ok(json!({
        "reservation_id": "ime-reservation",
        "artifact_id": "a1",
        "view_id": harness.app.view_id.clone(),
        "owner": inline_actor,
        "node_ids": ["text-1"],
        "expires_in_ms": 300000,
        "revision": 1
    })));

    let mut commit = screen(1.0, 1280.0, 800.0);
    commit.events.push(Event::Ime(eframe::egui::ImeEvent::Commit("하".into())));
    draw_desktop_frame(&mut harness, commit);
    let committed = harness.sent();
    let change = committed.iter().find(|request| request.1 == "changes.apply").expect("IME commit submits a ChangeSet");
    assert_eq!(change.2["base_revision"], 1);
    assert_eq!(change.2["operations"][0]["range"], json!({ "paragraph_id": "text-1", "start": 1, "end": 1 }), "the IME commit is inserted against committed text A, not the transient preedit range");
    assert_eq!(change.2["operations"][0]["text"], "하");
    let before_reply = harness.app.inline_text_edit.as_ref().map(|edit| (
        edit.revision,
        edit.committed_text.clone(),
        edit.composing,
        edit.ime_commit_pending,
        edit.reservation_id.clone(),
        edit.reservation_pending,
        edit.queue.len(),
        edit.in_flight.as_ref().map(|change| (
            change.range.clone(), change.text.clone(), change.after_text.clone(), change.ime_commit, change.request_id.clone(),
        )),
    ));
    assert!(before_reply.as_ref().is_some_and(|state| state.2 == false && state.3 && state.4.as_deref() == Some("ime-reservation") && state.7.as_ref().is_some_and(|change| change.3)), "IME state before ChangeSet reply: {before_reply:?}");
    harness.answer(&committed, "changes.apply", Ok(json!({ "revision": 2, "changed_ids": ["text-1"], "diagnostics": [] })));
    let after = harness.sent();
    let remaining = harness.app.inline_text_edit.as_ref().map(|edit| (
        edit.revision,
        edit.committed_text.clone(),
        edit.conflict.clone(),
        edit.composing,
        edit.ime_commit_pending,
        edit.reservation_id.clone(),
        edit.reservation_pending,
        edit.release_when_reserved,
        edit.queue.len(),
        edit.in_flight.as_ref().map(|change| change.ime_commit),
    ));
    assert!(after.iter().any(|request| request.1 == "targets.release"), "IME reservation release missing; reply_revision=2, before={before_reply:?}, calls={:?}, remaining={remaining:?}, pending={:?}", methods(&after), harness.app.pending.values().map(|pending| pending.method.as_str()).collect::<Vec<_>>());
}

#[test]
fn inline_canvas_ime_reservation_send_failure_keeps_the_committed_draft_retryable() {
    let mut harness = inline_text_harness();
    draw_desktop_frame(&mut harness, screen(1.0, 1280.0, 800.0));
    let _ = harness.sent();
    let mut enter = screen(1.0, 1280.0, 800.0);
    enter.events.push(Event::Key {
        key: eframe::egui::Key::Enter,
        physical_key: Some(eframe::egui::Key::Enter),
        pressed: true,
        repeat: false,
        modifiers: Modifiers::NONE,
    });
    draw_desktop_frame(&mut harness, enter);
    assert_eq!(harness.app.text_draft, "A");
    let _ = harness.sent();

    let (disconnected_rpc, _unused_replies, disconnected_work) = RpcWorker::detached();
    drop(disconnected_work);
    harness.app.rpc = disconnected_rpc;

    let mut preedit = screen(1.0, 1280.0, 800.0);
    preedit.events.extend([
        Event::Ime(eframe::egui::ImeEvent::Enabled),
        Event::Ime(eframe::egui::ImeEvent::Preedit("ㅎ".into())),
    ]);
    draw_desktop_frame(&mut harness, preedit);
    let during = harness.app.inline_text_edit.as_ref().expect("editing remains open");
    assert!(!during.reservation_pending, "a failed channel send is not left pending without a reply");
    assert!(during.conflict.is_some());

    let mut commit = screen(1.0, 1280.0, 800.0);
    commit.events.push(Event::Ime(eframe::egui::ImeEvent::Commit("하".into())));
    draw_desktop_frame(&mut harness, commit);
    let after = harness.app.inline_text_edit.as_ref().expect("the uncommitted change remains recoverable");
    assert_eq!(harness.app.text_draft, "A하");
    assert_eq!(after.queue.len(), 1, "the IME commit stays queued for retry");
    assert!(after.retry_allowed);
    assert!(!after.reservation_pending);
    assert!(harness.work.try_iter().all(|work| !matches!(work, Work::Call { method, .. } if method == "changes.apply")));
}

mod inline_text_adversarial_tests {
    use super::*;

    fn enter_inline_edit(harness: &mut Harness) {
        draw_desktop_frame(harness, screen(1.0, 1280.0, 800.0));
        let _ = harness.sent();
        let mut enter = screen(1.0, 1280.0, 800.0);
        enter.events.push(Event::Key {
            key: eframe::egui::Key::Enter,
            physical_key: Some(eframe::egui::Key::Enter),
            pressed: true,
            repeat: false,
            modifiers: Modifiers::NONE,
        });
        draw_desktop_frame(harness, enter);
        assert!(harness.app.inline_text_edit.is_some(), "Enter starts the text session");

        let selection = harness.sent();
        if selection.iter().any(|request| request.1 == "selection.set") {
            harness.answer(&selection, "selection.set", Ok(json!({})));
            let context_reply = harness.sent();
            if context_reply.iter().any(|request| request.1 == "context.read") {
                let mut snapshot = context("a1", &harness.app.view_id, 1);
                snapshot["selection"] = json!({
                    "node_ids": ["text-1"],
                    "text_range": { "paragraph_id": "text-1", "start": 1, "end": 1 },
                });
                harness.answer(&context_reply, "context.read", Ok(snapshot));
            }
        }
        let _ = harness.sent();
    }

    fn target_busy() -> DltError {
        DltError {
            code: ErrorCode::TargetBusy,
            message: "text target is reserved".into(),
            current_revision: None,
            affected_ids: vec!["text-1".into()],
            retryable: true,
            diagnostics: Vec::new(),
        }
    }

    fn refresh_after_external_change(harness: &mut Harness, mut artifact: Artifact, text: Option<&str>) {
        artifact.revision = 2;
        let ArtifactBody::Canvas(document) = &mut artifact.body else { panic!("Canvas artifact") };
        if let Some(text) = text {
            document.pages[0].nodes[0].text = text.into();
        } else {
            document.pages[0].nodes.clear();
        }

        let refresh = harness.sent();
        assert!(refresh.iter().any(|request| request.1 == "workspace.list"));
        assert!(refresh.iter().any(|request| request.1 == "context.read"));
        assert!(refresh.iter().any(|request| request.1 == "artifact.inspect"));
        let mut active = summary("a1", "canvas", 2);
        active["engine_id"] = json!(artifact.engine_id);
        harness.answer(&refresh, "workspace.list", Ok(json!({ "workspace_id": "w1", "artifacts": [active] })));

        let view_id = harness.app.view_id.clone();
        let mut snapshot = context("a1", &view_id, 2);
        snapshot["selection"] = if text.is_some() {
            json!({ "node_ids": ["text-1"], "text_range": { "paragraph_id": "text-1", "start": 1, "end": 1 } })
        } else {
            json!({ "node_ids": [], "text_range": null })
        };
        harness.answer(&refresh, "context.read", Ok(snapshot));
        harness.answer(&refresh, "artifact.inspect", Ok(serde_json::to_value(artifact).expect("external Artifact JSON")));
        let _ = harness.sent();
    }

    fn copy_inline_draft(harness: &mut Harness, expected: &str) {
        // A new egui Area uses its first frame to measure content and is invisible until repaint.
        let _sizing_pass = draw_desktop_frame(harness, screen(1.0, 1280.0, 800.0));
        let conflict = draw_desktop_frame(harness, screen(1.0, 1280.0, 800.0));
        let shown_text = drawn_text(&conflict);
        let session = harness.app.inline_text_edit.as_ref().map(|edit| (
            edit.id, edit.conflict.clone(), edit.composing, edit.retry_allowed,
            edit.in_flight.is_some(), edit.reservation_pending, edit.queue.len(),
        ));
        assert!(shown_text.iter().any(|text| text == "초안 복사"),
            "conflict retains its copy action; session={session:?}; artifact={:?}; active={:?}; board_rect={:?}; drawn_text={shown_text:?}",
            harness.app.artifact.as_ref().map(|artifact| (&artifact.id, artifact.revision)),
            harness.app.active.as_ref().map(|active| (&active.id, active.revision)),
            harness.app.board_rect,
        );
        let position = find_text_center(&conflict, "초안 복사");
        let mut click = screen(1.0, 1280.0, 800.0);
        click.events.extend([
            Event::PointerMoved(position),
            Event::PointerButton { pos: position, button: PointerButton::Primary, pressed: true, modifiers: Modifiers::NONE },
            Event::PointerButton { pos: position, button: PointerButton::Primary, pressed: false, modifiers: Modifiers::NONE },
        ]);
        let output = draw_desktop_frame(harness, click);
        let copied = output.platform_output.commands.iter().find_map(|command| match command {
            eframe::egui::OutputCommand::CopyText(text) => Some(text.as_str()),
            _ => None,
        });
        assert_eq!(copied, Some(expected), "copy uses the full draft, not the truncated preview");
    }

    #[test]
    fn target_busy_then_external_text_change_or_delete_preserves_and_copies_the_full_draft() {
        for (label, external_text) in [("changed", Some("external")), ("deleted", None)] {
            let mut harness = inline_text_harness();
            enter_inline_edit(&mut harness);
            let suffix = "x".repeat(300);
            let expected = format!("A{suffix}");
            let mut typing = screen(1.0, 1280.0, 800.0);
            typing.events.push(Event::Text(suffix.clone()));
            draw_desktop_frame(&mut harness, typing);
            assert_eq!(harness.app.text_draft, expected, "{label}");
            let request = harness.sent();
            assert_eq!(methods(&request), ["changes.apply"], "{label}");
            harness.answer(&request, "changes.apply", Err(target_busy()));

            let before = harness.app.artifact.as_ref().expect("base Canvas").clone();
            refresh_after_external_change(&mut harness, before, external_text);
            let session = harness.app.inline_text_edit.as_ref().expect("conflicted edit remains open");
            assert_eq!(harness.app.text_draft, expected, "{label}: the full draft survives refresh");
            assert_eq!(session.queue.front().map(|edit| edit.text.as_str()), Some(suffix.as_str()), "{label}: retry payload stays intact");
            assert!(!session.retry_allowed, "{label}: changed/deleted text is never rebased");
            copy_inline_draft(&mut harness, &expected);
        }
    }

    #[test]
    fn escape_waits_for_in_flight_reply_and_an_old_ticket_duplicate_cannot_change_the_next_session() {
        let mut harness = inline_text_harness();
        enter_inline_edit(&mut harness);
        let mut typing = screen(1.0, 1280.0, 800.0);
        typing.events.push(Event::Text("x".into()));
        draw_desktop_frame(&mut harness, typing);
        let request = harness.sent();
        let old_ticket = request.iter().find(|item| item.1 == "changes.apply").expect("in-flight text change").0;
        let old_session = harness.app.inline_text_edit.as_ref().expect("old session").id;

        let mut escape = screen(1.0, 1280.0, 800.0);
        escape.events.push(Event::Key {
            key: eframe::egui::Key::Escape,
            physical_key: Some(eframe::egui::Key::Escape),
            pressed: true,
            repeat: false,
            modifiers: Modifiers::NONE,
        });
        draw_desktop_frame(&mut harness, escape);
        let pending = harness.app.inline_text_edit.as_ref().expect("Escape waits for the sent transaction");
        assert_eq!(pending.id, old_session);
        assert!(pending.finish_requested);
        assert!(!harness.app.begin_inline_text_edit(&harness.ctx, "text-1", 0, None), "a new session cannot start before the old ticket resolves");

        harness.answer(&request, "changes.apply", Ok(json!({ "revision": 2, "changed_ids": ["text-1"], "diagnostics": [] })));
        assert!(harness.app.inline_text_edit.is_none(), "the late success closes only the canceled session");
        let refresh = harness.sent();
        let mut updated = harness.app.artifact.as_ref().expect("Canvas artifact").clone();
        updated.revision = 2;
        let ArtifactBody::Canvas(document) = &mut updated.body else { panic!("Canvas artifact") };
        document.pages[0].nodes[0].text = "Ax".into();
        let mut active = summary("a1", "canvas", 2);
        active["engine_id"] = json!(updated.engine_id);
        harness.answer(&refresh, "workspace.list", Ok(json!({ "workspace_id": "w1", "artifacts": [active] })));
        let view_id = harness.app.view_id.clone();
        let mut snapshot = context("a1", &view_id, 2);
        snapshot["selection"] = json!({ "node_ids": ["text-1"], "text_range": { "paragraph_id": "text-1", "start": 2, "end": 2 } });
        harness.answer(&refresh, "context.read", Ok(snapshot));
        harness.answer(&refresh, "artifact.inspect", Ok(serde_json::to_value(updated).expect("committed Artifact JSON")));
        let _ = harness.sent();

        assert!(harness.app.begin_inline_text_edit(&harness.ctx, "text-1", 0, None), "the next session starts from committed text");
        let next_id = harness.app.inline_text_edit.as_ref().expect("new session").id;
        assert_ne!(next_id, old_session);
        assert_eq!(harness.app.text_draft, "Ax");
        let _ = harness.sent();

        harness.replies.send(RpcReply {
            ticket: old_ticket,
            result: Ok(RpcPayload::Json(json!({ "revision": 3, "changed_ids": ["text-1"], "diagnostics": [] }))),
        }).expect("the test window still listens");
        harness.app.process_replies(&harness.ctx);
        let current = harness.app.inline_text_edit.as_ref().expect("the stale ticket cannot close the new session");
        assert_eq!(current.id, next_id);
        assert_eq!(harness.app.text_draft, "Ax");
        assert_eq!(harness.app.active.as_ref().unwrap().revision, 2);
        assert!(harness.sent().is_empty(), "a reply for the removed old ticket sends no follow-up work");
    }

    #[test]
    fn stale_empty_table_cell_edit_after_external_merge_keeps_and_copies_the_full_draft() {
        use designllm_contracts::{AssetRef, AssetStore, CanvasTableSpan, EngineAdapter, EngineContext, Operation};

        struct NoAssets;
        impl AssetStore for NoAssets {
            fn put(&self, _bytes: &[u8], _mime: &str) -> designllm_contracts::DltResult<AssetRef> {
                Err(DltError::new(ErrorCode::NotFound, "the fixture has no assets"))
            }
            fn read(&self, _asset: &AssetRef) -> designllm_contracts::DltResult<Vec<u8>> {
                Err(DltError::new(ErrorCode::NotFound, "the fixture has no assets"))
            }
            fn lookup(&self, _hash: &str) -> designllm_contracts::DltResult<AssetRef> {
                Err(DltError::new(ErrorCode::NotFound, "the fixture has no assets"))
            }
        }

        let (mut harness, _, _, _) = super::ui_channel_tests::inline_table_cell_harness("c12", false, false, false);
        draw_desktop_frame(&mut harness, screen(1.0, 1280.0, 800.0));
        let _ = harness.sent();

        let mut enter = screen(1.0, 1280.0, 800.0);
        enter.events.push(Event::Key {
            key: eframe::egui::Key::Enter,
            physical_key: Some(eframe::egui::Key::Enter),
            pressed: true,
            repeat: false,
            modifiers: Modifiers::NONE,
        });
        draw_desktop_frame(&mut harness, enter);
        assert!(harness.app.inline_text_edit.is_some(), "Enter starts editing the empty Rectangle table cell");
        assert_eq!(harness.app.text_draft, "");
        let selection = harness.sent();
        if selection.iter().any(|request| request.1 == "selection.set") {
            harness.answer(&selection, "selection.set", Ok(json!({})));
            let context_request = harness.sent();
            if context_request.iter().any(|request| request.1 == "context.read") {
                let mut snapshot = context("a1", &harness.app.view_id, 1);
                snapshot["selection"] = json!({
                    "node_ids": ["c12"],
                    "text_range": { "paragraph_id": "c12", "start": 0, "end": 0 },
                });
                harness.answer(&context_request, "context.read", Ok(snapshot));
            }
        }
        let _ = harness.sent();

        let suffix = "x".repeat(300);
        let mut typing = screen(1.0, 1280.0, 800.0);
        typing.events.push(Event::Text(suffix.clone()));
        draw_desktop_frame(&mut harness, typing);
        assert_eq!(harness.app.text_draft, suffix);
        let edit_request = harness.sent();
        assert_eq!(methods(&edit_request), ["changes.apply"]);
        assert_eq!(edit_request[0].2["base_revision"], 1);
        assert_eq!(edit_request[0].2["operations"], json!([{
            "op": "replace_text",
            "range": { "paragraph_id": "c12", "start": 0, "end": 0 },
            "text": suffix,
        }]));

        let base = harness.app.artifact.as_ref().expect("base Canvas").clone();
        let expected = {
            let ArtifactBody::Canvas(document) = &base.body else { panic!("Canvas body") };
            document.pages[0].nodes.iter().find(|node| node.id == "table").expect("table").ext.table.clone().expect("table metadata")
        };
        assert!(expected.cell_spans.is_empty(), "the selected cell is not covered in the base revision");
        let mut merged_table = expected.clone();
        merged_table.cell_spans.insert("c11".into(), CanvasTableSpan { row_span: 1, column_span: 2 });
        let merge = Operation::SetCanvasTableStructure { table_id: "table".into(), expected, table: merged_table };
        let assets = NoAssets;
        let font_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets/fonts");
        let renderer = designllm_render::Renderer::bundled_only(&font_dir).expect("bundled fonts");
        let engine_context = EngineContext { assets: &assets, renderer: &renderer };
        let mut merged = designllm_canvas::CanvasEngine.prepare_change(&base, &[merge], &engine_context)
            .expect("the product Canvas engine accepts the external merge of the still-empty cell").artifact;
        merged.revision = 2;
        let ArtifactBody::Canvas(document) = &merged.body else { panic!("merged Canvas body") };
        let table = document.pages[0].nodes.iter().find(|node| node.id == "table").expect("merged table").ext.table.as_ref().unwrap();
        assert_eq!(table.cell_spans.get("c11"), Some(&CanvasTableSpan { row_span: 1, column_span: 2 }));
        assert!(document.pages[0].nodes[0].children.iter().find(|cell| cell.id == "c12").unwrap().text.is_empty(), "the local draft was never persisted before merge");

        let mut stale = DltError::new(ErrorCode::RevisionConflict, "base revision 1 is no longer current");
        stale.current_revision = Some(2);
        harness.answer(&edit_request, "changes.apply", Err(stale));
        let refresh = harness.sent();
        assert!(refresh.iter().any(|request| request.1 == "workspace.list"));
        assert!(refresh.iter().any(|request| request.1 == "context.read"));
        assert!(refresh.iter().any(|request| request.1 == "artifact.inspect"));
        let mut active = summary("a1", "canvas", 2);
        active["engine_id"] = json!(merged.engine_id.clone());
        harness.answer(&refresh, "workspace.list", Ok(json!({ "workspace_id": "w1", "artifacts": [active] })));
        let mut snapshot = context("a1", &harness.app.view_id, 2);
        snapshot["selection"] = json!({
            "node_ids": ["c12"],
            "text_range": { "paragraph_id": "c12", "start": 0, "end": 0 },
        });
        harness.answer(&refresh, "context.read", Ok(snapshot));
        harness.answer(&refresh, "artifact.inspect", Ok(serde_json::to_value(merged).expect("external merged Artifact JSON")));
        let _ = harness.sent();

        let session = harness.app.inline_text_edit.as_ref().expect("the stale cell draft remains open");
        assert_eq!(harness.app.text_draft, suffix, "the complete draft survives the merge refresh");
        assert_eq!(session.committed_text, "");
        assert_eq!(session.queue.front().map(|edit| edit.text.as_str()), Some(suffix.as_str()));
        assert!(!session.retry_allowed, "a now-covered cell is not offered for retry");
        assert!(session.conflict.is_some());
        copy_inline_draft(&mut harness, &suffix);
        assert!(harness.sent().iter().all(|request| request.1 != "changes.apply"), "copying does not retry the stale edit");
    }
}

#[test]
fn offscreen_component_page_instance_and_detach_controls_send_their_commands() {
    let mut source_toggle = Harness::new();
    theme::install(&source_toggle.ctx);
    source_toggle.sent();
    let mut artifact = Artifact::new(ArtifactKind::Canvas, "Component source", "designllm-canvas", ArtifactBody::Canvas(CanvasDocument {
        pages: vec![component_page("regular", None, vec![]), component_page("other", None, vec![])], ..Default::default()
    }));
    artifact.id = "source-toggle".into();
    artifact.revision = 1;
    install_canvas_properties(&mut source_toggle, artifact, 1, None);
    let first = draw_properties_offscreen(&mut source_toggle, screen(1.0, 1000.0, 1000.0));
    let checkbox = find_text_center(&first, "페이지를 소스로 사용");
    draw_properties_offscreen(&mut source_toggle, click_at(checkbox));
    let calls = source_toggle.sent();
    let change = calls.iter().find(|call| call.1 == "changes.apply").expect("source checkbox sends a change");
    assert_eq!(change.2["operations"][0]["op"], "set_page_role");
    assert_eq!(change.2["operations"][0]["page_id"], "other");

    let mut insert = Harness::new();
    theme::install(&insert.ctx);
    insert.sent();
    let source_node = Node {
        id: "source-text".into(), kind: NodeKind::Text, bounds: designllm_contracts::Rect { x: 10, y: 10, width: 1000, height: 300 },
        text: "Component".into(), style: Style::default(), asset: None, children: vec![], layout: None, interactions: vec![],
        variants: Default::default(), ext: NodeExt::default(),
    };
    let artifact = Artifact::new(ArtifactKind::Canvas, "Insert component", "designllm-canvas", ArtifactBody::Canvas(CanvasDocument {
        pages: vec![component_page("target", None, vec![]), component_page("source", Some(PageRole::Component), vec![source_node.clone()])],
        ..Default::default()
    }));
    install_canvas_properties(&mut insert, artifact, 0, None);
    let first = draw_properties_offscreen(&mut insert, screen(1.0, 1000.0, 1000.0));
    let button = find_text_center(&first, "컴포넌트 삽입");
    draw_properties_offscreen(&mut insert, click_at(button));
    let calls = insert.sent();
    let change = calls.iter().find(|call| call.1 == "changes.apply").expect("insert button sends a change");
    assert_eq!(change.2["operations"][0]["op"], "instantiate_component");
    assert_eq!(change.2["operations"][0]["component_page_id"], "source");
    assert!(change.2["operations"][0]["instance_id"].as_str().is_some_and(|id| !id.is_empty()));

    let mut detach = Harness::new();
    theme::install(&detach.ctx);
    detach.sent();
    let mut clone = source_node.clone();
    clone.id = "instance::source-text".into();
    let mut instance = Node {
        id: "instance".into(), kind: NodeKind::Group, bounds: designllm_contracts::Rect { x: 0, y: 0, width: 2000, height: 1000 },
        text: String::new(), style: Style::default(), asset: None, children: vec![clone], layout: None, interactions: vec![],
        variants: Default::default(), ext: NodeExt { instance: Some(InstanceRef { component_page_id: "source".into(), overrides: Default::default() }), ..Default::default() },
    };
    instance.ext.name = Some("Instance".into());
    let artifact = Artifact::new(ArtifactKind::Canvas, "Detach component", "designllm-canvas", ArtifactBody::Canvas(CanvasDocument {
        pages: vec![component_page("target", None, vec![instance]), component_page("source", Some(PageRole::Component), vec![source_node])],
        ..Default::default()
    }));
    install_canvas_properties(&mut detach, artifact, 0, Some("instance"));
    let first = draw_properties_offscreen(&mut detach, screen(1.0, 1000.0, 1000.0));
    let button = find_text_center(&first, "원본에서 분리");
    draw_properties_offscreen(&mut detach, click_at(button));
    let calls = detach.sent();
    let change = calls.iter().find(|call| call.1 == "changes.apply").expect("detach button sends a change");
    assert_eq!(change.2["operations"][0]["op"], "detach_instance");
    assert_eq!(change.2["operations"][0]["instance_id"], "instance");
}

#[test]
fn canvas_page_notes_are_editable_and_commit_on_blur() {
    use eframe::egui::{Id, Key};

    let mut harness = Harness::new();
    theme::install(&harness.ctx);
    harness.sent();
    let mut artifact = Artifact::new(ArtifactKind::Canvas, "보드", "designllm-canvas", ArtifactBody::Canvas(CanvasDocument {
        pages: vec![Page { notes: "기존 메모".into(), ..Page::new("canvas-page", Size::a4()) }],
        ..Default::default()
    }));
    artifact.id = "a1".into();
    artifact.revision = 1;
    install_canvas_properties(&mut harness, artifact, 0, None);
    harness.app.context.as_mut().unwrap().capabilities.insert(
        designllm_contracts::Feature::Notes,
        serde_json::from_value(supported()).expect("notes capability"),
    );

    let initial = draw_properties_offscreen(&mut harness, screen(1.0, 1000.0, 1000.0));
    assert!(drawn_text(&initial).iter().any(|text| text == "노트"), "Canvas page properties show notes");

    harness.ctx.memory_mut(|memory| memory.request_focus(Id::new(("slide-notes", 0))));
    let mut typing = screen(1.0, 1000.0, 1000.0);
    typing.events.extend([
        Event::Key { key: Key::A, physical_key: Some(Key::A), pressed: true, repeat: false, modifiers: Modifiers::COMMAND },
        Event::Text("수정된 메모".into()),
    ]);
    draw_properties_offscreen(&mut harness, typing);
    assert!(harness.sent().iter().all(|request| request.1 != "changes.apply"), "notes are not committed while editing");

    let blur = click_at(find_text_center(&initial, "페이지"));
    draw_properties_offscreen(&mut harness, blur);
    let change = harness.sent().into_iter().find(|request| request.1 == "changes.apply").expect("leaving the field commits notes");
    assert_eq!(change.2["allowed_intents"], json!(["content"]));
    assert_eq!(change.2["operations"][0]["op"], "set_page_notes");
    assert_eq!(change.2["operations"][0]["page_id"], "canvas-page");
    assert_eq!(change.2["operations"][0]["notes"], "수정된 메모");
}

#[test]
fn no_panel_is_drawn_under_another_at_any_window_size_or_scale() {
    for scale in [1.0, 1.5, 2.0] {
        for (width, height) in [(800.0, 560.0), (1232.0, 760.0), (1440.0, 900.0)] {
            for (left, right) in [(true, true), (false, true), (true, false), (false, false)] {
                let mut harness = Harness::new();
                harness.open_document("canvas", 1);
                theme::install(&harness.ctx);
                harness.app.view().left_open = left;
                harness.app.view().right_open = right;
                // A long document name must be cut short, not push the buttons off the window.
                if let Some(active) = harness.app.active.as_mut() {
                    active.title = "아주 긴 작업물 이름 ".repeat(12);
                }
                let mut layout = None;
                for _ in 0..2 {
                    let _ = harness.ctx.run(screen(scale, width, height), |ctx| {
                        layout = Some(harness.app.draw_frame(ctx));
                    });
                }
                let layout = layout.expect("a frame was drawn");
                let window = Rect::from_min_size(Pos2::ZERO, Vec2::new(width, height));
                let expected = crate::frame::editor_frame(window, left, harness.app.view_ref().left_width, right, harness.app.view_ref().right_width);
                let center = layout.center;
                assert!(window.expand(0.5).contains_rect(center), "the board leaves the window (scale {scale}, {width}x{height})");
                assert!(layout.top.bottom() <= center.top() + 0.5, "the board starts above the top bar");
                for other in [layout.left, layout.right].into_iter().flatten() {
                    assert!(window.expand(0.5).contains_rect(other), "a panel leaves the window");
                    if expected.left_docked || other != layout.left.unwrap_or(other) {
                        assert!(!center.shrink(1.0).intersects(other), "the board {center:?} reaches under {other:?} (scale {scale}, {width}x{height})");
                    }
                }
            }
        }
    }
}


#[test]
fn a_pdf_page_without_text_is_reported_and_sends_no_change() {
    let mut harness = Harness::new();
    harness.open_document("figure", 1);
    harness.sent();
    // The reply must be the one the import awaits, as import_pdf_evidence leaves it: a reply no import awaits shows nothing (figure_tools.rs pdf_reply_is_current).
    let ticket = harness.app.call("source.extract_pdf", json!({}), super::PendingAction::PdfEvidenceImport);
    harness.app.pdf_evidence_target = Some(super::PdfEvidenceTarget {
        artifact_id: "a1".into(),
        revision: 1,
        page_index: 0,
        file_name: "paper.pdf".into(),
        asset_sha256: Some("a".repeat(64)),
        awaiting: ticket,
    });
    let request = harness.sent();
    harness.answer(&request, "source.extract_pdf", Ok(json!({ "evidence": [] })));
    assert_eq!(
        harness.message(),
        Some((Severity::Error, "1쪽에는 글자가 없어 PDF 근거를 넣지 않았습니다".to_owned()))
    );
    assert!(harness.sent().is_empty(), "nothing is sent for an empty page");
}

#[test]
fn project_save_uses_the_current_artifacts_managed_file() {
    let mut harness = Harness::new();
    harness.open_document("canvas", 1);
    harness.sent();
    harness.app.home.project_folder = Some("C:/work/project".into());
    harness.app.save_path = "C:/out/another-artifact.designllm".into();
    harness.app.invoke("file.save", json!({})).unwrap();
    let requests = harness.sent();
    assert_eq!(methods(&requests), ["artifact.save"]);
    assert_eq!(requests[0].2["artifact_id"], "a1");
    assert_eq!(requests[0].2["expected_revision"], 1);
    assert_eq!(requests[0].2["target_path"], "");
    assert_eq!(requests[0].2["overwrite"], false);
}

#[test]
fn saving_and_exporting_confirm_once_and_the_confirmation_goes_away() {
    let mut harness = Harness::new();
    harness.open_document("canvas", 1);
    harness.sent();
    harness.app.save_path = "C:/out/new.pptx".into();
    harness.app.save_active();
    let request = harness.sent();
    assert_eq!(methods(&request), ["artifact.save"]);
    let request_id = request[0].2["request_id"].as_str().expect("save request id").to_owned();
    harness.app.handle_events(json!({ "events": [{
        "event_seq": 1,
        "artifact_id": "a1",
        "revision": 1,
        "event": "artifact.saved",
        "detail": { "request_id": request_id, "diagnostics": [{
            "code": "original_bytes",
            "message": "unchanged since open; original file bytes written",
            "node_id": null,
            "blocking": false,
            "params": {}
        }] }
    }] }));
    harness.sent();
    harness.answer(&request, "artifact.save", Ok(json!({})));
    assert_eq!(harness.message(), Some((Severity::Info, "저장했습니다".to_owned())));

    let later = Instant::now() + std::time::Duration::from_secs(5);
    assert!(harness.app.notices.current(later).is_none(), "a result message does not stay on screen");
}

#[test]
fn save_warnings_from_artifact_saved_are_shown_before_or_after_the_reply() {
    let mut harness = Harness::new();
    harness.open_document("canvas", 1);
    harness.sent();
    harness.app.save_path = "C:/out/new.pptx".into();

    for (sequence, event_first) in [(1, true), (2, false)] {
        harness.app.save_active();
        let request = harness.sent();
        let request_id = request[0].2["request_id"].as_str().expect("save request id").to_owned();
        let event = json!({
            "events": [{
                "protocol_version": 1,
                "event_seq": sequence,
                "artifact_id": "a1",
                "revision": 1,
                "event": "artifact.saved",
                "detail": {
                    "request_id": request_id,
                    "diagnostics": [{
                        "code": "PPTX_LOSS",
                        "message": "internal count=72 private part name",
                        "node_id": "private-node",
                        "blocking": false,
                        "params": {"private": "value"}
                    }]
                }
            }]
        });

        if event_first {
            harness.app.handle_events(event.clone());
            harness.sent();
        }
        harness.answer(&request, "artifact.save", Ok(json!({})));
        harness.sent();
        if !event_first {
            assert_eq!(harness.message(), Some((Severity::Info, "저장했습니다".into())));
            harness.app.handle_events(event);
            harness.sent();
        }

        let (severity, text) = harness.message().expect("warning is visible after successful save");
        assert_eq!(severity, Severity::Warning);
        assert!(text.contains("일부 요소를 저장 형식에서 완전히 표현하지 못했습니다"));
        assert!(!text.contains("PPTX_LOSS"));
        assert!(!text.contains("private"));
    }
}

#[test]
fn save_event_diagnostics_stay_correlated_after_switching_documents() {
    let mut harness = Harness::new();
    harness.open_document("canvas", 1);
    harness.sent();
    harness.app.save_path = "C:/out/new.pptx".into();
    harness.app.save_active();
    let request = harness.sent();
    let request_id = request[0].2["request_id"].as_str().expect("save request id").to_owned();

    harness.app.active = Some(serde_json::from_value(summary("a2", "canvas", 1)).expect("another summary"));
    harness.answer(&request, "artifact.save", Ok(json!({})));
    assert_eq!(harness.message(), Some((Severity::Info, "저장했습니다".into())));
    harness.sent();

    harness.app.handle_events(json!({ "events": [{
        "event_seq": 1,
        "artifact_id": "a1",
        "revision": 1,
        "event": "artifact.saved",
        "detail": { "request_id": request_id, "diagnostics": [{
            "code": "PPTX_LOSS",
            "message": "private detail",
            "node_id": null,
            "blocking": false,
            "params": {}
        }] }
    }] }));
    harness.sent();
    let (severity, text) = harness.message().expect("the old save's diagnostic is still shown");
    assert_eq!(severity, Severity::Warning);
    assert!(text.contains("저장 형식에서 완전히 표현하지 못했습니다"));
    assert!(!text.contains("private"));
}

#[test]
fn ambiguous_save_failures_keep_event_diagnostics_until_confirmation_or_timeout() {
    let mut harness = Harness::new();
    harness.open_document("canvas", 1);
    harness.sent();
    harness.app.save_path = "C:/out/new.pptx".into();

    harness.app.save_active();
    let request = harness.sent();
    let request_id = request[0].2["request_id"].as_str().expect("save request id").to_owned();
    harness.answer(&request, "artifact.save", Err(DltError::new(ErrorCode::HostNotRunning, "private transport message")));
    assert!(harness.app.pending_save_notices.contains_key(&request_id), "an unknown result keeps the event correlation");
    assert_eq!(harness.message().unwrap().0, Severity::Error, "the transport error remains visible until an event resolves it");

    harness.app.handle_events(json!({ "events": [{
        "event_seq": 1,
        "artifact_id": "a1",
        "revision": 1,
        "event": "artifact.saved",
        "detail": { "request_id": request_id, "diagnostics": [{
            "code": "PPTX_LOSS",
            "message": "private part name and details",
            "node_id": "private-node",
            "blocking": false,
            "params": {"private": "value"}
        }] }
    }] }));
    harness.sent();
    let (severity, text) = harness.message().expect("the late event confirms the save warning");
    assert_eq!(severity, Severity::Warning);
    assert!(text.contains("저장 형식에서 완전히 표현하지 못했습니다"));
    assert!(!text.contains("private"));
    assert!(harness.app.pending_save_notices.is_empty());

    harness.app.save_active();
    let event_first = harness.sent();
    let event_first_id = event_first[0].2["request_id"].as_str().expect("save request id").to_owned();
    harness.app.handle_events(json!({ "events": [{
        "event_seq": 2,
        "artifact_id": "a1",
        "revision": 1,
        "event": "artifact.saved",
        "detail": { "request_id": event_first_id, "diagnostics": [] }
    }] }));
    harness.sent();
    harness.answer(&event_first, "artifact.save", Err(DltError::new(ErrorCode::HostNotRunning, "private transport message")));
    assert!(harness.app.pending_save_notices.is_empty(), "a prior saved event confirms the ambiguous result");
    assert_eq!(harness.message(), Some((Severity::Info, "저장했습니다".into())));

    harness.app.save_active();
    let rejected = harness.sent();
    let rejected_id = rejected[0].2["request_id"].as_str().expect("save request id").to_owned();
    harness.answer(&rejected, "artifact.save", Err(DltError::new(ErrorCode::InvalidArgument, "definite refusal")));
    assert!(!harness.app.pending_save_notices.contains_key(&rejected_id), "a definitive refusal clears correlation");
    harness.app.handle_events(json!({ "events": [{
        "event_seq": 3,
        "artifact_id": "a1",
        "revision": 1,
        "event": "artifact.saved",
        "detail": { "request_id": rejected_id, "diagnostics": [] }
    }] }));
    harness.sent();
    assert_eq!(harness.message().unwrap().0, Severity::Error, "an unrelated late event cannot turn a refusal into success");

    harness.app.save_active();
    let timed_out = harness.sent();
    let timed_out_id = timed_out[0].2["request_id"].as_str().expect("save request id").to_owned();
    harness.answer(&timed_out, "artifact.save", Err(DltError::new(ErrorCode::Unavailable, "host unavailable")));
    harness.app.pending_save_notices.get_mut(&timed_out_id).expect("unknown result retained").started =
        Instant::now() - std::time::Duration::from_secs(121);
    harness.app.handle_events(json!({ "events": [] }));
    assert!(!harness.app.pending_save_notices.contains_key(&timed_out_id), "unresolved correlations expire");
}

#[test]
fn workspace_notices_are_safe_persistent_and_session_acknowledged() {
    let mut harness = Harness::new();
    let startup = harness.sent();
    let notices = vec![
        Diagnostic {
            code: "store_backup_missing".into(),
            message: "복구용 데이터베이스 백업을 찾을 수 없습니다.".into(),
            node_id: Some("private-node".into()),
            blocking: false,
            params: [("private".into(), json!("value"))].into_iter().collect(),
        },
        Diagnostic {
            code: "unknown.notice".into(),
            message: "private raw message".into(),
            node_id: None,
            blocking: false,
            params: Default::default(),
        },
    ];
    harness.answer(
        &startup,
        "workspace.list",
        Ok(json!({ "workspace_id": "w1", "artifacts": [], "workspace_notices": notices })),
    );

    let (_, text) = harness.message().expect("workspace notice remains visible");
    assert_eq!(text, "복구용 데이터베이스 백업을 찾을 수 없습니다.\n작업공간에서 확인이 필요한 상태가 발견되었습니다. 자세한 내용은 로그를 확인해 주세요.");
    assert!(!text.contains("unknown.notice"));
    assert!(!text.contains("private"));
    harness.app.update_workspace_notices("w1", None);
    assert_eq!(harness.message().unwrap().1, text, "an omitted notice field leaves the current warning unchanged");

    harness.app.set_error("일시적인 오류".into());
    assert_eq!(harness.message().unwrap().1, "일시적인 오류");
    harness.app.notices.clear_transient();
    assert!(harness.message().is_some(), "the workspace notice returns after a transient message");
    harness.app.acknowledge_workspace_notice();
    assert!(harness.message().is_none(), "confirmation dismisses it for this app session");

    harness.app.update_workspace_notices("w1", Some(vec![
        Diagnostic {
            code: "store_backup_missing".into(),
            message: "복구용 데이터베이스 백업을 찾을 수 없습니다.".into(),
            node_id: Some("private-node".into()),
            blocking: false,
            params: [("private".into(), json!("value"))].into_iter().collect(),
        },
        Diagnostic {
            code: "unknown.notice".into(),
            message: "private raw message".into(),
            node_id: None,
            blocking: false,
            params: Default::default(),
        },
    ]));
    assert!(harness.message().is_none(), "the same notice is not repeated after confirmation");
}

#[test]
fn committed_change_warnings_use_safe_notice_text() {
    let mut harness = Harness::new();
    harness.open_document("canvas", 1);
    harness.sent();
    harness.app.call("changes.apply", json!({}), super::PendingAction::Apply);
    let request = harness.sent();
    harness.answer(
        &request,
        "changes.apply",
        Ok(json!({
            "revision": 2,
            "diagnostics": [{
                "code": "NATIVE_UNIT_ROUNDING",
                "message": "private dimensions and raw unit values",
                "node_id": "private-node",
                "blocking": false,
                "params": {"private": "value"}
            }]
        })),
    );
    let (severity, text) = harness.message().expect("engine warning is visible after edit");
    assert_eq!(severity, Severity::Warning);
    assert_eq!(text, "일부 값이 저장 형식의 정밀도에 맞게 조정되었습니다.");
    assert!(!text.contains("NATIVE_UNIT_ROUNDING"));
    assert!(!text.contains("private"));
}

#[test]
fn the_selected_element_stays_selected_after_an_edit() {
    use designllm_contracts::{
        Artifact, ArtifactBody, ArtifactKind, Diagram, FigureDocument, FigurePanel, PanelBody,
        Rect as EmuRect, Selection, Size, Style,
    };
    let panel = FigurePanel {
        id: "p1".into(),
        bounds: EmuRect { x: 0, y: 0, width: 100, height: 100 },
        label: "(a)".into(),
        body: PanelBody::Diagram(Diagram {
            nodes: Vec::new(),
            edges: Vec::new(),
            evidence_ids: Default::default(),
            node_origins: Default::default(),
        }),
        style: Style::default(),
    };
    let mut artifact = Artifact::new(
        ArtifactKind::Figure,
        "그림",
        "engine",
        ArtifactBody::Figure(FigureDocument { panels: vec![panel], ..FigureDocument::new(Size::a4()) }),
    );
    artifact.id = "a1".into();
    artifact.revision = 2;

    let mut harness = Harness::new();
    let (_, view_id) = harness.open_document("figure", 1);
    harness.app.artifact = Some(artifact.clone());
    harness.app.selected_node = Some("p1".into());
    harness.sent();

    // The edit is committed; the host answers the context read of the new revision with an empty
    // selection and reports the old one as stale.
    let (operations, intents) = edit();
    harness.app.apply_operations(operations, intents);
    let request = harness.sent();
    harness.answer(&request, "changes.apply", Ok(json!({ "revision": 2 })));
    let reload = harness.sent();
    let mut after = context("a1", &view_id, 2);
    after["selected_properties"]["stale_selection"] =
        json!({ "revision": 1, "selection": Selection { node_ids: vec!["p1".into()], text_range: None } });
    harness.answer(&reload, "context.read", Ok(after));
    assert_eq!(harness.app.selected_node, None, "the host's selection is empty until it is asked again");
    assert!(harness.sent().is_empty(), "the window waits for the new document first");

    harness.answer(&reload, "artifact.inspect", Ok(serde_json::to_value(&artifact).expect("artifact")));
    let again = harness.sent();
    assert_eq!(methods(&again), ["selection.set"]);
    assert_eq!(again[0].2["selection"]["node_ids"], json!(["p1"]));
    assert_eq!(again[0].2["expected_revision"], 2);
    assert_eq!(harness.app.selected_node.as_deref(), Some("p1"));

    // An element that is gone (an undone addition) is not asked for.
    let mut harness = Harness::new();
    let (_, view_id) = harness.open_document("figure", 1);
    let mut emptied = artifact.clone();
    if let ArtifactBody::Figure(figure) = &mut emptied.body {
        figure.panels.clear();
    }
    harness.app.reselect = Some(Selection { node_ids: vec!["p1".into()], text_range: None });
    harness.app.refresh_active();
    let reload = harness.sent();
    harness.answer(&reload, "context.read", Ok(context("a1", &view_id, 2)));
    harness.app.reselect = Some(Selection { node_ids: vec!["p1".into()], text_range: None });
    harness.answer(&reload, "artifact.inspect", Ok(serde_json::to_value(&emptied).expect("artifact")));
    assert!(harness.sent().is_empty());
    assert_eq!(harness.app.selected_node, None);
}

fn inferred_table(id: &str) -> designllm_contracts::Dataset {
    use designllm_contracts::{CellState, DataCell, DataColumn, DataRow, Dataset};
    let columns = [("group", "text"), ("value", "numeric")];
    Dataset {
        id: id.into(),
        version_hash: format!("{id}-hash"),
        original_asset_hash: "b".repeat(64),
        origin: "user_experimental".into(),
        columns: columns
            .iter()
            .map(|(name, data_type)| DataColumn {
                id: (*name).into(),
                name: (*name).into(),
                data_type: (*data_type).into(),
                unit: None,
            })
            .collect(),
        rows: vec![DataRow {
            id: "r1".into(),
            cells: columns
                .iter()
                .map(|(name, _)| {
                    ((*name).to_owned(), DataCell { original: "1".into(), parsed_decimal: Some("1".into()), state: CellState::Valid })
                })
                .collect(),
        }],
        derived_from: None,
        transforms: Vec::new(),
    }
}

/// A figure window whose chart form has read a table named `data-1`: the requests so far are drained.
fn reading_a_table() -> Harness {
    let mut harness = Harness::new();
    harness.open_document("figure", 1);
    harness.sent();
    harness.app.figure_form = Some(super::FigureForm::Chart);
    harness.app.csv_path = "C:/data/table.csv".into();
    harness.app.import_csv_source();
    let request = harness.sent();
    assert_eq!(methods(&request), ["asset.import"]);
    harness.answer(&request, "asset.import", Ok(json!({ "sha256": "b".repeat(64), "mime": "text/csv", "bytes": 40 })));
    let request = harness.sent();
    assert_eq!(methods(&request), ["source.import_csv"]);
    assert_eq!(request[0].2, json!({ "sha256": "b".repeat(64) }), "the first read infers the types");
    harness.answer(
        &request,
        "source.import_csv",
        Ok(serde_json::to_value(inferred_table("data-1")).expect("dataset")),
    );
    harness
}

#[test]
fn after_the_file_is_read_the_columns_are_declared_before_any_chart_is_described() {
    let harness = reading_a_table();
    let draft = harness.app.csv_draft.as_ref().expect("the table was read");
    assert_eq!(draft.step, super::state::CsvStep::Columns);
    assert_eq!(
        draft
            .columns
            .iter()
            .map(|column| (column.name.as_str(), column.data_type.as_str()))
            .collect::<Vec<_>>(),
        [("group", "text"), ("value", "numeric")],
        "the declaration starts from the inferred types"
    );
    assert!(harness.message().is_none());
}

#[test]
fn the_declared_table_replaces_the_inferred_one_and_starts_the_chart() {
    let mut harness = reading_a_table();
    {
        let draft = harness.app.csv_draft.as_mut().expect("draft");
        draft.columns[0].data_type = "category".into();
        draft.columns[1].unit = "mg/L".into();
    }
    // What the confirm button sends.
    let request = harness.app.csv_draft.as_ref().expect("draft").declaration_request();
    harness.app.call("source.import_csv", request, super::PendingAction::CsvDeclaredImport);
    let sent = harness.sent();
    assert_eq!(
        sent[0].2,
        json!({
            "sha256": "b".repeat(64),
            "columns": [
                { "name": "group", "data_type": "category" },
                { "name": "value", "data_type": "numeric", "unit": "mg/L" },
            ]
        })
    );
    let mut declared = inferred_table("data-1");
    declared.columns[0].data_type = "category".into();
    declared.columns[1].unit = Some("mg/L".into());
    harness.answer(&sent, "source.import_csv", Ok(serde_json::to_value(declared).expect("dataset")));

    let draft = harness.app.csv_draft.as_ref().expect("draft");
    assert_eq!(draft.step, super::state::CsvStep::Chart);
    assert_eq!((draft.x_column.as_str(), draft.y_column.as_str()), ("group", "value"));
    assert_eq!(draft.y_unit, "mg/L");
    assert!(harness.message().is_none());
}

#[test]
fn a_declaration_the_host_refuses_shows_the_hosts_reason_and_nothing_else() {
    let mut harness = reading_a_table();
    let request = harness.app.csv_draft.as_ref().expect("draft").declaration_request();
    harness.app.call("source.import_csv", request, super::PendingAction::CsvDeclaredImport);
    let sent = harness.sent();
    let reason = "column \"value\": a unit is allowed only on numeric columns";
    harness.answer(&sent, "source.import_csv", Err(DltError::invalid(reason)));
    assert_eq!(harness.message(), Some((Severity::Error, reason.to_owned())));
    assert_eq!(
        harness.app.csv_draft.as_ref().map(|draft| draft.step),
        Some(super::state::CsvStep::Columns),
        "the user stays on the declaration to correct it"
    );
}

#[test]
fn a_table_the_figure_already_holds_keeps_its_declared_types_and_skips_the_declaration() {
    use designllm_contracts::{Artifact, ArtifactBody, ArtifactKind, FigureDocument, Size};
    let mut held = inferred_table("data-1");
    held.columns[0].data_type = "category".into();
    let mut artifact = Artifact::new(
        ArtifactKind::Figure,
        "그림",
        "engine",
        ArtifactBody::Figure(FigureDocument { datasets: vec![held], ..FigureDocument::new(Size::a4()) }),
    );
    artifact.id = "a1".into();

    let mut harness = Harness::new();
    harness.open_document("figure", 1);
    harness.app.artifact = Some(artifact);
    harness.sent();
    harness.app.figure_form = Some(super::FigureForm::Chart);
    harness.app.csv_path = "C:/data/table.csv".into();
    harness.app.import_csv_source();
    let request = harness.sent();
    harness.answer(&request, "asset.import", Ok(json!({ "sha256": "b".repeat(64), "mime": "text/csv", "bytes": 40 })));
    let request = harness.sent();
    harness.answer(
        &request,
        "source.import_csv",
        Ok(serde_json::to_value(inferred_table("data-1")).expect("dataset")),
    );
    let draft = harness.app.csv_draft.as_ref().expect("draft");
    assert_eq!(draft.step, super::state::CsvStep::Chart, "the same table is not declared a second time");
    assert_eq!(draft.dataset.columns[0].data_type, "category", "the figure's declared types apply");
}

#[test]
fn the_column_declaration_fits_its_window_however_many_columns_the_file_has() {
    for scale in [1.0, 1.5] {
        let mut harness = reading_a_table();
        theme::install(&harness.ctx);
        {
            let draft = harness.app.csv_draft.as_mut().expect("draft");
            for index in 0..40 {
                draft.columns.push(super::state::ColumnDeclaration {
                    name: format!("a column with a rather long header name {index}"),
                    data_type: "numeric".into(),
                    unit: "mg/L".into(),
                });
            }
        }
        let mut area = None;
        for _ in 0..3 {
            let _ = harness.ctx.run(screen(scale, 800.0, 560.0), |ctx| {
                harness.app.draw_figure_form(ctx);
                area = ctx.memory(|memory| memory.area_rect(eframe::egui::Id::new("차트 추가")));
            });
        }
        let area = area.expect("the window was shown");
        let screen = Rect::from_min_size(Pos2::ZERO, Vec2::new(800.0, 560.0));
        assert!(screen.expand(1.0).contains_rect(area), "the window {area:?} leaves the screen (scale {scale})");
    }
}

/// Figures inside documents: what the window asks of the host, in what order, and what it shows
/// when the host or the document's engine says no.
mod figure_pictures {
    use designllm_contracts::{
        Artifact, ArtifactBody, ArtifactKind, AssetRef, Diagnostic, DltError, ErrorCode,
        FigureDocument, FigureLink, FlowBlock, FlowDocument, Node, NodeKind, Page, Paragraph,
        ParagraphFormat, Rect as EmuRect, Section, Size, SlideDocument, Style, TextRun, EMU_PER_MM,
    };
    use serde_json::{json, Value};

    use super::super::links::Blocked;
    use super::{context, drawn_text, methods, screen, summary, Harness, Request};
    use crate::{notice::Severity, theme};

    fn hash(digit: char) -> String {
        digit.to_string().repeat(64)
    }

    fn slide(revision: u64, pictures: Vec<Node>, links: Vec<FigureLink>) -> Artifact {
        let mut artifact = Artifact::new(
            ArtifactKind::Slides,
            "문서",
            "engine",
            ArtifactBody::Slides(SlideDocument {
                pages: vec![Page { nodes: pictures, ..Page::new("page-1", Size::slide()) }],
                styles: Default::default(),
                theme: None,
            }),
        );
        artifact.id = "a1".into();
        artifact.revision = revision;
        artifact.figure_links = links;
        artifact
    }

    fn picture(id: &str, asset_hash: &str) -> Node {
        Node {
            id: id.into(),
            kind: NodeKind::Image,
            bounds: EmuRect { x: 1_000_000, y: 1_000_000, width: 3_600_000, height: 2_400_000 },
            text: String::new(),
            style: Style::default(),
            asset: Some(AssetRef { sha256: asset_hash.into(), mime: "image/png".into(), bytes: 100 }),
            children: Vec::new(),
            layout: None,
            interactions: Vec::new(),
            variants: Default::default(),
            ext: Default::default(),
        }
    }

    fn paragraph(id: &str) -> FlowBlock {
        FlowBlock::Paragraph(Paragraph {
            id: id.into(),
            runs: vec![TextRun::new(format!("{id}-run"), id)],
            style_ref: None,
            format: ParagraphFormat::default(),
        })
    }

    /// An A4 document of one section with the paragraphs a, b and c.
    fn text_document(revision: u64) -> Artifact {
        let mut artifact = Artifact::new(
            ArtifactKind::Docx,
            "문서",
            "engine",
            ArtifactBody::Flow(FlowDocument {
                sections: vec![Section {
                    id: "s1".into(),
                    setup: Default::default(),
                    blocks: vec![paragraph("a"), paragraph("b"), paragraph("c")],
                }],
                styles: Default::default(),
            }),
        );
        artifact.id = "a1".into();
        artifact.revision = revision;
        artifact
    }

    fn figure(revision: u64, width_mm: i64, height_mm: i64) -> Value {
        let mut artifact = Artifact::new(
            ArtifactKind::Figure,
            "그림 1",
            "engine",
            ArtifactBody::Figure(FigureDocument::new(Size { width: width_mm * EMU_PER_MM, height: height_mm * EMU_PER_MM })),
        );
        artifact.id = "f1".into();
        artifact.revision = revision;
        serde_json::to_value(artifact).expect("figure")
    }

    fn rendered(revision: u64, render_hash: &str) -> Value {
        json!({
            "figure_id": "f1",
            "figure_revision": revision,
            "representation": "png",
            "asset": { "sha256": render_hash, "mime": "image/png", "bytes": 4096 },
            "render_hash": render_hash,
            "editability": "none",
            "diagnostics": [],
        })
    }

    /// The work list has `document` first and the figure "f1" (at `figure_revision`) after it, and
    /// the document is open. Returns the id of the window's view.
    fn open_with_figure(harness: &mut Harness, kind: &str, document: &Artifact, figure_revision: u64) -> String {
        let startup = harness.sent();
        harness.answer(
            &startup,
            "workspace.list",
            Ok(json!({
                "workspace_id": "w1",
                "artifacts": [
                    summary("a1", kind, document.revision),
                    { "id": "f1", "kind": "figure", "title": "그림 1", "revision": figure_revision, "engine_id": "engine", "dirty": false },
                ],
            })),
        );
        harness.app.invoke_ui("nav.open_project", json!({ "artifact_id": "a1" }));
        let opening = harness.sent();
        let view_id = opening
            .iter()
            .find(|request| request.1 == "context.read")
            .and_then(|request| request.2["view_id"].as_str())
            .expect("the context read names the view")
            .to_owned();
        harness.answer(&opening, "context.read", Ok(context("a1", &view_id, document.revision)));
        harness.answer(&opening, "artifact.inspect", Ok(serde_json::to_value(document).expect("document")));
        harness.sent();
        view_id
    }

    /// Answers the window's reads for the figure until the change set is on its way.
    fn up_to_the_change(harness: &mut Harness, figure: Value, render_hash: &str) -> Vec<Request> {
        let revision = figure["revision"].as_u64().expect("revision");
        let request = harness.sent();
        assert_eq!(methods(&request), ["artifact.inspect"], "the figure is read first");
        assert_eq!(request[0].2, json!({ "artifact_id": "f1" }));
        harness.answer(&request, "artifact.inspect", Ok(figure));
        let request = harness.sent();
        assert_eq!(methods(&request), ["figure.render_link"], "then rendered");
        assert_eq!(
            request[0].2,
            json!({ "figure_id": "f1", "figure_revision": revision, "representation": "png", "profile": { "dpi": 300, "transparent": false } })
        );
        harness.answer(&request, "figure.render_link", Ok(rendered(revision, render_hash)));
        harness.sent()
    }

    #[test]
    fn a_figure_goes_into_a_slide_in_three_calls_and_one_change_set_with_the_picture_first() {
        let mut harness = Harness::new();
        open_with_figure(&mut harness, "slides", &slide(4, Vec::new(), Vec::new()), 3);
        harness.app.begin_figure_insert("f1");
        let change = up_to_the_change(&mut harness, figure(3, 170, 120), &hash('a'));

        assert_eq!(methods(&change), ["changes.apply"], "picture and link are one change set");
        let set = &change[0].2;
        assert_eq!((set["artifact_id"].as_str(), set["base_revision"].as_u64()), (Some("a1"), Some(4)));
        assert_eq!(set["allowed_intents"], json!(["content"]));
        let operations = set["operations"].as_array().expect("operations");
        assert_eq!(
            operations.iter().map(|operation| operation["op"].as_str()).collect::<Vec<_>>(),
            [Some("add_node"), Some("update_figure_link")],
            "the picture comes first, its link right after"
        );
        assert_eq!((operations[0]["page_id"].as_str(), &operations[0]["parent_id"]), (Some("page-1"), &Value::Null));
        let node = &operations[0]["node"];
        assert_eq!(node["kind"], "image");
        assert_eq!(node["asset"]["sha256"], hash('a'), "the picture is the render");
        // The figure's own size fits the slide, so the picture keeps it and sits in the middle.
        assert_eq!(
            node["bounds"],
            json!({ "x": 3_036_000, "y": 1_269_000, "width": 6_120_000, "height": 4_320_000 })
        );
        assert_eq!(
            operations[1]["link"],
            json!({
                "figure_id": "f1",
                "figure_revision": 3,
                "target_node_id": node["id"],
                "representation": "png",
                "render_hash": hash('a'),
            })
        );
        assert_eq!(harness.message(), None);
    }

    #[test]
    fn a_figure_goes_into_a_text_document_after_the_selected_paragraph_no_wider_than_the_text() {
        let mut harness = Harness::new();
        let view_id = open_with_figure(&mut harness, "docx", &text_document(7), 3);
        let mut selected = context("a1", &view_id, 7);
        selected["selection"] = json!({ "node_ids": [], "text_range": { "paragraph_id": "b", "start": 0, "end": 0 } });
        harness.app.context = Some(serde_json::from_value(selected).expect("context"));

        harness.app.begin_figure_insert("f1");
        // A figure larger than the text area (170 mm on this page) comes in as wide as the text.
        let change = up_to_the_change(&mut harness, figure(3, 300, 150), &hash('a'));
        let operations = change[0].2["operations"].as_array().expect("operations").clone();
        assert_eq!(operations[0]["op"], "insert_flow_block");
        assert_eq!((operations[0]["section_id"].as_str(), operations[0]["index"].as_u64()), (Some("s1"), Some(2)));
        let block = &operations[0]["block"];
        assert_eq!(block["type"], "image");
        assert_eq!(
            block["value"]["bounds"],
            json!({ "x": 0, "y": 0, "width": 6_120_000, "height": 3_060_000 }),
            "170 mm wide with the figure's proportions"
        );
        assert_eq!(operations[1]["link"]["target_node_id"], block["value"]["id"]);
        assert_eq!(change[0].2["base_revision"], 7);
    }

    #[test]
    fn a_linked_picture_is_refreshed_with_the_link_alone_and_only_when_the_user_asks() {
        let mut harness = Harness::new();
        let link = FigureLink {
            figure_id: "f1".into(),
            figure_revision: 3,
            target_node_id: "pic-1".into(),
            representation: "png".into(),
            render_hash: hash('a'),
        };
        let document = slide(4, vec![picture("pic-1", &hash('a'))], vec![link]);
        // The figure is at revision 5 now; the picture still shows revision 3.
        open_with_figure(&mut harness, "slides", &document, 5);
        harness.app.selected_node = Some("pic-1".into());

        // Showing the offer, however often it is drawn, asks the host for nothing.
        theme::install(&harness.ctx);
        let mut shown = Vec::new();
        for _ in 0..3 {
            let output = harness.ctx.run(screen(1.0, 1280.0, 800.0), |ctx| {
                harness.app.draw_frame(ctx);
            });
            shown = drawn_text(&output);
        }
        let drawn = harness.sent();
        assert!(
            drawn.iter().all(|request| !matches!(request.1.as_str(), "changes.apply" | "figure.render_link" | "artifact.inspect")),
            "{:?}",
            methods(&drawn)
        );
        assert!(shown.iter().any(|text| text == "갱신 r3 → r5"), "the update names both revisions: {shown:?}");
        assert!(
            shown.iter().filter(|text| *text == "그림 1").count() >= 2,
            "the figure's title is in the work list and on the picture's inspector: {shown:?}"
        );
        // A slide lists figures in its insert menu (+ ▾); it is not blocked.
        assert!(harness.app.figure_insertion_blocked().is_none(), "the slide offers figures: {shown:?}");

        harness.app.begin_figure_update("pic-1");
        let change = up_to_the_change(&mut harness, figure(5, 170, 120), &hash('b'));
        assert_eq!(methods(&change), ["changes.apply"]);
        let set = &change[0].2;
        assert_eq!(set["base_revision"], 4);
        assert_eq!(
            set["operations"],
            json!([{
                "op": "update_figure_link",
                "link": {
                    "figure_id": "f1",
                    "figure_revision": 5,
                    "target_node_id": "pic-1",
                    "representation": "png",
                    "render_hash": hash('b'),
                },
            }]),
            "the same picture, the new revision and its render: nothing else"
        );
    }

    fn frame_text(harness: &mut Harness) -> Vec<String> {
        theme::install(&harness.ctx);
        let mut shown = Vec::new();
        for _ in 0..3 {
            let output = harness.ctx.run(screen(1.0, 1280.0, 800.0), |ctx| {
                harness.app.draw_frame(ctx);
            });
            shown = drawn_text(&output);
        }
        shown
    }

    #[test]
    fn a_picture_that_shows_the_current_revision_offers_no_update() {
        let mut harness = Harness::new();
        let link = FigureLink {
            figure_id: "f1".into(),
            figure_revision: 3,
            target_node_id: "pic-1".into(),
            representation: "png".into(),
            render_hash: hash('a'),
        };
        let document = slide(4, vec![picture("pic-1", &hash('a'))], vec![link]);
        open_with_figure(&mut harness, "slides", &document, 3);
        harness.app.selected_node = Some("pic-1".into());
        let shown = frame_text(&mut harness);
        assert!(shown.iter().filter(|text| *text == "그림 1").count() >= 2, "the title still opens the figure: {shown:?}");
        assert!(shown.iter().all(|text| !text.starts_with("갱신")), "nothing to update: {shown:?}");

        // A picture that is not a figure's has no figure row at all.
        let plain = slide(4, vec![picture("pic-1", &hash('a'))], Vec::new());
        let mut harness = Harness::new();
        open_with_figure(&mut harness, "slides", &plain, 5);
        harness.app.selected_node = Some("pic-1".into());
        let shown = frame_text(&mut harness);
        assert!(shown.iter().all(|text| text != "그림 1"), "a picture without a link is not named after a figure: {shown:?}");
        assert!(shown.iter().all(|text| !text.starts_with("갱신")));
    }

    #[test]
    fn a_picture_in_the_outline_carries_the_name_of_its_figure() {
        let mut harness = Harness::new();
        let link = FigureLink {
            figure_id: "f1".into(),
            figure_revision: 3,
            target_node_id: "pic-1".into(),
            representation: "png".into(),
            render_hash: hash('a'),
        };
        let document = slide(4, vec![picture("pic-1", &hash('a')), picture("pic-2", &hash('b'))], vec![link]);
        open_with_figure(&mut harness, "slides", &document, 3);
        let shown = frame_text(&mut harness);
        assert_eq!(
            shown.iter().filter(|text| *text == "그림 1").count(),
            1,
            "the layers tree names the linked picture; the other picture is just an image: {shown:?}"
        );
        assert_eq!(shown.iter().filter(|text| *text == "이미지").count(), 1, "{shown:?}");
    }

    #[test]
    fn a_picture_is_selected_once_the_document_that_holds_it_is_loaded() {
        let mut harness = Harness::new();
        let view_id = open_with_figure(&mut harness, "slides", &slide(4, Vec::new(), Vec::new()), 3);
        harness.app.begin_figure_insert("f1");
        let change = up_to_the_change(&mut harness, figure(3, 170, 120), &hash('a'));
        let picture_id = change[0].2["operations"][0]["node"]["id"].as_str().expect("id").to_owned();
        harness.answer(&change, "changes.apply", Ok(json!({ "revision": 5 })));

        let reload = harness.sent();
        assert_eq!(methods(&reload), super::reload_methods());
        harness.answer(&reload, "context.read", Ok(context("a1", &view_id, 5)));
        let link = FigureLink {
            figure_id: "f1".into(),
            figure_revision: 3,
            target_node_id: picture_id.clone(),
            representation: "png".into(),
            render_hash: hash('a'),
        };
        let document = slide(5, vec![picture(&picture_id, &hash('a'))], vec![link]);
        harness.answer(&reload, "artifact.inspect", Ok(serde_json::to_value(document).expect("document")));
        let selection = harness.sent();
        // The loaded slides also ask for their design warnings (lint.rs).
        assert_eq!(methods(&selection), ["selection.set", "artifact.validate"]);
        assert_eq!(selection[0].2["selection"]["node_ids"], json!([picture_id]));
        assert_eq!(selection[0].2["expected_revision"], 5);
        assert_eq!(harness.app.selected_node.as_deref(), Some(picture_id.as_str()));
        assert_eq!(harness.app.select_after_commit, None);
    }

    #[test]
    fn a_document_that_refuses_the_picture_says_why_in_its_own_words() {
        let mut harness = Harness::new();
        open_with_figure(&mut harness, "docx", &text_document(7), 3);
        harness.app.begin_figure_insert("f1");
        let change = up_to_the_change(&mut harness, figure(3, 170, 120), &hash('a'));
        let reason = "Inserting before the first paragraph of an HWP section (which holds the section definition) is not supported";
        harness.answer(&change, "changes.apply", Err(DltError::new(ErrorCode::UnsupportedOperation, reason)));
        assert_eq!(harness.message(), Some((Severity::Error, reason.to_owned())));
        assert_eq!(harness.app.select_after_commit, None, "nothing was inserted, so nothing is selected later");
        assert!(harness.sent().is_empty(), "a refusal changes nothing and reloads nothing");

        // Trying again is allowed at once.
        harness.app.begin_figure_insert("f1");
        assert_eq!(methods(&harness.sent()), ["artifact.inspect"]);
        assert_eq!(harness.message(), None);
    }

    #[test]
    fn a_figure_that_cannot_be_rendered_is_explained_by_its_problem_and_nothing_is_sent() {
        let mut harness = Harness::new();
        open_with_figure(&mut harness, "slides", &slide(4, Vec::new(), Vec::new()), 3);
        harness.app.begin_figure_insert("f1");
        let request = harness.sent();
        harness.answer(&request, "artifact.inspect", Ok(figure(3, 170, 120)));
        let request = harness.sent();
        let mut refusal = DltError::new(
            ErrorCode::ExportLossRequired,
            "figure render reports losses; choose another representation",
        );
        refusal.diagnostics = vec![Diagnostic {
            code: "LABEL_OVERFLOW".into(),
            message: "label does not fit".into(),
            node_id: None,
            blocking: true,
            params: Default::default(),
        }];
        harness.answer(&request, "figure.render_link", Err(refusal));
        assert_eq!(
            harness.message(),
            Some((Severity::Error, "글자가 도형 안에 들어가지 않습니다. 도형을 키우거나 글자를 줄여 주세요".to_owned()))
        );
        assert!(harness.sent().is_empty(), "no change set follows a failed render");
    }

    #[test]
    fn leaving_the_document_before_the_render_arrives_drops_the_change() {
        let mut harness = Harness::new();
        open_with_figure(&mut harness, "slides", &slide(4, Vec::new(), Vec::new()), 3);
        harness.app.begin_figure_insert("f1");
        let request = harness.sent();
        harness.answer(&request, "artifact.inspect", Ok(figure(3, 170, 120)));
        let render = harness.sent();
        assert_eq!(methods(&render), ["figure.render_link"]);

        let other = serde_json::from_value(summary("a2", "canvas", 1)).expect("summary");
        harness.app.activate(other);
        harness.sent();
        harness.answer(&render, "figure.render_link", Ok(rendered(3, &hash('a'))));
        assert!(
            harness.sent().iter().all(|request| request.1 != "changes.apply"),
            "the document the user asked for is no longer the one on screen"
        );
    }

    #[test]
    fn a_document_whose_engine_takes_no_figure_pictures_cannot_start_one() {
        let mut harness = Harness::new();
        let view_id = open_with_figure(&mut harness, "hwp", &text_document(7), 3);
        assert_eq!(harness.app.figure_insertion_blocked(), None);

        let mut refused = context("a1", &view_id, 7);
        refused["capabilities"]["figure_links"] = json!({
            "read": "supported", "render": "supported", "edit": "unsupported", "preserve": "supported",
            "write": "unsupported", "reason": "this document is read-only",
        });
        harness.app.context = Some(serde_json::from_value(refused).expect("context"));
        assert_eq!(harness.app.figure_insertion_blocked(), Some(Blocked::Unsupported));

        // A workspace with no figure has nothing to put in.
        let mut empty = Harness::new();
        empty.open_document("slides", 4);
        assert_eq!(empty.app.figure_insertion_blocked(), Some(Blocked::NoFigures));
    }

    #[test]
    fn a_page_that_is_not_there_gets_no_picture_and_says_so() {
        let mut harness = Harness::new();
        open_with_figure(&mut harness, "slides", &slide(4, Vec::new(), Vec::new()), 3);
        harness.app.page_index = 4;
        harness.app.begin_figure_insert("f1");
        assert_eq!(
            harness.message(),
            Some((Severity::Error, "그림을 넣을 위치를 찾지 못했습니다".to_owned()))
        );
        assert!(harness.sent().is_empty(), "nothing is asked of the host for a place that does not exist");
    }

    #[test]
    fn a_second_request_while_the_first_is_under_way_is_ignored() {
        let mut harness = Harness::new();
        open_with_figure(&mut harness, "slides", &slide(4, Vec::new(), Vec::new()), 3);
        harness.app.begin_figure_insert("f1");
        assert_eq!(harness.app.figure_insertion_blocked(), Some(Blocked::Busy), "the button waits for the answer");
        harness.app.begin_figure_insert("f1");
        assert_eq!(methods(&harness.sent()), ["artifact.inspect"]);
    }
}

/// The UI channel against the harness: a fake agent queues runs, the window executes them through
/// the same command path and completes them with its view token.
#[cfg(test)]
mod ui_channel_tests {
    use super::*;
    use eframe::egui::{Color32, ColorImage};
    use serde_json::{json, Value};
    use std::time::{Duration, Instant};

    /// An absolute local path on this platform for a file the test never writes (`C:/tmp/<name>` on Windows).
    fn local_test_path(name: &str) -> String {
        if cfg!(windows) { format!("C:/tmp/{name}") } else { format!("/tmp/{name}") }
    }

    /// Publishes the window's state now and answers with a token; returns the request sent.
    fn publish_with_guard(harness: &mut Harness, guard_version: Option<u32>) -> Value {
        harness.app.publish_ui_if_due();
        let sent = harness.sent();
        let request = sent.iter().find(|item| item.1 == "ui.state.publish").expect("the window publishes its state").clone();
        let view_id = request.2["view_id"].clone();
        let state_seq = harness.app.ui.state_seq.unwrap_or(0) + 1;
        assert_eq!(request.2["run_guard_version"], json!(designllm_contracts::UI_RUN_GUARD_VERSION));
        harness.answer(
            &sent,
            "ui.state.publish",
            Ok(json!({
                "view_id": view_id,
                "view_token": "tok-1",
                "state_seq": state_seq,
                "ttl_ms": 10000,
                "commands": 100,
                "run_guard_version": guard_version,
            })),
        );
        request.2
    }

    fn publish(harness: &mut Harness) -> Value {
        publish_with_guard(harness, Some(designllm_contracts::UI_RUN_GUARD_VERSION))
    }

    /// One run as the core hands it to the window (status delivered).
    fn run(run_id: &str, command: &str, args: Value, permission: &str) -> Value {
        json!({
            "run_id": run_id, "view_id": "view", "command_id": command, "args": args, "actor": "agent-x",
            "permission": permission, "allowed_intents": [], "status": "delivered", "timeout_ms": 15000,
        })
    }

    fn guarded_run(harness: &Harness, run_id: &str, command: &str, args: Value, permission: &str, allowed_intents: Value) -> Value {
        let state = harness.app.ui_state();
        json!({
            "run_id": run_id,
            "view_id": harness.app.ui.view_id,
            "command_id": command,
            "args": args,
            "actor": "agent-x",
            "permission": permission,
            "allowed_intents": allowed_intents,
            "status": "delivered",
            "timeout_ms": 15000,
            "expected": {
                "state_seq": harness.app.ui.state_seq.expect("the view was published"),
                "screen": state.screen,
                "artifact_id": state.artifact_id,
                "revision": state.revision,
                "selection": state.selection,
                "page_index": state.page_index,
            }
        })
    }

    /// The window polls for runs; the answer carries `runs` as `ui_runs`.
    fn deliver(harness: &mut Harness, runs: Vec<Value>) {
        harness.app.last_event_poll = Instant::now() - Duration::from_secs(10);
        harness.app.poll_events();
        let sent = harness.sent();
        let poll = sent.iter().find(|item| item.1 == "events.poll").expect("the window polls for runs").clone();
        assert_eq!(poll.2["view_token"], "tok-1", "the poll carries the view token");
        harness.answer(&sent, "events.poll", Ok(json!({ "events": [], "last_seq": 0, "resync_required": false, "artifacts": [], "ui_runs": runs })));
    }

    fn completions(harness: &mut Harness) -> Vec<Value> {
        harness.sent().into_iter().filter(|item| item.1 == "ui.command.complete").map(|item| item.2).collect()
    }

    fn set_canvas_list_context(harness: &mut Harness, edit: Option<&str>) {
        let mut value = context("a1", &harness.app.view_id, 1);
        if let Some(edit) = edit {
            value["capability_extensions"]["lists"] = json!({
                "read": "supported", "render": "supported", "edit": edit,
                "preserve": "supported", "write": "supported", "reason": "",
            });
        }
        harness.app.context = Some(serde_json::from_value(value).expect("context"));
    }

    fn canvas_text_node(id: &str, text: &str) -> designllm_contracts::Node {
        serde_json::from_value(json!({
            "id": id,
            "kind": "text",
            "text": text,
            "bounds": { "x": 952500, "y": 952500, "width": 2286000, "height": 762000 }
        }))
        .expect("Canvas text node")
    }

    fn text_artifact(node: designllm_contracts::Node) -> designllm_contracts::Artifact {
        use designllm_contracts::{Artifact, ArtifactBody, ArtifactKind, CanvasDocument, Page, Size};
        let mut artifact = Artifact::new(
            ArtifactKind::Canvas,
            "목록",
            "canvas.v1",
            ArtifactBody::Canvas(CanvasDocument {
                pages: vec![Page { nodes: vec![node], ..Page::new("page-1", Size::a4()) }],
                styles: Default::default(),
                ..Default::default()
            }),
        );
        artifact.id = "a1".into();
        artifact.revision = 1;
        artifact
    }

    fn flow_artifact() -> designllm_contracts::Artifact {
        use designllm_contracts::{Artifact, ArtifactBody, ArtifactKind, FlowBlock, FlowDocument, Paragraph, Section, TextRun};
        let mut artifact = Artifact::new(
            ArtifactKind::Docx,
            "문서",
            "docx.v1",
            ArtifactBody::Flow(FlowDocument {
                sections: vec![Section {
                    id: "section-1".into(),
                    setup: Default::default(),
                    blocks: vec![FlowBlock::Paragraph(Paragraph {
                        id: "paragraph-1".into(),
                        runs: vec![TextRun::new("run-1", "가나다라마바사")],
                        style_ref: None,
                        format: Default::default(),
                    })],
                }],
                styles: Default::default(),
            }),
        );
        artifact.id = "a1".into();
        artifact.revision = 1;
        artifact
    }

    fn set_flow_range(harness: &mut Harness, start: u32, end: u32) {
        let range = designllm_contracts::TextRange { paragraph_id: "paragraph-1".into(), start, end };
        harness.app.screen = super::super::Screen::Editor;
        harness.app.active = Some(serde_json::from_value(summary("a1", "docx", 1)).expect("Flow summary"));
        harness.app.artifact = Some(flow_artifact());
        let mut value = context("a1", &harness.app.view_id, 1);
        value["selection"] = json!({ "node_ids": [], "text_range": range.clone() });
        harness.app.context = Some(serde_json::from_value(value).expect("Flow context"));
        harness.app.local_selection = Some(designllm_contracts::Selection { node_ids: Vec::new(), text_range: Some(range) });
    }

    #[test]
    fn the_window_publishes_its_screen_and_registry_once_and_then_only_changes() {
        let mut harness = Harness::new();
        let params = publish(&mut harness);
        assert_eq!(params["state"]["screen"], "home");
        let listed = params["commands"].as_array().expect("the first publish sends the registry");
        assert!(listed.len() > 50);
        let unbuilt = listed.iter().find(|item| item["id"] == "web.refresh").expect("listed");
        assert_eq!(unbuilt["enabled"], false, "an unbuilt command is listed but not enabled");
        harness.app.publish_ui_if_due();
        assert!(harness.sent().iter().all(|item| item.1 != "ui.state.publish"), "nothing changed, nothing published");
        harness.app.view().left_open = false;
        harness.app.publish_ui_if_due();
        let sent = harness.sent();
        let again = sent.iter().find(|item| item.1 == "ui.state.publish").expect("the change is published");
        assert!(again.2["commands"].is_null(), "later publishes keep the registry");
        assert_eq!(again.2["view_token"], "tok-1");
        assert_eq!(again.2["state"]["panels"][0]["open"], false);
    }

    #[test]
    fn text_list_registry_tracks_live_canvas_capability_and_disables_slides() {
        let mut harness = Harness::new();
        harness.sent();
        harness.app.screen = super::super::Screen::Editor;
        harness.app.active = Some(serde_json::from_value(summary("a1", "canvas", 1)).expect("Canvas summary"));
        set_canvas_list_context(&mut harness, None);

        let first = publish(&mut harness);
        let listed = first["commands"].as_array().expect("initial registry");
        assert_eq!(listed.iter().find(|command| command["id"] == "text.list").unwrap()["enabled"], false);
        deliver(&mut harness, vec![json!({
            "run_id": "disabled-list-run",
            "view_id": "view",
            "command_id": "text.list",
            "args": {"list": "bullet"},
            "actor": "agent-list",
            "permission": "change",
            "allowed_intents": ["content"],
            "status": "delivered",
            "timeout_ms": 15000,
        })]);
        let refused = harness.sent();
        assert!(refused.iter().all(|request| request.1 != "changes.apply"), "a disabled command does not reach the document");
        assert!(refused.iter().find(|request| request.1 == "ui.command.complete").unwrap().2["error"].is_object());

        set_canvas_list_context(&mut harness, Some("supported"));
        harness.app.publish_ui_if_due();
        let sent = harness.sent();
        let publish = sent.iter().find(|request| request.1 == "ui.state.publish").expect("capability change republishes the registry");
        assert!(publish.2["commands"].is_array());
        assert_eq!(publish.2["commands"].as_array().unwrap().iter().find(|command| command["id"] == "text.list").unwrap()["enabled"], true);
        let view_id = publish.2["view_id"].clone();
        harness.answer(&sent, "ui.state.publish", Ok(json!({
            "view_id": view_id,
            "view_token": "tok-1",
            "state_seq": 2,
            "ttl_ms": 10000,
            "commands": 100,
            "run_guard_version": designllm_contracts::UI_RUN_GUARD_VERSION,
        })));

        harness.app.active = Some(serde_json::from_value(summary("a2", "slides", 1)).expect("Slides summary"));
        harness.app.publish_ui_if_due();
        let sent = harness.sent();
        let publish = sent.iter().find(|request| request.1 == "ui.state.publish").expect("artifact kind republishes the registry");
        assert_eq!(publish.2["commands"].as_array().unwrap().iter().find(|command| command["id"] == "text.list").unwrap()["enabled"], false);
        let view_id = publish.2["view_id"].clone();
        harness.answer(&sent, "ui.state.publish", Ok(json!({
            "view_id": view_id,
            "view_token": "tok-1",
            "state_seq": 3,
            "ttl_ms": 10000,
            "commands": 100,
            "run_guard_version": designllm_contracts::UI_RUN_GUARD_VERSION,
        })));
    }

    #[test]
    fn native_dialog_and_confirmation_commands_are_agent_disabled_but_remain_gui_enabled() {
        let mut harness = Harness::new();
        // The preview commands also need a WebView2 runtime (none on macOS and Linux); this test is about agent runs only.
        harness.app.web_runtime_unavailable = None;
        harness.sent();
        harness.app.screen = super::super::Screen::Editor;
        harness.app.active = Some(serde_json::from_value(summary("a1", "canvas", 1)).expect("Canvas summary"));
        let published = publish(&mut harness);
        let command_list = published["commands"].as_array().expect("registry");
        let disabled = [
            "add.image", "add.figure_image", "view.play", "web.preview", "file.save", "file.save_as",
            "file.export_selection", "file.dismiss_loss", "home.attach", "home.star", "home.runner",
        ];
        for id in disabled {
            assert_eq!(command_list.iter().find(|command| command["id"] == id).unwrap()["enabled"], false, "{id} is blocked for queued agent runs");
            assert!(harness.app.command_enabled_for_current_kind(id), "{id} remains available to direct GUI dispatch");
        }

        let export = command_list.iter().find(|command| command["id"] == "file.export").unwrap();
        assert_eq!(export["enabled"], true, "an explicit target keeps export available to queued runs");
        assert!(harness.app.command_enabled_for_current_kind("file.export"));

        deliver(
            &mut harness,
            vec![
                run("dialog-run", "nav.open_file", json!({}), "change"),
                run("attach-run", "home.attach", json!({ "path": "C:/private/source.docx" }), "read"),
                run("star-run", "home.star", json!({ "artifact_id": "a1" }), "read"),
                run("runner-run", "home.runner", json!({ "runtime": "codex", "model": "other", "effort": "low" }), "read"),
            ],
        );
        let sent = harness.sent();
        assert!(sent.iter().all(|request| request.1 != "artifact.open"), "an old queued dialog run never opens a picker");
        assert!(sent.iter().all(|request| !matches!(request.1.as_str(), "artifact.open" | "settings.save" | "artifact.export")), "no unsafe side effect is sent");
        let completions: Vec<_> = sent.iter().filter(|request| request.1 == "ui.command.complete").collect();
        assert_eq!(completions.len(), 4);
        for complete in completions {
            let refusal: DltError = serde_json::from_value(complete.2["error"].clone()).expect("a safe refusal");
            if complete.2["run_id"] == "dialog-run" {
                assert_eq!(refusal.code, ErrorCode::InvalidArgument);
                assert!(refusal.message.contains("path"));
            } else {
                assert_eq!(refusal.code, ErrorCode::UnsupportedOperation);
            }
        }
        assert!(harness.app.home.starred.is_empty(), "a queued star does not change local settings");
        assert!(harness.app.home.attachments.is_empty(), "a queued path is not placed in the later AI request");
        assert_eq!(harness.app.ai_runtime, "claude");
        assert_eq!(harness.app.ai_model, "");
    }

    #[test]
    fn text_list_ui_command_uses_the_same_content_change_path() {
        let mut harness = Harness::new();
        harness.sent();
        harness.app.screen = super::super::Screen::Editor;
        harness.app.active = Some(serde_json::from_value(summary("a1", "canvas", 1)).expect("Canvas summary"));
        set_canvas_list_context(&mut harness, Some("supported"));
        harness.app.artifact = Some(text_artifact(canvas_text_node("list-text", "")));
        harness.app.selected_node = Some("list-text".into());
        harness.app.selected_nodes = vec!["list-text".into()];
        let params = publish(&mut harness);
        let list_command = params["commands"].as_array().unwrap().iter().find(|command| command["id"] == "text.list").unwrap();
        assert_eq!(list_command["enabled"], true);
        assert_eq!(list_command["permission"], "change");
        assert_eq!(list_command["intents"], json!(["content"]));
        assert_eq!(list_command["args_schema"]["properties"]["list"]["enum"], json!(["bullet", "numbered", "none"]));

        let mut run = guarded_run(&harness, "run-list", "text.list", json!({ "list": "numbered" }), "change", json!(["content"]));
        run["actor"] = json!("agent-list");
        deliver(&mut harness, vec![run]);
        let sent = harness.sent();
        let change = sent.iter().find(|request| request.1 == "changes.apply").expect("same registered command sends a change set");
        assert_eq!(change.2["actor"], "agent-list");
        assert_eq!(change.2["allowed_intents"], json!(["content"]));
        assert_eq!(change.2["operations"], json!([{"op":"set_node_list","node_id":"list-text","list":"numbered"}]));
        assert!(sent.iter().all(|request| request.1 != "ui.command.complete"), "the run is not completed before changes.apply replies");
        harness.answer(&sent, "changes.apply", Ok(json!({ "revision": 2 })));
        let completed = harness.sent();
        let complete = completed.iter().find(|request| request.1 == "ui.command.complete").expect("the run completes after the apply reply");
        assert_eq!(complete.2["result"]["command"], "text.list");
        assert!(complete.2["error"].is_null());
    }

    #[test]
    fn live_text_range_is_published_and_a_queued_replace_refuses_a_different_range() {
        let mut harness = Harness::new();
        harness.sent();
        set_flow_range(&mut harness, 0, 2);
        let state = publish(&mut harness)["state"].clone();
        assert_eq!(state["selection"]["text_range"], json!({ "paragraph_id": "paragraph-1", "start": 0, "end": 2 }));
        let queued = guarded_run(&harness, "stale-text-range", "text.replace", json!({ "text": "바꿈" }), "change", json!(["content"]));

        set_flow_range(&mut harness, 3, 5);
        assert_eq!(harness.app.ui_state().selection.text_range.as_ref().unwrap().start, 3);
        deliver(&mut harness, vec![queued]);
        let sent = harness.sent();
        assert!(sent.iter().all(|request| request.1 != "changes.apply"), "the old text range does not reach the document");
        let complete = sent.iter().find(|request| request.1 == "ui.command.complete").expect("the stale run is completed");
        let refusal: DltError = serde_json::from_value(complete.2["error"].clone()).expect("a refusal");
        assert_eq!(refusal.code, ErrorCode::RevisionConflict);
    }

    #[test]
    fn a_text_range_run_refuses_while_selection_set_is_waiting_for_context_sync() {
        let mut harness = Harness::new();
        harness.sent();
        set_flow_range(&mut harness, 0, 2);
        publish(&mut harness);

        harness.app.set_selection(designllm_contracts::Selection {
            node_ids: Vec::new(),
            text_range: Some(designllm_contracts::TextRange { paragraph_id: "paragraph-1".into(), start: 3, end: 5 }),
        });
        let published = publish(&mut harness);
        assert_eq!(published["state"]["selection"]["text_range"]["start"], 3);
        assert!(harness.app.pending.values().any(|pending| pending.method == "selection.set"));
        let queued = guarded_run(&harness, "pending-text-range", "text.replace", json!({ "text": "바꿈" }), "change", json!(["content"]));

        deliver(&mut harness, vec![queued]);
        let sent = harness.sent();
        assert!(sent.iter().all(|request| request.1 != "changes.apply"), "the current Core range is not yet confirmed");
        let complete = sent.iter().find(|request| request.1 == "ui.command.complete").expect("the run is completed");
        let refusal: DltError = serde_json::from_value(complete.2["error"].clone()).expect("a refusal");
        assert_eq!(refusal.code, ErrorCode::RevisionConflict);
    }

    #[test]
    fn a_context_refresh_with_no_held_edit_keeps_the_current_refusal() {
        let mut harness = Harness::new();
        harness.app.screen = super::super::Screen::Editor;
        harness.app.active = Some(serde_json::from_value(summary("a1", "canvas", 1)).expect("summary"));
        harness.app.artifact = Some(text_artifact(canvas_text_node("n1", "text")));
        harness.app.context = Some(serde_json::from_value(context("a1", &harness.app.view_id, 1)).expect("context"));
        harness.app.reservations_for = Some("a1".into());
        harness.app.refusal = Some("최신 내용을 불러온 뒤 다시 시도해 주세요".into());
        harness.app.refusal_ids.clear();
        assert!(harness.app.held_edits.is_empty());

        harness.app.resume_held_edits();

        assert_eq!(harness.app.refusal.as_deref(), Some("최신 내용을 불러온 뒤 다시 시도해 주세요"));
    }

    #[test]
    fn a_background_lint_request_does_not_clear_a_user_refusal() {
        let mut harness = Harness::new();
        harness.sent();
        harness.app.set_error("최신 내용을 불러온 뒤 다시 시도해 주세요".into());
        harness.app.call(
            "artifact.validate",
            json!({ "artifact_id": "a1" }),
            super::super::PendingAction::LintWarnings { artifact_id: "a1".into() },
        );
        assert_eq!(harness.message().map(|(_, text)| text), Some("최신 내용을 불러온 뒤 다시 시도해 주세요".into()));
    }

    #[test]
    fn mutating_ui_runs_fail_closed_without_guard_negotiation_or_expected_state() {
        for (guard_version, omit_expected) in [(None, false), (Some(designllm_contracts::UI_RUN_GUARD_VERSION), true)] {
            let mut harness = Harness::new();
            harness.sent();
            harness.app.screen = super::super::Screen::Editor;
            harness.app.active = Some(serde_json::from_value(summary("a1", "canvas", 1)).expect("Canvas summary"));
            set_canvas_list_context(&mut harness, Some("supported"));
            harness.app.artifact = Some(text_artifact(canvas_text_node("list-text", "")));
            harness.app.selected_node = Some("list-text".into());
            harness.app.selected_nodes = vec!["list-text".into()];
            publish_with_guard(&mut harness, guard_version);

            let mut queued = guarded_run(&harness, "unguarded-list", "text.list", json!({ "list": "numbered" }), "change", json!(["content"]));
            if omit_expected {
                queued.as_object_mut().unwrap().remove("expected");
            }
            deliver(&mut harness, vec![queued]);
            let sent = harness.sent();
            assert!(sent.iter().all(|request| request.1 != "changes.apply"), "mutating runs without a matching guard cannot edit");
            let complete = sent.iter().find(|request| request.1 == "ui.command.complete").expect("the run is completed");
            let refusal: DltError = serde_json::from_value(complete.2["error"].clone()).expect("a refusal");
            assert_eq!(refusal.code, ErrorCode::RevisionConflict);
        }
    }

    #[test]
    fn canvas_text_list_properties_are_gated_by_the_live_extension() {
        fn draw(harness: &mut Harness) -> Vec<String> {
            let ctx = harness.ctx.clone();
            theme::install(&ctx);
            let output = ctx.run(screen(1.0, 800.0, 560.0), |ctx| {
                let palette = theme::palette(ctx);
                eframe::egui::CentralPanel::default().show(ctx, |ui| harness.app.draw_properties(ui, &palette));
            });
            drawn_text(&output)
        }

        let mut harness = Harness::new();
        harness.app.screen = super::super::Screen::Editor;
        harness.app.active = Some(serde_json::from_value(summary("a1", "canvas", 1)).expect("Canvas summary"));
        harness.app.artifact = Some(text_artifact(canvas_text_node("list-text", "")));
        harness.app.selected_node = Some("list-text".into());
        harness.app.selected_nodes = vec!["list-text".into()];
        set_canvas_list_context(&mut harness, None);
        assert!(!draw(&mut harness).iter().any(|text| text == "목록"), "unsupported list controls stay hidden");

        set_canvas_list_context(&mut harness, Some("supported"));
        let shown = draw(&mut harness);
        assert!(shown.iter().any(|text| text == "목록"), "the Canvas text inspector shows the list control");
        assert!(shown.iter().any(|text| text == "없음"), "an unlisted text node displays the clear state");
    }

    #[test]
    fn canvas_table_width_inspector_commits_one_design_change_on_enter() {
        use designllm_contracts::{Artifact, ArtifactBody, ArtifactKind, CanvasDocument, CanvasTable, CanvasTableRow, LayoutSpec, Node, NodeExt, NodeKind, Page, Rect, Size, Style};
        use eframe::egui::{Event, Id, Key, Modifiers};

        let mut harness = Harness::new();
        let (_, view_id) = harness.open_document("canvas", 1);
        let mut context_value = context("a1", &view_id, 1);
        context_value["capabilities"]["tables"] = supported();
        context_value["selection"]["node_ids"] = json!(["table"]);
        harness.app.context = Some(serde_json::from_value(context_value).expect("table context"));
        let cell = |id: &str| Node {
            id: id.into(),
            kind: NodeKind::Rectangle,
            bounds: Rect { x: 0, y: 0, width: 120 * 9525, height: 36 * 9525 },
            text: String::new(),
            style: Style::default(),
            asset: None,
            children: Vec::new(),
            layout: Some(LayoutSpec { width_mode: "fill".into(), height_mode: "hug".into(), ..Default::default() }),
            interactions: Vec::new(),
            variants: Default::default(),
            ext: NodeExt::default(),
        };
        let table = Node {
            id: "table".into(),
            kind: NodeKind::Group,
            bounds: Rect { x: 0, y: 0, width: 240 * 9525, height: 80 * 9525 },
            text: String::new(),
            style: Style::default(),
            asset: None,
            children: vec![cell("c1"), cell("c2")],
            layout: Some(LayoutSpec { mode: "grid".into(), grid_columns: Some(2), ..Default::default() }),
            interactions: Vec::new(),
            variants: Default::default(),
            ext: NodeExt {
                table: Some(CanvasTable {
                    rows: vec![CanvasTableRow { id: "row".into(), cell_ids: vec!["c1".into(), "c2".into()] }],
                    header_row_id: None,
                    column_widths: None,
                    cell_spans: Default::default(),
                }),
                ..Default::default()
            },
        };
        let mut artifact = Artifact::new(
            ArtifactKind::Canvas,
            "표",
            "canvas.v1",
            ArtifactBody::Canvas(CanvasDocument {
                pages: vec![Page { nodes: vec![table], ..Page::new("page-1", Size::a4()) }],
                ..Default::default()
            }),
        );
        artifact.id = "a1".into();
        artifact.revision = 1;
        harness.app.artifact = Some(artifact);
        harness.app.selected_node = Some("table".into());
        harness.app.selected_nodes = vec!["table".into()];
        assert!(harness.app.selected_canvas_table_node().is_some(), "the selected model carries typed table metadata");

        let draw = |harness: &mut Harness, input: eframe::egui::RawInput| {
            let ctx = harness.ctx.clone();
            ctx.run(input, |ctx| {
                let palette = theme::palette(ctx);
                eframe::egui::CentralPanel::default().show(ctx, |ui| harness.app.draw_properties(ui, &palette));
            })
        };
        theme::install(&harness.ctx);
        let initial = draw(&mut harness, screen(1.0, 800.0, 560.0));
        assert!(drawn_text(&initial).iter().any(|text| text == "열 비율"), "table controls text: {:?}", drawn_text(&initial));
        let width_id = Id::new(("canvas-table-width", "table", 0));
        harness.ctx.memory_mut(|memory| memory.request_focus(width_id));
        let mut typing = screen(1.0, 800.0, 560.0);
        typing.events.extend([
            Event::Key { key: Key::A, physical_key: Some(Key::A), pressed: true, repeat: false, modifiers: Modifiers::COMMAND },
            Event::Text("3".into()),
        ]);
        let _ = draw(&mut harness, typing);
        assert!(harness.sent().iter().all(|request| request.1 != "changes.apply"), "typing alone does not apply the ratio");

        let mut enter = screen(1.0, 800.0, 560.0);
        enter.events.push(Event::Key {
            key: Key::Enter, physical_key: Some(Key::Enter), pressed: true, repeat: false, modifiers: Modifiers::default(),
        });
        let _ = draw(&mut harness, enter);
        let request = harness.sent().into_iter().find(|request| request.1 == "changes.apply").expect("Enter applies the table width");
        assert_eq!(request.2["allowed_intents"], json!(["design"]));
        assert_eq!(request.2["operations"][0]["op"], "set_canvas_table_column_widths");
        assert_eq!(request.2["operations"][0]["column_widths"], json!([3, 1]));
    }

    fn table_span_test_node(table_id: &str, prefix: &str) -> Node {
        use designllm_contracts::{CanvasTable, CanvasTableRow, LayoutSpec, NodeExt, NodeKind, Rect, Style};

        let cell_id = |row: usize, column: usize| format!("{prefix}{}{}", row + 1, column + 1);
        let cell = |id: String| Node {
            id,
            kind: NodeKind::Rectangle,
            bounds: Rect {
                x: 0,
                y: 0,
                width: 120 * designllm_contracts::EMU_PER_CSS_PX,
                height: 36 * designllm_contracts::EMU_PER_CSS_PX,
            },
            text: String::new(),
            style: Style::default(),
            asset: None,
            children: Vec::new(),
            layout: Some(LayoutSpec { width_mode: "fill".into(), height_mode: "hug".into(), ..Default::default() }),
            interactions: Vec::new(),
            variants: Default::default(),
            ext: NodeExt::default(),
        };
        let rows = vec![
            CanvasTableRow { id: format!("{table_id}-row-1"), cell_ids: vec![cell_id(0, 0), cell_id(0, 1)] },
            CanvasTableRow { id: format!("{table_id}-row-2"), cell_ids: vec![cell_id(1, 0), cell_id(1, 1)] },
        ];
        let children = rows.iter().flat_map(|row| row.cell_ids.iter().cloned()).map(cell).collect();
        Node {
            id: table_id.into(),
            kind: NodeKind::Group,
            bounds: Rect {
                x: 0,
                y: 0,
                width: 240 * designllm_contracts::EMU_PER_CSS_PX,
                height: 72 * designllm_contracts::EMU_PER_CSS_PX,
            },
            text: String::new(),
            style: Style::default(),
            asset: None,
            children,
            layout: Some(LayoutSpec {
                mode: "grid".into(),
                height_mode: "hug".into(),
                grid_columns: Some(2),
                gap: 0,
                row_gap: Some(0),
                ..Default::default()
            }),
            interactions: Vec::new(),
            variants: Default::default(),
            ext: NodeExt {
                table: Some(CanvasTable { rows, header_row_id: None, column_widths: None, cell_spans: Default::default() }),
                ..NodeExt::default()
            },
        }
    }

    fn table_span_test_artifact(two_tables: bool) -> Artifact {
        let mut nodes = vec![table_span_test_node("table", "c")];
        if two_tables { nodes.push(table_span_test_node("table-2", "x")); }
        let mut artifact = Artifact::new(
            ArtifactKind::Canvas,
            "표 병합 테스트",
            "canvas.v1",
            ArtifactBody::Canvas(CanvasDocument {
                pages: vec![designllm_contracts::Page { nodes, ..designllm_contracts::Page::new("page-1", designllm_contracts::Size::slide()) }],
                ..Default::default()
            }),
        );
        artifact.id = "a1".into();
        artifact.revision = 1;
        artifact
    }

    fn table_span_harness(artifact: Artifact) -> Harness {
        let mut harness = Harness::new();
        theme::install(&harness.ctx);
        let (_, view_id) = harness.open_document("canvas", 1);
        let mut snapshot = context("a1", &view_id, 1);
        snapshot["capabilities"]["tables"] = supported();
        harness.app.context = Some(serde_json::from_value(snapshot).expect("table context"));
        harness.app.artifact = Some(artifact);
        harness
    }

    fn table_span_table_mut<'a>(artifact: &'a mut Artifact, table_id: &str) -> &'a mut Node {
        let ArtifactBody::Canvas(document) = &mut artifact.body else { panic!("Canvas body") };
        document.pages[0].nodes.iter_mut().find(|node| node.id == table_id).expect("table")
    }

    #[test]
    fn canvas_table_span_planner_uses_rectangular_selection_and_one_table() {
        use designllm_contracts::{CanvasTableSpan, Operation};

        let mut harness = table_span_harness(table_span_test_artifact(false));
        harness.app.selected_node = Some("c12".into());
        harness.app.selected_nodes = vec!["c22".into(), "c11".into(), "c12".into(), "c21".into()];
        let (unmerge, operation) = harness.app.canvas_table_span_operation().expect("a rectangular 2×2 selection can merge");
        assert!(!unmerge);
        let Operation::SetCanvasTableStructure { table_id, expected, table } = operation else { panic!("table structure operation") };
        assert_eq!(table_id, "table");
        assert!(expected.cell_spans.is_empty());
        assert_eq!(table.cell_spans.get("c11"), Some(&CanvasTableSpan { row_span: 2, column_span: 2 }), "top-left is the anchor regardless of selection order");
        assert_eq!(table.rows, expected.rows, "merging retains dense row and cell IDs");

        harness.app.selected_node = Some("c21".into());
        harness.app.selected_nodes = vec!["c11".into(), "c12".into(), "c21".into()];
        assert!(harness.app.canvas_table_span_operation().is_none(), "a non-rectangular selection is refused");

        harness.app.artifact = Some(table_span_test_artifact(true));
        harness.app.selected_node = Some("c11".into());
        harness.app.selected_nodes = vec!["c11".into(), "x11".into()];
        assert!(harness.app.canvas_table_span_operation().is_none(), "selection across two tables is refused");
    }

    #[test]
    fn table_merge_commands_refuse_a_single_cell_an_outside_region_an_overlap_and_an_unmerged_cell() {
        use designllm_contracts::CanvasTableSpan;

        let args = |row: usize, column: usize, rows: usize, columns: usize| {
            json!({ "table_id": "table", "row": row, "column": column, "rows": rows, "columns": columns })
        };
        let mut harness = table_span_harness(table_span_test_artifact(false));
        harness.app.selected_node = Some("table".into());
        harness.app.selected_nodes = vec!["table".into()];
        let single = harness.app.table_merge_cells(&args(0, 0, 1, 1));
        assert!(single.err().is_some_and(|message| message.contains("두 칸")), "a single cell is not a merge");
        let outside = harness.app.table_merge_cells(&args(0, 0, 100, 1));
        assert!(outside.err().is_some_and(|message| message.contains("표 밖")), "a region past the grid is refused");
        let unmerged = harness.app.table_unmerge_cell(&json!({ "table_id": "table", "row": 0, "column": 0 }));
        assert!(unmerged.err().is_some_and(|message| message.contains("병합된 칸이 아닙니다")), "an unmerged cell has nothing to unmerge");

        let mut artifact = table_span_test_artifact(false);
        table_span_table_mut(&mut artifact, "table").ext.table.as_mut().unwrap().cell_spans.insert(
            "c11".into(), CanvasTableSpan { row_span: 1, column_span: 2 },
        );
        let mut merged = table_span_harness(artifact);
        merged.app.selected_node = Some("table".into());
        merged.app.selected_nodes = vec!["table".into()];
        let overlap = merged.app.table_merge_cells(&args(0, 0, 1, 2));
        assert!(overlap.err().is_some_and(|message| message.contains("걸친")), "a region over a merged cell is refused");
    }

    #[test]
    fn table_merge_refuses_a_covered_cell_with_a_different_style_and_says_why() {
        let mut artifact = table_span_test_artifact(false);
        table_span_table_mut(&mut artifact, "table").children[1].style.color = "#CC0000".into();
        let mut harness = table_span_harness(artifact);
        harness.app.selected_node = Some("table".into());
        harness.app.selected_nodes = vec!["table".into()];
        let args = json!({ "table_id": "table", "row": 0, "column": 0, "rows": 1, "columns": 2 });
        harness.app.run_span_action(Ok(("table.merge_cells", args)));
        let notice = harness.app.notices.current(std::time::Instant::now()).expect("the refusal is shown");
        assert_eq!(notice.severity, crate::notice::Severity::Error);
        assert!(notice.text.contains("서식"), "a covered style difference is refused with its reason: {}", notice.text);
    }

    /// The core's other hidden-cell rules are checked before the change is sent, each with its reason.
    #[test]
    fn table_merge_refuses_a_covered_cell_layout_height_or_interaction_target_and_bad_arguments_by_name() {
        let args = json!({ "table_id": "table", "row": 0, "column": 0, "rows": 1, "columns": 2 });
        let refusal = |change: &dyn Fn(&mut designllm_contracts::Node)| {
            let mut artifact = table_span_test_artifact(false);
            change(table_span_table_mut(&mut artifact, "table"));
            let mut harness = table_span_harness(artifact);
            harness.app.table_merge_cells(&args).err().unwrap_or_default()
        };
        let padded = refusal(&|table| table.children[1].layout.as_mut().unwrap().padding = 9525);
        assert!(padded.contains("배치"), "TABLE_SPAN_HIDDEN_LAYOUT: {padded}");
        let taller = refusal(&|table| table.children[1].bounds.height += 9525);
        assert!(taller.contains("높이"), "TABLE_SPAN_HIDDEN_GEOMETRY: {taller}");
        let targeted = refusal(&|table| {
            let covered = table.children[1].id.clone();
            table.children[0].interactions.push(designllm_contracts::Interaction::ToggleVisibility { node_id: covered });
        });
        assert!(targeted.contains("상호작용"), "TABLE_SPAN_INTERACTION_TARGET: {targeted}");

        let mut harness = table_span_harness(table_span_test_artifact(false));
        let negative = harness.app.table_merge_cells(&json!({ "table_id": "table", "row": -1, "column": 0, "rows": 1, "columns": 2 }));
        assert_eq!(negative.err().as_deref(), Some("row는 0 이상의 정수여야 합니다"));
        let fraction = harness.app.table_merge_cells(&json!({ "table_id": "table", "row": 0, "column": 0, "rows": 1.5, "columns": 2 }));
        assert_eq!(fraction.err().as_deref(), Some("rows는 1 이상의 정수여야 합니다"));
        let missing = harness.app.table_unmerge_cell(&json!({ "table_id": "table", "row": 0 }));
        assert_eq!(missing.err().as_deref(), Some("column 값을 지정하세요"));
    }

    #[test]
    fn canvas_table_span_planner_rejects_covered_cells_locks_and_stale_context() {
        use designllm_contracts::CanvasTableSpan;

        let mut artifact = table_span_test_artifact(false);
        table_span_table_mut(&mut artifact, "table").ext.table.as_mut().unwrap().cell_spans.insert(
            "c11".into(), CanvasTableSpan { row_span: 1, column_span: 2 },
        );
        let mut harness = table_span_harness(artifact);
        harness.app.selected_node = Some("c12".into());
        harness.app.selected_nodes = vec!["c12".into()];
        assert!(harness.app.canvas_table_span_operation().is_none(), "a covered cell cannot be used as an anchor or unmerge target");
        harness.app.selected_node = Some("c11".into());
        harness.app.selected_nodes = vec!["c11".into(), "c21".into()];
        assert!(harness.app.canvas_table_span_operation().is_none(), "a merge intersecting an existing span is refused");

        let mut locked = table_span_test_artifact(false);
        table_span_table_mut(&mut locked, "table").children[2].ext.locked = Some(true);
        table_span_table_mut(&mut locked, "table").ext.locked = Some(false);
        let mut locked_harness = table_span_harness(locked);
        locked_harness.app.selected_node = Some("c11".into());
        locked_harness.app.selected_nodes = vec!["c11".into(), "c12".into()];
        assert!(locked_harness.app.canvas_table_span_operation().is_none(), "one locked table cell blocks a structure edit");

        table_span_table_mut(locked_harness.app.artifact.as_mut().unwrap(), "table").ext.locked = Some(true);
        assert!(locked_harness.app.canvas_table_span_operation().is_none(), "a locked table blocks a structure edit");

        let mut stale = table_span_harness(table_span_test_artifact(false));
        stale.app.selected_node = Some("c11".into());
        stale.app.selected_nodes = vec!["c11".into(), "c12".into()];
        stale.app.context.as_mut().unwrap().revision += 1;
        assert!(stale.app.canvas_table_span_operation().is_none(), "a stale revision disables the planner");
    }

    #[test]
    fn canvas_table_span_planner_unmerges_only_the_anchor_and_preserves_cell_data() {
        use designllm_contracts::{CanvasTableSpan, Operation};

        let mut artifact = table_span_test_artifact(false);
        let table_node = table_span_table_mut(&mut artifact, "table");
        let cell_style = designllm_contracts::Style { fill: "#AABBCC".into(), ..Default::default() };
        table_node.children[0].text = "anchor content".into();
        table_node.children[0].style = cell_style.clone();
        table_node.children[1].style = cell_style;
        table_node.ext.table.as_mut().unwrap().cell_spans.insert(
            "c11".into(), CanvasTableSpan { row_span: 1, column_span: 2 },
        );
        let original_rows = table_node.ext.table.as_ref().unwrap().rows.clone();
        let original_cells = serde_json::to_value(&table_node.children).expect("cell snapshot");
        let mut harness = table_span_harness(artifact);
        harness.app.selected_node = Some("c11".into());
        harness.app.selected_nodes = vec!["c11".into()];

        let (unmerge, operation) = harness.app.canvas_table_span_operation().expect("the anchor exposes unmerge");
        assert!(unmerge);
        let Operation::SetCanvasTableStructure { table_id, expected, table } = operation else { panic!("table structure operation") };
        assert_eq!(table_id, "table");
        assert!(expected.cell_spans.contains_key("c11"));
        assert!(table.cell_spans.is_empty());
        assert_eq!(table.rows, original_rows);
        let ArtifactBody::Canvas(document) = &harness.app.artifact.as_ref().unwrap().body else { panic!("Canvas body") };
        let cells_after = serde_json::to_value(&document.pages[0].nodes[0].children).expect("cells after unmerge planning");
        assert_eq!(cells_after, original_cells, "unmerge planning does not discard cell text or style");
    }

    #[test]
    fn canvas_table_merge_button_sends_same_style_populated_cells() {
        let mut artifact = table_span_test_artifact(false);
        let table_node = table_span_table_mut(&mut artifact, "table");
        table_node.children[0].text = "visible anchor text".into();
        table_node.children[1].text = "covered plain text".into();
        // The ordinary table cells share the same default style, layout and non-text fields.
        let mut harness = table_span_harness(artifact);
        harness.app.selected_node = Some("c11".into());
        harness.app.selected_nodes = vec!["c11".into(), "c12".into()];
        let output = draw_properties_offscreen(&mut harness, screen(1.0, 1000.0, 1000.0));
        assert!(drawn_text(&output).iter().any(|text| text == "병합"), "the valid rectangular cell selection shows merge");
        let position = find_text_center(&output, "병합");
        let _ = draw_properties_offscreen(&mut harness, click_at(position));
        let requests = harness.sent();
        let change = requests.iter().find(|request| request.1 == "changes.apply").expect("the merge button sends a local ChangeSet");
        assert_eq!(change.2["base_revision"], 1);
        assert_eq!(change.2["allowed_intents"], json!(["content"]));
        // The covered text moves to the anchor first, then the covered cell is emptied, then the structure change (one change).
        assert_eq!(change.2["operations"][0]["op"], "set_text");
        assert_eq!(change.2["operations"][0]["node_id"], "c11");
        assert_eq!(change.2["operations"][0]["text"], "visible anchor text\ncovered plain text");
        assert_eq!(change.2["operations"][1]["op"], "set_text");
        assert_eq!(change.2["operations"][1]["node_id"], "c12");
        assert_eq!(change.2["operations"][1]["text"], "");
        let structure = change.2["operations"].as_array().unwrap().iter().find(|op| op["op"] == "set_canvas_table_structure").expect("the structure change is in the same ChangeSet");
        assert_eq!(structure["table_id"], "table");
        assert_eq!(structure["table"]["cell_spans"]["c11"], json!({ "row_span": 1, "column_span": 2 }));
        assert!(requests.iter().all(|request| !request.1.starts_with("ui.command.")), "the local inspector does not dispatch an MCP/UI command");
        let ArtifactBody::Canvas(document) = &harness.app.artifact.as_ref().unwrap().body else { panic!("Canvas body") };
        assert_eq!(document.pages[0].nodes[0].children[0].text, "visible anchor text");
        assert_eq!(document.pages[0].nodes[0].children[1].text, "covered plain text", "planning a merge never consumes source cell text");
    }

    #[test]
    fn canvas_table_merge_control_hides_single_nonrect_covered_locked_and_stale_targets() {
        let visible = |harness: &mut Harness| {
            let output = draw_properties_offscreen(harness, screen(1.0, 1000.0, 1000.0));
            drawn_text(&output).iter().any(|text| text == "병합" || text == "병합 해제")
        };

        let mut single = table_span_harness(table_span_test_artifact(false));
        single.app.selected_node = Some("c11".into());
        single.app.selected_nodes = vec!["c11".into()];
        assert!(!visible(&mut single), "one unmerged cell does not show a merge action");

        let mut nonrect = table_span_harness(table_span_test_artifact(false));
        nonrect.app.selected_node = Some("c21".into());
        nonrect.app.selected_nodes = vec!["c11".into(), "c12".into(), "c21".into()];
        assert!(!visible(&mut nonrect), "a non-rectangular selection has no merge action");

        let mut two_tables = table_span_harness(table_span_test_artifact(true));
        two_tables.app.selected_node = Some("c11".into());
        two_tables.app.selected_nodes = vec!["c11".into(), "x11".into()];
        assert!(!visible(&mut two_tables), "a selection across tables has no merge action");

        let mut covered_artifact = table_span_test_artifact(false);
        table_span_table_mut(&mut covered_artifact, "table").ext.table.as_mut().unwrap().cell_spans.insert(
            "c11".into(), designllm_contracts::CanvasTableSpan { row_span: 1, column_span: 2 },
        );
        let mut covered = table_span_harness(covered_artifact);
        covered.app.selected_node = Some("c12".into());
        covered.app.selected_nodes = vec!["c12".into()];
        assert!(!visible(&mut covered), "a covered cell has no merge or unmerge action");

        let mut crossing_artifact = table_span_test_artifact(false);
        table_span_table_mut(&mut crossing_artifact, "table").ext.table.as_mut().unwrap().cell_spans.insert(
            "c11".into(), designllm_contracts::CanvasTableSpan { row_span: 1, column_span: 2 },
        );
        let mut crossing = table_span_harness(crossing_artifact);
        crossing.app.selected_node = Some("c11".into());
        crossing.app.selected_nodes = vec!["c11".into(), "c21".into()];
        assert!(!visible(&mut crossing), "a selection intersecting an existing span has no merge action");

        let mut locked_artifact = table_span_test_artifact(false);
        table_span_table_mut(&mut locked_artifact, "table").children[1].ext.locked = Some(true);
        let mut locked = table_span_harness(locked_artifact);
        locked.app.selected_node = Some("c11".into());
        locked.app.selected_nodes = vec!["c11".into(), "c12".into()];
        assert!(!visible(&mut locked), "a locked table cell hides the merge action");

        let mut stale = table_span_harness(table_span_test_artifact(false));
        stale.app.selected_node = Some("c11".into());
        stale.app.selected_nodes = vec!["c11".into(), "c12".into()];
        stale.app.context.as_mut().unwrap().revision += 1;
        assert!(!visible(&mut stale), "a stale table context hides the merge action");
    }

    /// A refused `canvas.table.add` takes the refusal branch of a change set (backlog desktop 30): the callout says why, nothing
    /// is selected for a table that was not made, and the ticket an agent's run would have kept is forgotten.
    #[test]
    fn a_refused_canvas_table_add_shows_the_refusal_and_keeps_no_selection_to_apply() {
        let mut harness = table_span_harness(table_span_test_artifact(false));
        // The size limit's words name the engine's column limit (backlog desktop 48).
        let too_big = harness.app.invoke("canvas.table.add", json!({ "rows": 500, "columns": 21 })).expect_err("10,500 cells");
        assert!(too_big.contains(&format!("1–{}개의 열", designllm_contracts::MAX_GRID_COLUMNS)) && too_big.contains("10,000"), "{too_big}");
        harness.app.invoke("canvas.table.add", json!({ "rows": 2, "columns": 2 })).expect("the table is sent");
        let requests = harness.sent();
        let ticket = requests.iter().rev().find(|request| request.1 == "changes.apply").expect("one change set").0;
        harness.app.ui.remember_agent_commit(ticket);
        let selected = harness.app.selected_nodes.clone();
        let refused = designllm_contracts::DltError::new(designllm_contracts::ErrorCode::RevisionConflict, "the canvas moved");
        harness.answer(&requests, "changes.apply", Err(refused));
        assert!(harness.app.refusal.is_some(), "the callout says why");
        assert_eq!(harness.app.selected_nodes, selected, "the selection stays as it was");
        assert!(!harness.app.ui.take_agent_commit(ticket), "the refused commit's ticket is gone");
    }

    /// Regression (a follow-up of the 48-desktop-a review): a cell of a locked table offered its fill for editing, and the engine refused
    /// the change (`ensure_table_owner_unlocked`). The fill row is off for it now, as for a locked node.
    #[test]
    fn a_cell_of_a_locked_table_offers_no_fill_edit() {
        use eframe::egui::epaint::Shape;
        fn texts(output: &eframe::egui::FullOutput) -> Vec<(String, Pos2)> {
            fn walk(shape: &Shape, found: &mut Vec<(String, Pos2)>) {
                match shape {
                    Shape::Text(text) => found.push((text.galley.text().to_owned(), text.pos + text.galley.size() / 2.0)),
                    Shape::Vec(shapes) => shapes.iter().for_each(|shape| walk(shape, found)),
                    _ => {}
                }
            }
            let mut found = Vec::new();
            output.shapes.iter().for_each(|clipped| walk(&clipped.shape, &mut found));
            found
        }
        // The add action of a section: the "추가" on the line of its title.
        let add_of = |output: &eframe::egui::FullOutput, section: &str| {
            let all = texts(output);
            let (_, title) = all.iter().find(|(text, _)| text == section).cloned().unwrap_or_else(|| panic!("the {section} section is drawn"));
            all.into_iter().find(|(text, centre)| text == "추가" && (centre.y - title.y).abs() < 4.0).map(|(_, centre)| centre)
        };
        // The cell `c11` in a table locked or not, the cell itself locked or not; `painted` gives it its own fill and stroke.
        let harness_with = |table_locked: bool, cell_locked: bool, painted: bool| {
            let mut artifact = table_span_test_artifact(false);
            let table = table_span_table_mut(&mut artifact, "table");
            table.ext.locked = table_locked.then_some(true);
            table.children[0].style.fill = if painted { "#AABBCC" } else { "none" }.into();
            table.children[0].style.stroke = if painted { "#112233" } else { "none" }.into();
            table.children[0].style.stroke_width = if painted { 2 } else { 0 };
            table.children[0].ext.locked = cell_locked.then_some(true);
            let mut harness = table_span_harness(artifact);
            harness.app.selected_node = Some("c11".into());
            harness.app.selected_nodes = vec!["c11".into()];
            harness
        };

        for section in ["채우기", "테두리"] {
            let mut open = harness_with(false, false, false);
            let shown = draw_properties_offscreen(&mut open, screen(1.0, 1000.0, 1000.0));
            let add = add_of(&shown, section).unwrap_or_else(|| panic!("a cell of an unlocked table offers {section}"));
            let _ = draw_properties_offscreen(&mut open, click_at(add));
            assert!(open.sent().iter().any(|request| request.1 == "changes.apply"), "the add action of {section} sends a change");

            // A cell of a locked table (this task), and a locked node (the stroke rows follow the fill, backlog desktop 20).
            for (table_locked, cell_locked) in [(true, false), (false, true)] {
                let mut locked = harness_with(table_locked, cell_locked, false);
                let shown = draw_properties_offscreen(&mut locked, screen(1.0, 1000.0, 1000.0));
                assert_eq!(add_of(&shown, section), None, "no {section} is offered (table locked {table_locked}, cell locked {cell_locked})");
                let _ = draw_properties_offscreen(&mut locked, click_at(add));
                assert!(!locked.sent().iter().any(|request| request.1 == "changes.apply"), "nothing is sent for {section}");
            }
        }

        // A cell that already has its fill and stroke: the colour fields of both rows are off in a locked table or for a locked
        // node, as the add actions are. The unlocked cell is the control: typing a new hex there sends a change.
        for hex in ["#AABBCC", "#112233"] {
            let type_new_hex = |harness: &mut Harness| {
                let shown = draw_properties_offscreen(harness, screen(1.0, 1000.0, 1000.0));
                let at = texts(&shown).into_iter().find(|(text, _)| text == hex).map(|(_, at)| at).unwrap_or_else(|| panic!("{hex} is drawn"));
                let _ = draw_properties_offscreen(harness, click_at(at));
                let key = |key| Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers: Modifiers::NONE };
                let mut typing = screen(1.0, 1000.0, 1000.0);
                typing.events = vec![key(eframe::egui::Key::Backspace), Event::Text("F".into()), key(eframe::egui::Key::Enter)];
                let _ = draw_properties_offscreen(harness, typing);
            };
            for (table_locked, cell_locked) in [(false, false), (true, false), (false, true)] {
                let mut painted = harness_with(table_locked, cell_locked, true);
                type_new_hex(&mut painted);
                let changed = painted.sent().iter().any(|request| request.1 == "changes.apply");
                let editable = !table_locked && !cell_locked;
                assert_eq!(changed, editable, "the colour {hex} edit (table locked {table_locked}, cell locked {cell_locked})");
            }
        }
    }

    #[test]
    fn canvas_table_anchor_unmerge_sends_structure_only_and_preserves_data() {
        let mut artifact = table_span_test_artifact(false);
        let table_node = table_span_table_mut(&mut artifact, "table");
        table_node.children[0].text = "keep anchor text".into();
        table_node.children[0].style.fill = "#AABBCC".into();
        table_node.children[1].style.fill = "#AABBCC".into();
        table_node.ext.table.as_mut().unwrap().cell_spans.insert(
            "c11".into(), designllm_contracts::CanvasTableSpan { row_span: 1, column_span: 2 },
        );
        let mut harness = table_span_harness(artifact);
        harness.app.selected_node = Some("c11".into());
        harness.app.selected_nodes = vec!["c11".into()];

        let output = draw_properties_offscreen(&mut harness, screen(1.0, 1000.0, 1000.0));
        assert!(drawn_text(&output).iter().any(|text| text == "병합 해제"), "selecting the span anchor exposes unmerge");
        let position = find_text_center(&output, "병합 해제");
        let _ = draw_properties_offscreen(&mut harness, click_at(position));
        let requests = harness.sent();
        let change = requests.iter().find(|request| request.1 == "changes.apply").expect("anchor unmerge sends a ChangeSet");
        assert_eq!(change.2["operations"][0]["op"], "set_canvas_table_structure");
        assert_eq!(change.2["operations"][0]["expected"]["cell_spans"]["c11"], json!({ "row_span": 1, "column_span": 2 }));
        assert!(change.2["operations"][0]["table"].get("cell_spans").is_none(), "the empty span map stays omitted from legacy JSON");
        let ArtifactBody::Canvas(document) = &harness.app.artifact.as_ref().unwrap().body else { panic!("Canvas body") };
        assert_eq!(document.pages[0].nodes[0].children[0].text, "keep anchor text");
        assert_eq!(document.pages[0].nodes[0].children[0].style.fill, "#AABBCC");
        assert!(document.pages[0].nodes[0].ext.table.as_ref().unwrap().cell_spans.contains_key("c11"), "planning alone does not mutate the artifact");
    }

    #[test]
    fn canvas_table_merge_refusal_is_visible_and_preserves_covered_data() {
        let mut artifact = table_span_test_artifact(false);
        table_span_table_mut(&mut artifact, "table").children[1].text = "keep covered text".into();
        let mut harness = table_span_harness(artifact);
        harness.app.selected_node = Some("c11".into());
        harness.app.selected_nodes = vec!["c11".into(), "c12".into()];

        let output = draw_properties_offscreen(&mut harness, screen(1.0, 1000.0, 1000.0));
        assert!(drawn_text(&output).iter().any(|text| text == "병합"), "the rectangular selection can request a merge");
        let position = find_text_center(&output, "병합");
        let _ = draw_properties_offscreen(&mut harness, click_at(position));
        let requests = harness.sent();
        let change = requests.iter().find(|request| request.1 == "changes.apply").expect("the merge request reaches the backend");
        let structure = change.2["operations"].as_array().unwrap().iter().find(|op| op["op"] == "set_canvas_table_structure").expect("the structure change");
        assert_eq!(structure["table"]["cell_spans"]["c11"], json!({ "row_span": 1, "column_span": 2 }));

        let mut refusal = DltError::new(ErrorCode::InvalidArgument, "covered cell content cannot be hidden");
        refusal.affected_ids.push("c12".into());
        refusal.diagnostics.push(Diagnostic {
            code: "TABLE_SPAN_HIDDEN_DATA".into(),
            message: "covered cell content cannot be hidden".into(),
            node_id: Some("c12".into()),
            blocking: true,
            params: Default::default(),
        });
        harness.answer(&requests, "changes.apply", Err(refusal));
        assert!(harness.message().is_some(), "a backend refusal is visible to the user");
        assert!(harness.app.refusal.is_some(), "the refusal callout remains attached to the selected target");
        let ArtifactBody::Canvas(document) = &harness.app.artifact.as_ref().unwrap().body else { panic!("Canvas body") };
        assert_eq!(document.pages[0].nodes[0].children[1].text, "keep covered text", "a refused structural edit leaves covered data untouched");
        assert_eq!(document.pages[0].nodes[0].children[1].style, document.pages[0].nodes[0].children[0].style, "a refused merge preserves the covered cell style");
        assert_eq!(harness.app.selected_nodes, vec!["c11".to_owned(), "c12".to_owned()], "a refusal leaves the user's selection available to fix");
        assert!(document.pages[0].nodes[0].ext.table.as_ref().unwrap().cell_spans.is_empty());
    }

    #[test]
    fn clicking_covered_merge_text_selects_anchor_for_unmerge_without_inline_edit() {
        use designllm_contracts::{RenderPage, RenderProfile, RenderScene, RenderSnapshot, SourceRegion, TextRange, ViewTransform};

        for edit_text in [false, true] {
            let mut artifact = table_span_test_artifact(false);
            let table_node = table_span_table_mut(&mut artifact, "table");
            table_node.children[0].text = "anchor text".into();
            table_node.children[1].text = "covered text".into();
            table_node.ext.table.as_mut().unwrap().cell_spans.insert(
                "c11".into(), designllm_contracts::CanvasTableSpan { row_span: 1, column_span: 2 },
            );
            let mut harness = table_span_harness(artifact);
            let covered_bounds = designllm_contracts::Rect {
                x: 0, y: 0, width: 120 * designllm_contracts::EMU_PER_CSS_PX, height: 36 * designllm_contracts::EMU_PER_CSS_PX,
            };
            let page = RenderPage {
                id: "page-1".into(),
                page_index: 0,
                size: designllm_contracts::Size::slide(),
                primitives: Vec::new(),
                source_map: vec![SourceRegion {
                    node_id: "c12".into(),
                    bounds: covered_bounds,
                    logical_range: Some(TextRange { paragraph_id: "c12".into(), start: 0, end: 12 }),
                    grapheme_rects: Vec::new(),
                }],
            };
            let snapshot = RenderSnapshot {
                id: "covered-text-snapshot".into(),
                artifact_id: "a1".into(),
                revision: 1,
                profile: RenderProfile::default(),
                scene: RenderScene {
                    pages: vec![page],
                    font_fingerprint: "fixture".into(),
                    diagnostics: Vec::new(),
                    metadata: Default::default(),
                    links: Vec::new(),
                },
                preview: None,
                preview_transform: None,
            };
            harness.app.store_page_texture(&harness.ctx, 0, &snapshot, 1, 1, &[255, 255, 255, 255]);
            harness.app.page_transforms.insert(0, ViewTransform {
                id: "transform-1".into(),
                dpi: 96,
                zoom_milli: 1000,
                offset_x_micropx: 0,
                offset_y_micropx: 0,
                crop: None,
            });
            let _ = harness.sent();
            harness.app.hit_test_page(0, Pos2::new(20.0, 20.0), 1.0, false, edit_text);
            let hit_request = harness.sent();
            assert_eq!(methods(&hit_request), ["render.hit_test"]);
            harness.answer(
                &hit_request,
                "render.hit_test",
                Ok(json!({
                    "hit": { "node_id": "c12", "position": { "paragraph_id": "c12", "grapheme_index": 2, "trailing": false } }
                })),
            );

            assert_eq!(harness.app.selected_node.as_deref(), Some("c11"));
            assert_eq!(harness.app.selected_nodes, vec!["c11".to_owned()]);
            assert!(harness.app.inline_text_edit.is_none(), "covered text requires unmerge before inline editing");
            assert!(harness.app.canvas_table_span_operation().is_some_and(|(unmerge, _)| unmerge), "the anchor enables the existing unmerge action");
            let followup = harness.sent();
            assert!(followup.iter().all(|request| !matches!(request.1.as_str(), "changes.apply" | "targets.reserve")), "covered text hit does not mutate or reserve the hidden cell");
            if edit_text {
                assert!(harness.message().is_some_and(|(_, message)| message.contains("병합을 해제")), "double-click explains the required recovery action");
            }
            let panel = draw_properties_offscreen(&mut harness, screen(1.0, 1000.0, 1000.0));
            assert!(drawn_text(&panel).iter().any(|text| text == "병합 해제"), "the selected anchor exposes unmerge in the existing inspector");
        }
    }

    pub(super) fn inline_table_cell_harness(
        target_cell: &str,
        covered: bool,
        table_locked: bool,
        cell_locked: bool,
    ) -> (Harness, designllm_contracts::Rect, designllm_contracts::Rect, designllm_contracts::Size) {
        use designllm_contracts::{CanvasTableSpan, RenderPage, RenderProfile, RenderScene, RenderSnapshot, SourceRegion};

        let mut artifact = table_span_test_artifact(false);
        let page_size = designllm_contracts::Size::slide();
        let table_bounds = designllm_contracts::Rect {
            x: 20 * designllm_contracts::EMU_PER_CSS_PX,
            y: 30 * designllm_contracts::EMU_PER_CSS_PX,
            width: 400 * designllm_contracts::EMU_PER_CSS_PX,
            height: 72 * designllm_contracts::EMU_PER_CSS_PX,
        };
        let grid_frame = designllm_contracts::Rect {
            x: table_bounds.x,
            y: table_bounds.y,
            width: if covered { table_bounds.width } else { 100 * designllm_contracts::EMU_PER_CSS_PX },
            height: 36 * designllm_contracts::EMU_PER_CSS_PX,
        };
        let stored_bounds = designllm_contracts::Rect {
            x: 300 * designllm_contracts::EMU_PER_CSS_PX,
            y: 200 * designllm_contracts::EMU_PER_CSS_PX,
            width: 120 * designllm_contracts::EMU_PER_CSS_PX,
            height: 36 * designllm_contracts::EMU_PER_CSS_PX,
        };
        let table_node = table_span_table_mut(&mut artifact, "table");
        table_node.bounds = table_bounds;
        table_node.ext.locked = table_locked.then_some(true);
        let table = table_node.ext.table.as_mut().unwrap();
        table.column_widths = Some(vec![1, 3]);
        if covered {
            table.cell_spans.insert("c11".into(), CanvasTableSpan { row_span: 1, column_span: 2 });
        }
        let target = table_node.children.iter_mut().find(|cell| cell.id == target_cell).expect("target cell");
        target.bounds = stored_bounds;
        target.ext.locked = cell_locked.then_some(true);

        let mut frames = std::collections::BTreeMap::from([
            ("table".to_string(), table_bounds),
            ("c11".to_string(), grid_frame),
            ("c21".to_string(), designllm_contracts::Rect {
                x: table_bounds.x,
                y: table_bounds.y + 36 * designllm_contracts::EMU_PER_CSS_PX,
                width: 100 * designllm_contracts::EMU_PER_CSS_PX,
                height: 36 * designllm_contracts::EMU_PER_CSS_PX,
            }),
            ("c22".to_string(), designllm_contracts::Rect {
                x: table_bounds.x + 100 * designllm_contracts::EMU_PER_CSS_PX,
                y: table_bounds.y + 36 * designllm_contracts::EMU_PER_CSS_PX,
                width: 300 * designllm_contracts::EMU_PER_CSS_PX,
                height: 36 * designllm_contracts::EMU_PER_CSS_PX,
            }),
        ]);
        if !covered {
            frames.insert("c12".into(), designllm_contracts::Rect {
                x: table_bounds.x + 100 * designllm_contracts::EMU_PER_CSS_PX,
                y: table_bounds.y,
                width: 300 * designllm_contracts::EMU_PER_CSS_PX,
                height: 36 * designllm_contracts::EMU_PER_CSS_PX,
            });
        }
        let source_map = table_node.children.iter().filter(|cell| frames.contains_key(&cell.id)).map(|cell| SourceRegion {
            node_id: cell.id.clone(),
            bounds: cell.bounds,
            logical_range: None,
            grapheme_rects: Vec::new(),
        }).collect();
        let page = RenderPage {
            id: "page-1".into(),
            page_index: 0,
            size: page_size,
            primitives: Vec::new(),
            source_map,
        };
        let snapshot = RenderSnapshot {
            id: "table-inline-snapshot".into(),
            artifact_id: "a1".into(),
            revision: 1,
            profile: RenderProfile::default(),
            scene: RenderScene {
                pages: vec![page],
                font_fingerprint: "fixture".into(),
                diagnostics: Vec::new(),
                metadata: [("node_frames".into(), serde_json::to_value(&frames).expect("node frames"))].into_iter().collect(),
                links: Vec::new(),
            },
            preview: None,
            preview_transform: None,
        };
        let mut harness = table_span_harness(artifact);
        harness.app.selected_node = Some(target_cell.into());
        harness.app.selected_nodes = vec![target_cell.into()];
        let selected = designllm_contracts::Selection { node_ids: vec![target_cell.into()], text_range: None };
        harness.app.context.as_mut().unwrap().selection = selected.clone();
        harness.app.local_selection = Some(selected);
        harness.app.reservations_for = Some("a1".into());
        harness.app.store_page_texture(&harness.ctx, 0, &snapshot, 1, 1, &[255, 255, 255, 255]);
        assert_eq!(harness.app.page_textures[&0].frames.get("c11"), Some(&grid_frame));
        (harness, grid_frame, stored_bounds, page_size)
    }

    fn page_frame_on_screen(harness: &Harness, frame: designllm_contracts::Rect, page_size: designllm_contracts::Size) -> Rect {
        let board_rect = harness.app.board_rect.expect("board has been drawn");
        let view = harness.app.view_ref();
        let slots = crate::board::arrange(ArtifactKind::Canvas, &harness.app.page_sizes(), view.zoom);
        let origin = board_rect.min + view.pan;
        let page_rect = crate::board::to_screen(slots[0].world, origin, view.zoom);
        crate::app::page_to_screen(frame, page_size, page_rect).intersect(page_rect)
    }

    fn has_filled_rect(output: &eframe::egui::FullOutput, expected: Rect, fill: eframe::egui::Color32) -> bool {
        use eframe::egui::epaint::Shape;
        fn contains(shape: &Shape, expected: Rect, fill: eframe::egui::Color32) -> bool {
            match shape {
                Shape::Rect(rect) => rect.rect == expected && rect.fill == fill,
                Shape::Vec(shapes) => shapes.iter().any(|shape| contains(shape, expected, fill)),
                _ => false,
            }
        }
        output.shapes.iter().any(|shape| contains(&shape.shape, expected, fill))
    }

    #[test]
    fn canvas_table_cell_enter_and_first_input_use_the_engine_grid_frame() {
        let (mut harness, grid_frame, stored_bounds, page_size) = inline_table_cell_harness("c11", false, false, false);
        draw_desktop_frame(&mut harness, screen(1.0, 1280.0, 800.0));
        let _ = harness.sent();

        let mut enter = screen(1.0, 1280.0, 800.0);
        enter.events.push(Event::Key {
            key: eframe::egui::Key::Enter,
            physical_key: Some(eframe::egui::Key::Enter),
            pressed: true,
            repeat: false,
            modifiers: Modifiers::NONE,
        });
        let entered = draw_desktop_frame(&mut harness, enter);
        assert!(harness.app.inline_text_edit.is_some(), "Enter opens editing on a selected Rectangle table cell");
        assert_eq!(harness.app.text_draft, "", "the selected empty cell starts with an empty draft");
        let expected = page_frame_on_screen(&harness, grid_frame, page_size);
        let stored = page_frame_on_screen(&harness, stored_bounds, page_size);
        assert_ne!(expected, stored, "the test distinguishes the grid frame from Node.bounds");
        assert!(has_filled_rect(&entered, expected, theme::palette(&harness.ctx).artboard), "the edit overlay follows the engine's grid frame");
        assert!(harness.sent().iter().all(|request| request.1 != "changes.apply"), "opening the editor does not commit text");

        let mut first_input = screen(1.0, 1280.0, 800.0);
        first_input.events.push(Event::Text("X".into()));
        draw_desktop_frame(&mut harness, first_input);
        let requests = harness.sent();
        let changes: Vec<_> = requests.iter().filter(|request| request.1 == "changes.apply").collect();
        assert_eq!(changes.len(), 1, "the first character typed into an empty cell is committed");
        assert_eq!(changes[0].2["operations"], json!([{
            "op": "replace_text",
            "range": { "paragraph_id": "c11", "start": 0, "end": 0 },
            "text": "X"
        }]));
    }

    #[test]
    fn canvas_table_cell_enter_refuses_covered_and_locked_cells() {
        fn assert_enter_refused(mut harness: Harness) {
            draw_desktop_frame(&mut harness, screen(1.0, 1280.0, 800.0));
            let _ = harness.sent();
            let mut enter = screen(1.0, 1280.0, 800.0);
            enter.events.push(Event::Key {
                key: eframe::egui::Key::Enter,
                physical_key: Some(eframe::egui::Key::Enter),
                pressed: true,
                repeat: false,
                modifiers: Modifiers::NONE,
            });
            draw_desktop_frame(&mut harness, enter);
            assert!(harness.app.inline_text_edit.is_none(), "a covered or locked cell cannot start inline editing");
            let requests = harness.sent();
            assert!(requests.iter().all(|request| !matches!(request.1.as_str(), "changes.apply" | "targets.reserve")), "a refused cell does not send edits or reservations");
        }

        assert_enter_refused(inline_table_cell_harness("c12", true, false, false).0);
        assert_enter_refused(inline_table_cell_harness("c11", false, false, true).0);
        assert_enter_refused(inline_table_cell_harness("c11", false, true, false).0);
    }

    #[test]
    fn published_selection_has_primary_first_and_detail_lists_partly_visible_pages() {
        let mut harness = Harness::new();
        let active = serde_json::from_value(summary("a1", "canvas", 0)).expect("summary");
        harness.app.activate(active);
        let pages = (0..3)
            .map(|index| designllm_contracts::Page::new(format!("page-{index}"), designllm_contracts::Size { width: 100 * 9525, height: 100 * 9525 }))
            .collect();
        harness.app.artifact = Some(designllm_contracts::Artifact::new(
            designllm_contracts::ArtifactKind::Canvas,
            "canvas",
            "canvas.v1",
            designllm_contracts::ArtifactBody::Canvas(designllm_contracts::CanvasDocument { pages, styles: Default::default(), ..Default::default() }),
        ));
        harness.app.selected_nodes = vec!["third".into(), "secondary".into(), "primary".into()];
        harness.app.selected_node = Some("primary".into());
        harness.app.board_rect = Some(Rect::from_min_size(Pos2::new(10.0, 20.0), Vec2::new(250.0, 120.0)));
        let view = harness.app.view();
        view.zoom = 1.0;
        view.pan = Vec2::new(-180.0, 0.0);

        let state = harness.app.ui_state();
        assert_eq!(state.selection.node_ids, ["primary", "third", "secondary"]);
        assert_eq!(state.detail["visible_pages"], json!([1, 2]));
    }

    #[test]
    fn published_selection_includes_a_primary_missing_from_the_multiselect_cache() {
        let mut harness = Harness::new();
        harness.app.selected_node = Some("primary".into());
        harness.app.selected_nodes = vec!["secondary".into(), "third".into()];
        assert_eq!(harness.app.ui_state().selection.node_ids, ["primary", "secondary", "third"]);
    }

    #[test]
    fn queued_layer_navigation_requires_state_bound_runs_and_stays_disabled_without_layers() {
        let mut home = Harness::new();
        home.sent();
        let home_state = publish(&mut home);
        for id in ["select.next", "select.previous"] {
            let command = home_state["commands"]
                .as_array()
                .unwrap()
                .iter()
                .find(|command| command["id"] == id)
                .expect("the command remains in the registry");
            assert_eq!(command["enabled"], false, "{id} is unavailable on the home screen");
        }
        deliver(&mut home, vec![run("home-select", "select.previous", json!({}), "read")]);
        let home_sent = home.sent();
        assert!(home_sent.iter().all(|request| request.1 != "selection.set"));
        let home_complete = home_sent.iter().find(|request| request.1 == "ui.command.complete").expect("the home run is refused");
        let home_refusal: DltError = serde_json::from_value(home_complete.2["error"].clone()).expect("a refusal");
        assert_eq!(home_refusal.code, ErrorCode::RevisionConflict, "the delivered run has no queue-time snapshot");

        let mut harness = Harness::new();
        harness.sent();
        harness.app.screen = super::super::Screen::Editor;
        harness.app.active = Some(serde_json::from_value(summary("a1", "canvas", 1)).expect("Canvas summary"));
        let mut artifact = text_artifact(canvas_text_node("n1", "one"));
        if let designllm_contracts::ArtifactBody::Canvas(document) = &mut artifact.body {
            document.pages[0].nodes.push(canvas_text_node("n2", "two"));
        }
        harness.app.artifact = Some(artifact);
        harness.app.context = Some(serde_json::from_value(context("a1", &harness.app.view_id, 1)).expect("context"));

        let published = publish(&mut harness);
        for id in ["select.next", "select.previous"] {
            let command = published["commands"]
                .as_array()
                .unwrap()
                .iter()
                .find(|command| command["id"] == id)
                .expect("the command remains in the registry");
            assert_eq!(command["enabled"], true, "{id} is available on a Canvas page with layers");
        }

        deliver(&mut harness, vec![run("select-run", "select.next", json!({}), "read")]);
        let sent = harness.sent();
        assert!(sent.iter().all(|request| request.1 != "selection.set"), "a queued command without its state snapshot is refused");
        let complete = sent.iter().find(|request| request.1 == "ui.command.complete").expect("the refused run completes");
        let refusal: DltError = serde_json::from_value(complete.2["error"].clone()).expect("a refusal");
        assert_eq!(refusal.code, ErrorCode::RevisionConflict);
        assert!(harness.app.ui_state().selection.node_ids.is_empty());
    }

    #[test]
    fn queued_file_save_requires_an_explicit_path_and_no_pending_edit() {
        let mut harness = Harness::new();
        harness.sent();
        harness.app.screen = super::super::Screen::Editor;
        harness.app.active = Some(serde_json::from_value(summary("a1", "canvas", 1)).expect("Canvas summary"));
        harness.app.artifact = Some(text_artifact(canvas_text_node("n1", "one")));
        harness.app.context = Some(serde_json::from_value(context("a1", &harness.app.view_id, 1)).expect("context"));
        let published = publish(&mut harness);
        let save = published["commands"].as_array().unwrap().iter().find(|command| command["id"] == "file.save").expect("save command");
        assert_eq!(save["permission"], "file_write");
        assert!(save["args_schema"]["required"].as_array().unwrap().contains(&json!("target_path")));
        assert_eq!(save["enabled"], true, "the engine can save native Canvas files");

        let no_target = guarded_run(&harness, "save-no-target", "file.save", json!({}), "file_write", json!([]));
        deliver(&mut harness, vec![no_target]);
        let sent = harness.sent();
        assert!(sent.iter().all(|request| request.1 != "artifact.save"), "a queued save never falls back to the GUI's remembered path");
        let completed = sent.iter().find(|request| request.1 == "ui.command.complete").expect("missing target is reported");
        let refusal: DltError = serde_json::from_value(completed.2["error"].clone()).expect("target refusal");
        assert_eq!(refusal.code, ErrorCode::InvalidArgument);

        harness.app.pending.insert(999, super::super::PendingRequest {
            action: super::super::PendingAction::Apply,
            method: "changes.apply".into(),
            artifact_id: Some("a1".into()),
            require_active: true,
            apply: None,
            ui_run: None,
        });
        let queued = guarded_run(
            &harness,
            "save-pending-edit",
            "file.save",
            json!({ "target_path": local_test_path("current.canvas") }),
            "file_write",
            json!([]),
        );
        deliver(&mut harness, vec![queued]);
        let sent = harness.sent();
        assert!(sent.iter().all(|request| request.1 != "artifact.save"), "save waits until the pending edit resolves");
        let completed = sent.iter().find(|request| request.1 == "ui.command.complete").expect("pending edit is reported");
        let refusal: DltError = serde_json::from_value(completed.2["error"].clone()).expect("pending edit refusal");
        assert_eq!(refusal.code, ErrorCode::RevisionConflict);
    }

    #[test]
    fn queued_file_save_refuses_a_stale_revision_before_rpc() {
        let mut harness = Harness::new();
        harness.sent();
        harness.app.screen = super::super::Screen::Editor;
        harness.app.active = Some(serde_json::from_value(summary("a1", "canvas", 2)).expect("Canvas summary"));
        let mut artifact = text_artifact(canvas_text_node("n1", "one"));
        artifact.revision = 2;
        harness.app.artifact = Some(artifact);
        harness.app.context = Some(serde_json::from_value(context("a1", &harness.app.view_id, 2)).expect("context"));
        publish(&mut harness);

        let mut queued = guarded_run(
            &harness,
            "save-stale-revision",
            "file.save",
            json!({ "target_path": local_test_path("stale.canvas") }),
            "file_write",
            json!([]),
        );
        queued["expected"]["revision"] = json!(1);
        deliver(&mut harness, vec![queued]);
        let sent = harness.sent();
        assert!(sent.iter().all(|request| request.1 != "artifact.save"), "the stale snapshot never reaches the save RPC");
        let complete = sent.iter().find(|request| request.1 == "ui.command.complete").expect("stale save is refused");
        let refusal: DltError = serde_json::from_value(complete.2["error"].clone()).expect("state refusal");
        assert_eq!(refusal.code, ErrorCode::RevisionConflict);
    }

    #[test]
    fn queued_file_export_refuses_while_an_edit_is_pending() {
        let mut harness = Harness::new();
        harness.sent();
        harness.app.screen = super::super::Screen::Editor;
        harness.app.active = Some(serde_json::from_value(summary("a1", "canvas", 1)).expect("Canvas summary"));
        harness.app.artifact = Some(text_artifact(canvas_text_node("n1", "one")));
        harness.app.context = Some(serde_json::from_value(context("a1", &harness.app.view_id, 1)).expect("context"));
        harness.app.pending.insert(999, super::super::PendingRequest {
            action: super::super::PendingAction::Apply,
            method: "changes.apply".into(),
            artifact_id: Some("a1".into()),
            require_active: true,
            apply: None,
            ui_run: None,
        });
        publish(&mut harness);
        let queued = guarded_run(
            &harness,
            "export-pending-edit",
            "file.export",
            json!({ "format": "pdf", "target_path": local_test_path("output.pdf"), "allowed_losses": [] }),
            "file_write",
            json!([]),
        );
        deliver(&mut harness, vec![queued]);
        let sent = harness.sent();
        assert!(sent.iter().all(|request| request.1 != "artifact.export"), "export is not taken from a pending revision");
        let complete = sent.iter().find(|request| request.1 == "ui.command.complete").expect("pending export is refused");
        let refusal: DltError = serde_json::from_value(complete.2["error"].clone()).expect("pending edit refusal");
        assert_eq!(refusal.code, ErrorCode::RevisionConflict);
    }

    #[test]
    fn direct_layer_navigation_refuses_stale_page_and_deleted_primary_before_rpc() {
        use designllm_contracts::{Page, Size};

        let mut harness = Harness::new();
        harness.sent();
        harness.app.screen = super::super::Screen::Editor;
        harness.app.active = Some(serde_json::from_value(summary("a1", "canvas", 1)).expect("Canvas summary"));
        let mut artifact = text_artifact(canvas_text_node("old-page", "one"));
        if let designllm_contracts::ArtifactBody::Canvas(document) = &mut artifact.body {
            document.pages.push(Page { nodes: vec![canvas_text_node("current-page", "two")], ..Page::new("page-2", Size::a4()) });
        }
        harness.app.artifact = Some(artifact);
        harness.app.context = Some(serde_json::from_value(context("a1", &harness.app.view_id, 1)).expect("context"));
        harness.app.selected_node = Some("old-page".into());
        harness.app.selected_nodes = vec!["old-page".into()];
        harness.app.local_selection = Some(designllm_contracts::Selection { node_ids: vec!["old-page".into()], text_range: None });
        harness.app.page_index = 1;
        harness.app.view().page_index = 1;

        let stale_page = harness.app.invoke("select.next", json!({}));
        assert!(stale_page.unwrap_err().contains("현재 페이지"));
        assert!(harness.sent().is_empty(), "a selection from a different page is not forwarded");
        assert_eq!(harness.app.selected_node.as_deref(), Some("old-page"));

        let designllm_contracts::ArtifactBody::Canvas(document) = &mut harness.app.artifact.as_mut().unwrap().body else { unreachable!() };
        document.pages[0].nodes.push(canvas_text_node("another-old-page", "세 번째"));
        document.pages[0].nodes.retain(|node| node.id != "old-page");
        harness.app.page_index = 0;
        harness.app.view().page_index = 0;
        let deleted = harness.app.invoke("select.next", json!({}));
        assert!(deleted.unwrap_err().contains("문서에 없습니다"));
        assert!(harness.sent().is_empty(), "a deleted primary is not replaced with an unrelated layer");
        assert_eq!(harness.app.selected_node.as_deref(), Some("old-page"));

        harness.app.page_index = u32::MAX;
        harness.app.view().page_index = u32::MAX;
        harness.app.local_selection = Some(designllm_contracts::Selection { node_ids: Vec::new(), text_range: None });
        harness.app.set_selected_ids(Vec::new());
        assert!(harness.app.invoke("select.next", json!({})).is_err(), "an empty current page cannot be advanced");
        assert!(harness.sent().is_empty(), "no selection RPC is sent for an empty page");
    }

    #[test]
    fn a_queued_run_runs_through_the_same_command_path_and_completes_with_the_view_token() {
        let mut harness = Harness::new();
        publish(&mut harness);
        assert!(harness.app.view_ref().left_open);
        deliver(&mut harness, vec![run("run-1", "nav.toggle_left", json!({}), "change")]);
        assert!(!harness.app.view_ref().left_open, "the agent's command ran in the window");
        let done = completions(&mut harness);
        assert_eq!(done.len(), 1, "{done:?}");
        assert_eq!(done[0]["run_id"], "run-1");
        assert_eq!(done[0]["view_token"], "tok-1");
        assert!(!done[0]["result"].is_null() && done[0]["error"].is_null(), "{done:?}");
    }

    #[test]
    fn a_run_the_registry_refuses_completes_with_the_reason_and_changes_nothing() {
        let mut harness = Harness::new();
        publish(&mut harness);
        deliver(
            &mut harness,
            vec![
                run("bad-args", "view.zoom_set", json!({ "percent": 500 }), "change"),
                run("read-only", "edit.delete", json!({}), "read"),
                run("unbuilt", "view.play", json!({}), "change"),
                run("unknown", "no.such.command", json!({}), "change"),
            ],
        );
        let done = completions(&mut harness);
        assert_eq!(done.len(), 4, "{done:?}");
        for item in &done {
            assert!(!item["error"].is_null() && item["result"].is_null(), "{item:?}");
        }
        assert!(harness.app.view_ref().left_open, "a refused run changed nothing");
    }

    #[test]
    fn a_screenshot_run_completes_with_the_captured_frame_as_an_asset() {
        let mut harness = Harness::new();
        publish(&mut harness);
        deliver(&mut harness, vec![run("shot-1", "ui.screenshot", json!({}), "read")]);
        assert!(completions(&mut harness).is_empty(), "the run waits for the frame");
        assert!(harness.app.ui.screenshot_wanted, "the next frame captures the window");
        harness.app.finish_screenshot(&ColorImage::filled([4, 3], Color32::WHITE));
        let sent = harness.sent();
        let import = sent.iter().find(|item| item.1 == "asset.import").expect("the frame is imported").clone();
        assert_eq!(import.2["mime"], "image/png");
        assert!(import.2["path"].as_str().is_some_and(|path| path.ends_with(".png")));
        harness.answer(&sent, "asset.import", Ok(json!({ "sha256": "abc", "mime": "image/png", "bytes": 10 })));
        let done = completions(&mut harness);
        assert_eq!(done.len(), 1, "{done:?}");
        assert_eq!(done[0]["result"]["asset"]["sha256"], "abc");
        assert_eq!(done[0]["result"]["width_px"], 4);
    }
}

mod flow_tests {
    use super::*;

    fn dry_run(losses: Value) -> Value {
        json!({
            "dry_run": true,
            "artifact_id": "artifact",
            "revision": 5,
            "format": "pdf",
            "mime": "application/pdf",
            "bytes": 10,
            "editability": "vector",
            "losses": losses,
            "diagnostics": [],
        })
    }

    fn html_export_result(diagnostics: Value) -> Value {
        json!({
            "request_id": "export-request",
            "artifact_id": "artifact",
            "revision": 5,
            "format": "html",
            "target_path": "C:/private/output/index.html",
            "sha256": "abc",
            "bytes": 10,
            "mime": "text/html",
            "editability": "objects",
            "diagnostics": diagnostics,
        })
    }

    #[test]
    fn a_home_request_with_a_prompt_is_one_host_call_with_its_attachments() {
        let mut harness = Harness::new();
        harness.sent();
        harness.app.home.attachments = vec!["C:/assets/ref.png".into()];
        harness.app.ai_runtime = "codex".into();
        harness.app.ai_model = "gpt-example".into();
        harness.app.ai_effort = "high".into();
        harness.app.ai_figure_refs = vec![FigureRevisionRef { figure_id: "figure-1".into(), figure_revision: 4 }];
        harness.app.create_from_home("blank", "로그인 화면".into()).expect("the request is sent");
        let work: Vec<Work> = harness.work.try_iter().collect();
        match work.as_slice() {
            [Work::Home { request, .. }] => {
                assert_eq!(request.prompt, "로그인 화면");
                assert_eq!((&*request.runtime, &*request.model, &*request.effort), ("codex", "gpt-example", "high"));
                assert_eq!(request.attachments.len(), 1, "the attachment goes with the request");
                assert_eq!(request.figure_refs, [FigureRevisionRef { figure_id: "figure-1".into(), figure_revision: 4 }]);
            }
            other => panic!("one home request and no other call, got {} calls", other.len()),
        }
        assert!(harness.app.home.attachments.is_empty(), "the attachments left the box with the request");
    }

    #[test]
    fn a_home_request_without_a_model_stays_on_home() {
        let mut harness = Harness::new();
        harness.sent();
        harness.app.ai_model.clear();
        assert!(harness.app.create_from_home("blank", "요청".into()).is_err());
        assert!(harness.app.screen == crate::app::Screen::Home);
        assert!(harness.work.try_iter().next().is_none());
    }

    #[test]
    fn editor_ai_job_sends_the_exact_selected_figure_revision() {
        let mut harness = Harness::new();
        harness.open_document("canvas", 2);
        harness.app.ai_prompt = "Place the approved figure".into();
        harness.app.ai_model = "mock-model".into();
        harness.app.ai_runtime = "mock".into();
        harness.app.ai_figure_refs = vec![FigureRevisionRef { figure_id: "figure-2".into(), figure_revision: 7 }];
        harness.app.start_ai_job();
        let request = harness.sent();
        assert_eq!(methods(&request), ["job.start"]);
        assert_eq!(request[0].2["figure_refs"], json!([{ "figure_id": "figure-2", "figure_revision": 7 }]));
    }

    #[test]
    fn a_home_create_without_a_prompt_is_a_plain_artifact_create() {
        let mut harness = Harness::new();
        harness.sent();
        harness.app.create_from_home("blank", String::new()).expect("the artifact is created");
        assert_eq!(methods(&harness.sent()), ["artifact.create"]);
    }

    #[test]
    fn job_sources_without_a_request_are_refused_before_anything_is_sent() {
        let mut harness = Harness::new();
        harness.sent();
        harness.app.home.attachments = vec!["C:/assets/ref.png".into()];
        assert!(harness.app.create_from_home("blank", "  ".into()).is_err());
        assert!(harness.work.try_recv().is_err(), "nothing reaches the host");
        harness.app.home.attachments.clear();
        harness.app.ai_figure_refs = vec![FigureRevisionRef { figure_id: "figure-1".into(), figure_revision: 0 }];
        assert!(harness.app.create_from_home("blank", "  ".into()).is_err());
        assert!(harness.work.try_recv().is_err(), "Figure approvals do not disappear into a create-only action");
    }

    #[test]
    fn export_shows_the_losses_of_a_dry_run_before_anything_is_written() {
        let mut harness = Harness::new();
        harness.open_document("canvas", 5);
        harness.sent();
        harness.app.export_format = "pdf".into();
        harness.app.export_path = "C:/out/doc.pdf".into();
        harness.app.export_active();
        let request = harness.sent();
        assert_eq!(methods(&request), ["artifact.export"]);
        assert_eq!(request[0].2["dry_run"], json!(true), "the first call only asks what would be lost");
        let loss = json!([{ "code": "font", "message": "글꼴 대체", "node_id": null, "blocking": false }]);
        harness.answer(&request, "artifact.export", Ok(dry_run(loss)));
        assert!(harness.message().is_none(), "a valid dry run raises no message, got {:?}", harness.message());
        assert!(harness.app.export_confirmation.is_some(), "the losses are shown first");
        assert!(harness.sent().is_empty(), "nothing is written until the user accepts");
    }

    #[test]
    fn export_without_losses_writes_straight_after_the_dry_run() {
        let mut harness = Harness::new();
        harness.open_document("canvas", 5);
        harness.sent();
        harness.app.export_format = "pdf".into();
        harness.app.export_path = "C:/out/doc.pdf".into();
        harness.app.export_active();
        let request = harness.sent();
        harness.answer(&request, "artifact.export", Ok(dry_run(json!([]))));
        let write = harness.sent();
        assert_eq!(methods(&write), ["artifact.export"]);
        assert!(write[0].2["dry_run"].is_null(), "the second call writes the file");
        assert!(harness.app.export_confirmation.is_none());
    }

    #[test]
    fn html_export_and_play_show_safe_link_name_warning_without_internal_text() {
        let diagnostics = json!([
            {
                "code": "HTML_LINK_ACCESSIBLE_NAME_MISSING",
                "message": "URL link has no accessible name; internal node=private-node path=C:/secret",
                "node_id": "private-node",
                "blocking": false,
                "params": { "source": "C:/secret" },
            },
            {
                "code": "PRIVATE_INTERNAL_EXPORT_DETAIL",
                "message": "never show this private engine text or path",
                "node_id": null,
                "blocking": false,
                "params": { "path": "C:/secret" },
            },
        ]);
        let expected = "내보냈습니다. 링크에 읽을 이름이 없습니다. 대체 텍스트나 표시할 이름을 추가하세요. 일부 요소를 완전히 표현하지 못했습니다. 결과를 확인해 주세요.";

        let mut export = Harness::new();
        export.sent();
        export.app.call("artifact.export", json!({}), super::super::PendingAction::Export);
        let request = export.sent();
        export.answer(&request, "artifact.export", Ok(html_export_result(diagnostics.clone())));
        let (severity, message) = export.message().expect("a non-blocking export warning is shown");
        assert_eq!(severity, Severity::Warning);
        assert_eq!(message, expected);
        assert!(!message.contains("private-node") && !message.contains("C:/secret") && !message.contains("never show"));

        let mut play = Harness::new();
        play.sent();
        play.app.smoke_output = Some(std::path::PathBuf::from("D:/scratch/canvas-preview"));
        play.app.call(
            "artifact.export",
            json!({}),
            super::super::PendingAction::PlayExport { path: std::path::PathBuf::from("D:/scratch/canvas-preview/index.html") },
        );
        let request = play.sent();
        play.answer(&request, "artifact.export", Ok(html_export_result(diagnostics)));
        let (severity, message) = play.message().expect("the play warning is shown without starting a window");
        assert_eq!(severity, Severity::Warning);
        assert_eq!(message, "링크에 읽을 이름이 없습니다. 대체 텍스트나 표시할 이름을 추가하세요. 일부 요소를 완전히 표현하지 못했습니다. 결과를 확인해 주세요.");
        assert!(!message.contains("private-node") && !message.contains("C:/secret") && !message.contains("never show"));
    }

    #[test]
    fn empty_export_and_play_diagnostics_keep_their_existing_success_ui() {
        let mut export = Harness::new();
        export.sent();
        export.app.call("artifact.export", json!({}), super::super::PendingAction::Export);
        let request = export.sent();
        export.answer(&request, "artifact.export", Ok(html_export_result(json!([]))));
        assert_eq!(export.message(), Some((Severity::Info, "내보냈습니다".into())));

        let mut play = Harness::new();
        play.sent();
        play.app.smoke_output = Some(std::path::PathBuf::from("D:/scratch/canvas-preview"));
        play.app.call(
            "artifact.export",
            json!({}),
            super::super::PendingAction::PlayExport { path: std::path::PathBuf::from("D:/scratch/canvas-preview/index.html") },
        );
        let request = play.sent();
        play.answer(&request, "artifact.export", Ok(html_export_result(json!([]))));
        assert!(play.message().is_none(), "play keeps its existing success UI when there is no diagnostic");
    }
}

mod layer_move_flow_tests {
    use super::*;

    #[test]
    fn moving_a_layer_is_one_change_that_sets_its_z_order() {
        let mut harness = Harness::new();
        harness.open_document("canvas", 5);
        harness.sent();
        harness.app.invoke_ui("layer.move", json!({ "node_id": "card", "index": 2 }));
        let request = harness.sent();
        assert_eq!(methods(&request), ["changes.apply"]);
        let operation = &request[0].2["operations"][0];
        assert_eq!(operation["op"], "set_z_order");
        assert_eq!(operation["node_id"], "card");
        assert_eq!(operation["index"], 2);
    }
}

mod agent_batch_flow_tests {
    use super::*;

    #[test]
    fn an_agent_batch_is_listed_until_it_is_undone_and_its_undo_names_it() {
        let mut harness = Harness::new();
        harness.open_document("canvas", 5);
        harness.sent();
        let artifact = harness.app.active.as_ref().expect("a document is open").id.clone();
        let inline_actor = format!("desktop:inline:{}", designllm_contracts::new_id());
        let event = |seq: u64, kind: &str, detail: Value| {
            json!({ "event_seq": seq, "artifact_id": artifact, "revision": 6, "event": kind, "detail": detail })
        };
        harness.app.handle_events(json!({ "events": [
            event(100, "artifact.changed", json!({ "request_id": "batch-1", "actor": "claude", "changed_ids": [] })),
            event(101, "artifact.changed", json!({ "request_id": "mine", "actor": "desktop", "changed_ids": [] })),
            event(102, "artifact.changed", json!({ "request_id": "inline-mine", "actor": inline_actor, "changed_ids": [] })),
            event(103, "artifact.changed", json!({ "request_id": "malformed-inline", "actor": "desktop:inline:not-a-uuid", "changed_ids": [] })),
            event(104, "artifact.changed", json!({ "request_id": "suffixed-inline", "actor": format!("desktop:inline:{}:suffix", designllm_contracts::new_id()), "changed_ids": [] })),
        ] }));
        harness.sent();
        let listed: Vec<&str> = harness.app.agent_batches.iter().map(|batch| batch.request_id.as_str()).collect();
        assert_eq!(listed, ["batch-1", "malformed-inline", "suffixed-inline"], "normal desktop and only well-formed inline desktop actors are excluded");
        harness.app.invoke_ui("ai.undo_change", json!({ "change_id": "batch-1" }));
        let request = harness.sent();
        assert_eq!(methods(&request), ["history.undo_change"]);
        assert_eq!(request[0].2["change_id"], "batch-1");
        harness.app.handle_events(json!({ "events": [event(105, "artifact.undone", json!({ "undone_change": "batch-1" }))] }));
        assert_eq!(harness.app.agent_batches.iter().map(|batch| batch.request_id.as_str()).collect::<Vec<_>>(), ["malformed-inline", "suffixed-inline"], "only the undone agent batch leaves the list");
    }

    /// 81: the host records an external connection's actor under its identity (`mcp:<connection>:<name>`, `job:<job>:<name>`,
    /// `cli:<name>`), also when the name it gave was `desktop` or an inline actor. Those changes stay AI batches.
    #[test]
    fn host_fixed_external_actors_are_agent_batches_even_when_they_named_the_desktop() {
        let mut harness = Harness::new();
        harness.open_document("canvas", 5);
        harness.sent();
        let artifact = harness.app.active.as_ref().expect("a document is open").id.clone();
        let connection = designllm_contracts::new_id();
        let inline = format!("desktop:inline:{}", designllm_contracts::new_id());
        let actors = [
            format!("mcp:{connection}:agent"),
            format!("mcp:{connection}:desktop"),
            format!("mcp:{connection}:{inline}"),
            format!("mcp:{connection}"),
            "job:j1:desktop".to_owned(),
            "cli:desktop".to_owned(),
        ];
        let events: Vec<Value> = actors.iter().enumerate().map(|(index, actor)| {
            json!({ "event_seq": 200 + index as u64, "artifact_id": artifact, "revision": 6, "event": "artifact.changed",
                    "detail": { "request_id": format!("external-{index}"), "actor": actor, "changed_ids": [] } })
        }).collect();
        harness.app.handle_events(json!({ "events": events }));
        harness.sent();
        let listed: Vec<&str> = harness.app.agent_batches.iter().map(|batch| batch.actor.as_str()).collect();
        assert_eq!(listed, actors.iter().map(String::as_str).collect::<Vec<_>>(), "every host-fixed external actor is an agent batch");
    }
}

#[test]
fn inline_view_actor_reservation_blocks_other_view_but_allows_owner_commit_and_release() {
    use super::figure_flow_tests::Host;

    let mut host = Host::start();
    let artifact_id = host.create("canvas", "inline view reservation");
    let inspected = host.call("artifact.inspect", json!({ "artifact_id": artifact_id })).expect("canvas inspects");
    let artifact: Artifact = serde_json::from_value(inspected).expect("artifact body");
    let ArtifactBody::Canvas(document) = artifact.body else { panic!("canvas document") };
    let page_id = document.pages.first().expect("canvas page").id.clone();
    let node: Node = serde_json::from_value(json!({
        "id": "reservation-node", "kind": "text", "text": "A",
        "bounds": { "x": 1000000, "y": 1000000, "width": 3000000, "height": 500000 }
    })).expect("text node");
    let initialized = host.call("changes.apply", json!({
        "request_id": designllm_contracts::new_id(), "actor": "desktop",
        "workspace_id": host.workspace_id, "artifact_id": artifact_id,
        "base_revision": artifact.revision, "allowed_intents": ["content", "design"],
        "operations": [{ "op": "add_node", "page_id": page_id, "parent_id": null, "node": node }]
    })).expect("node is added");
    let revision = initialized["revision"].as_u64().expect("revision");

    let view_a = designllm_contracts::new_id();
    let actor_a = super::state::inline_text_actor(&view_a);
    let view_b = designllm_contracts::new_id();
    let actor_b = super::state::inline_text_actor(&view_b);
    assert_ne!(actor_a, actor_b);
    let reservation = host.call("targets.reserve", json!({
        "artifact_id": artifact_id, "expected_revision": revision,
        "view_id": view_a, "owner": actor_a, "node_ids": ["reservation-node"], "ttl_ms": 300000
    })).expect("view A reserves the text node");
    let reservation_id = reservation["reservation_id"].as_str().expect("reservation id").to_owned();

    let competing_reserve = host.call("targets.reserve", json!({
        "artifact_id": artifact_id, "expected_revision": revision,
        "view_id": view_b, "owner": actor_b, "node_ids": ["reservation-node"], "ttl_ms": 300000
    })).expect_err("view B cannot reserve view A's node");
    assert_eq!(competing_reserve.code, ErrorCode::TargetBusy);

    let competing_change = host.call("changes.apply", json!({
        "request_id": designllm_contracts::new_id(), "actor": actor_b,
        "workspace_id": host.workspace_id, "artifact_id": artifact_id,
        "base_revision": revision, "allowed_intents": ["content"],
        "operations": [{ "op": "set_text", "node_id": "reservation-node", "text": "B" }]
    })).expect_err("view B cannot commit through view A's reservation");
    assert_eq!(competing_change.code, ErrorCode::TargetBusy);
    let held = host.call("targets.list", json!({ "artifact_id": artifact_id })).expect("reservations are listed");
    assert_eq!(held["reservations"].as_array().map(|items| items.len()), Some(1), "view B's refusal leaves view A's reservation intact");
    assert_eq!(held["reservations"][0]["reservation_id"], reservation_id);
    assert_eq!(held["reservations"][0]["owner"], actor_a);
    assert_eq!(held["reservations"][0]["view_id"], view_a);

    let owned_change = host.call("changes.apply", json!({
        "request_id": designllm_contracts::new_id(), "actor": actor_a,
        "workspace_id": host.workspace_id, "artifact_id": artifact_id,
        "base_revision": revision, "allowed_intents": ["content"],
        "operations": [{ "op": "set_text", "node_id": "reservation-node", "text": "A!" }]
    })).expect("the owner can commit while its reservation is held");
    let owned_revision = owned_change["revision"].as_u64().expect("committed revision");
    assert!(host.call("targets.release", json!({ "reservation_id": reservation_id })).is_ok(), "view A releases its reservation");

    let after_release = host.call("targets.reserve", json!({
        "artifact_id": artifact_id, "expected_revision": owned_revision,
        "view_id": view_b, "owner": actor_b, "node_ids": ["reservation-node"], "ttl_ms": 300000
    })).expect("view B can reserve after view A releases");
    assert!(host.call("targets.release", json!({ "reservation_id": after_release["reservation_id"] })).is_ok());
}

mod held_edit_flow_tests {
    use super::*;

    fn event(seq: u64, kind: &str, detail: Value) -> Value {
        json!({ "event_seq": seq, "artifact_id": "a1", "revision": 5, "event": kind, "detail": detail })
    }

    fn reserved_by_job() -> DltError {
        DltError {
            code: ErrorCode::TargetBusy,
            message: "targets are reserved by job j1 (claude)".into(),
            current_revision: None,
            affected_ids: vec!["card".into()],
            retryable: true,
            diagnostics: Vec::new(),
        }
    }

    #[test]
    fn a_refused_edit_is_sent_again_once_after_any_reserving_job_is_stopped_and_releases_its_nodes() {
        let mut harness = Harness::new();
        let (opening, _) = harness.open_document("canvas", 5);
        harness.answer(
            &opening,
            "targets.list",
            Ok(json!({
                "artifact_id": "a1",
                "reservations": [{ "reservation_id": "r1", "view_id": "v", "owner": "job j1 (claude)", "node_ids": ["card"], "job_id": "j1" }],
            })),
        );
        harness.sent();
        harness.app.invoke_ui("layer.move", json!({ "node_id": "card", "index": 0 }));
        let edit = harness.sent();
        assert_eq!(methods(&edit), ["changes.apply"]);
        harness.answer(&edit, "changes.apply", Err(reserved_by_job()));
        assert_eq!(harness.app.held_edits.len(), 1, "the refused ticket's edit is held for its job");

        harness.app.stop_job_for_edit("j1");
        let stop = harness.sent();
        assert_eq!(methods(&stop), ["job.cancel"]);
        assert_eq!(stop[0].2["job_id"], "j1");
        assert!(stop[0].2.get("attempt_id").is_none(), "job.cancel needs only the job id from this connection");
        harness.answer(&stop, "job.cancel", Ok(json!({ "id": "j1" })));
        assert!(
            harness.sent().iter().all(|request| request.1 != "changes.apply"),
            "nothing is sent again before the job releases its nodes"
        );

        harness.app.handle_events(json!({ "events": [event(400, "targets.released", json!({
            "reservation_id": "r1", "job_id": "j1", "owner": "job j1 (claude)", "node_ids": ["card"],
        }))] }));
        let resent: Vec<Request> = harness.sent().into_iter().filter(|request| request.1 == "changes.apply").collect();
        assert_eq!(resent.len(), 1, "the edit is sent again exactly once");
        assert_eq!(resent[0].2["operations"][0]["op"], "set_z_order");
        assert_eq!(harness.app.held_edits.len(), 1, "the held operation stays correlated while its retry is in flight");
        assert_eq!(harness.app.held_edits[0].state, super::super::state::HeldEditState::Sending);
        harness.answer(&resent, "changes.apply", Ok(json!({ "revision": 6, "changed_ids": ["card"], "diagnostics": [] })));
        assert!(harness.app.held_edits.is_empty(), "the held operation is removed only after its retry succeeds");
        assert!(harness.message().is_none(), "no status sentence in the status line");
    }
}

mod target_flow_tests {
    use super::*;

    fn event(seq: u64, kind: &str, detail: Value) -> Value {
        json!({ "event_seq": seq, "artifact_id": "a1", "revision": 5, "event": kind, "detail": detail })
    }

    fn listed(harness: &Harness) -> Vec<String> {
        harness.app.reservations.iter().map(|reservation| reservation.reservation_id.clone()).collect()
    }

    #[test]
    fn job_reservations_come_from_targets_list_and_their_events_until_released() {
        let mut harness = Harness::new();
        let (opening, _) = harness.open_document("canvas", 5);
        harness.answer(
            &opening,
            "targets.list",
            Ok(json!({
                "artifact_id": "a1",
                "reservations": [
                    { "reservation_id": "r1", "view_id": "v", "owner": "job j1 (claude)", "node_ids": ["card"], "job_id": "j1" },
                    { "reservation_id": "u1", "view_id": "v", "owner": "user", "node_ids": ["title"] },
                ],
            })),
        );
        assert_eq!(listed(&harness), ["r1"], "only job reservations are marked");
        harness.app.handle_events(json!({ "events": [
            event(300, "targets.reserved", json!({ "reservation_id": "r2", "job_id": "j2", "owner": "job j2 (codex)", "node_ids": ["title"] })),
        ] }));
        assert_eq!(listed(&harness), ["r1", "r2"], "a new job reservation is marked at once");
        harness.app.handle_events(json!({ "events": [
            event(301, "targets.released", json!({ "reservation_id": "r1", "job_id": "j1", "owner": "job j1 (claude)", "node_ids": ["card"] })),
        ] }));
        assert_eq!(listed(&harness), ["r2"], "a released reservation stops marking its nodes");
    }

    #[test]
    fn an_edit_refused_by_a_reservation_is_explained_at_its_node_and_not_in_the_status_line() {
        let mut harness = Harness::new();
        harness.open_document("canvas", 5);
        harness.sent();
        harness.app.invoke_ui("layer.move", json!({ "node_id": "card", "index": 0 }));
        let request = harness.sent();
        assert_eq!(methods(&request), ["changes.apply"]);
        let busy = DltError {
            code: ErrorCode::TargetBusy,
            message: "targets are reserved by job j1 (claude)".into(),
            current_revision: None,
            affected_ids: vec!["card".into()],
            retryable: true,
            diagnostics: Vec::new(),
        };
        harness.answer(&request, "changes.apply", Err(busy));
        assert_eq!(harness.app.refusal_ids, ["card"], "the callout names the reserved node");
        assert!(harness.app.refusal.is_some(), "the callout has its text");
        assert!(harness.message().is_none(), "a reserved node gets no status sentence");
    }
}

mod flow_document_find_tests {
    use designllm_contracts::{
        Artifact, ArtifactBody, ArtifactKind, FlowBlock, FlowDocument, Paragraph, ParagraphFormat,
        Section, Size, TextRange, TextRun,
    };
    use eframe::egui::{Pos2, Rect, Vec2};
    use serde_json::{json, Value};

    use super::{context, methods, Harness};

    fn artifact_with_first_text(revision: u64, first_text: &str) -> Artifact {
        let paragraph = |id: &str, text: &str| {
            FlowBlock::Paragraph(Paragraph {
                id: id.into(),
                runs: vec![TextRun::new(format!("{id}-run"), text)],
                style_ref: None,
                format: ParagraphFormat::default(),
            })
        };
        let mut artifact = Artifact::new(
            ArtifactKind::Docx,
            "본문 검색",
            "engine",
            ArtifactBody::Flow(FlowDocument {
                sections: vec![Section {
                    id: "section".into(),
                    setup: Default::default(),
                    blocks: vec![paragraph("first", first_text), paragraph("second", "new query result")],
                }],
                styles: Default::default(),
            }),
        );
        artifact.id = "a1".into();
        artifact.revision = revision;
        artifact
    }

    fn artifact(revision: u64) -> Artifact {
        artifact_with_first_text(revision, "target phrase")
    }

    fn page_snapshot_for(artifact_id: &str, revision: u64, page_index: u32, total_pages: u32, range: Option<TextRange>) -> Value {
        let source_map = range.into_iter().map(|logical_range| json!({
            "node_id": "flow-run",
            "bounds": { "x": 0, "y": 0, "width": 100, "height": 20 },
            "logical_range": logical_range,
            "grapheme_rects": [],
        })).collect::<Vec<_>>();
        json!({
            "id": format!("snapshot-{revision}-{page_index}"),
            "artifact_id": artifact_id,
            "revision": revision,
            "profile": { "dpi": 144, "transparent": false },
            "scene": {
                "pages": [{
                    "id": format!("page-{page_index}"),
                    "page_index": page_index,
                    "size": serde_json::to_value(Size::a4()).expect("page size"),
                    "primitives": [],
                    "source_map": source_map,
                }],
                "font_fingerprint": "",
                "diagnostics": [],
                "metadata": { "total_pages": total_pages },
            },
            "preview": null,
            "preview_transform": null,
        })
    }

    fn page_snapshot(revision: u64, page_index: u32, total_pages: u32, range: Option<TextRange>) -> Value {
        page_snapshot_for("a1", revision, page_index, total_pages, range)
    }

    fn opened_with_first_text(first_text: &str) -> Harness {
        let mut harness = Harness::new();
        let (_, view_id) = harness.open_document("docx", 1);
        harness.app.artifact = Some(artifact_with_first_text(1, first_text));
        harness.app.context = Some(serde_json::from_value(context("a1", &view_id, 1)).expect("context"));
        harness.app.total_pages = 2;
        harness.app.board_rect = Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(800.0, 600.0)));
        harness.app.layer_find_query = "target".into();
        harness.app.sync_flow_find_search();
        harness
    }

    fn opened() -> Harness {
        opened_with_first_text("target phrase")
    }

    #[test]
    fn clicking_a_flow_result_fetches_one_page_at_a_time_then_selects_its_source_range() {
        let mut harness = opened();
        let before = harness.app.artifact.clone().expect("document");
        harness.app.open_flow_find_result(0);
        let first = harness.sent();
        assert_eq!(methods(&first), ["render.get"]);
        assert_eq!(first[0].2["page_index"], 0);
        assert_eq!(first[0].2["expected_revision"], 1);

        harness.answer(&first, "render.get", Ok(page_snapshot(1, 0, 2, None)));
        let second = harness.sent();
        assert_eq!(methods(&second), ["render.get"], "the next page is not requested until the first reply arrives");
        assert_eq!(second[0].2["page_index"], 1);
        assert_eq!(second[0].2["expected_revision"], 1);

        harness.answer(
            &second,
            "render.get",
            Ok(page_snapshot(1, 1, 2, Some(TextRange { paragraph_id: "first".into(), start: 0, end: 6 }))),
        );
        let selection = harness.sent();
        assert_eq!(methods(&selection), ["selection.set"]);
        assert_eq!(selection[0].2["visible_page"], 1);
        assert_eq!(selection[0].2["selection"]["text_range"], json!({ "paragraph_id": "first", "start": 0, "end": 6 }));
        assert_eq!(harness.app.page_index, 1);
        assert_eq!(harness.app.artifact.as_ref().map(|document| document.revision), Some(1));
        assert_eq!(
            serde_json::to_value(harness.app.artifact.as_ref().expect("document")).expect("current document"),
            serde_json::to_value(&before).expect("original document"),
            "finding a result does not edit the document"
        );
        assert!(!selection.iter().any(|request| request.1 == "changes.apply"));
    }

    #[test]
    fn a_query_change_ignores_the_old_reply_and_resumes_only_the_latest_lookup() {
        let mut harness = opened();
        harness.app.open_flow_find_result(0);
        let old = harness.sent();
        assert_eq!(old[0].2["page_index"], 0);

        harness.app.layer_find_query = "query".into();
        harness.app.sync_flow_find_search();
        harness.app.open_flow_find_result(0);
        assert!(harness.sent().is_empty(), "there is still only one lookup request in flight");

        harness.answer(
            &old,
            "render.get",
            Ok(page_snapshot(1, 0, 2, Some(TextRange { paragraph_id: "first".into(), start: 0, end: 6 }))),
        );
        assert_eq!(harness.app.page_index, 0, "the old query cannot move the page");
        let latest = harness.sent();
        assert_eq!(methods(&latest), ["render.get"]);
        assert_eq!(latest[0].2["expected_revision"], 1);
        assert_eq!(latest[0].2["page_index"], 0);

        harness.answer(&latest, "render.get", Ok(page_snapshot(1, 0, 2, None)));
        let next_page = harness.sent();
        assert_eq!(next_page[0].2["page_index"], 1);
        harness.answer(
            &next_page,
            "render.get",
            Ok(page_snapshot(1, 1, 2, Some(TextRange { paragraph_id: "second".into(), start: 4, end: 9 }))),
        );
        let selection = harness.sent();
        assert_eq!(methods(&selection), ["selection.set"]);
        assert_eq!(selection[0].2["selection"]["text_range"], json!({ "paragraph_id": "second", "start": 4, "end": 9 }));
        assert_eq!(harness.app.page_index, 1);
    }

    #[test]
    fn a_revision_change_ignores_the_old_page_and_reads_the_new_revision() {
        let mut harness = opened();
        harness.app.open_flow_find_result(0);
        let old = harness.sent();

        harness.app.active.as_mut().expect("active document").revision = 2;
        harness.app.artifact = Some(artifact(2));
        harness.app.context.as_mut().expect("context").revision = 2;
        harness.app.sync_flow_find_search();
        harness.app.open_flow_find_result(0);
        assert!(harness.sent().is_empty(), "the old page request still owns the single in-flight slot");

        harness.answer(
            &old,
            "render.get",
            Ok(page_snapshot(1, 0, 2, Some(TextRange { paragraph_id: "first".into(), start: 0, end: 6 }))),
        );
        assert_eq!(harness.app.page_index, 0, "a page from an old revision cannot select the new document");
        let latest = harness.sent();
        assert_eq!(methods(&latest), ["render.get"]);
        assert_eq!(latest[0].2["expected_revision"], 2);
        harness.answer(
            &latest,
            "render.get",
            Ok(page_snapshot(2, 0, 2, Some(TextRange { paragraph_id: "first".into(), start: 0, end: 6 }))),
        );
        let selection = harness.sent();
        assert_eq!(methods(&selection), ["selection.set"]);
        assert_eq!(selection[0].2["expected_revision"], 2);
        assert_eq!(harness.app.page_index, 0);
        assert_eq!(harness.app.artifact.as_ref().map(|document| document.revision), Some(2));
    }

    #[test]
    fn next_and_previous_cycle_each_grapheme_match_without_expanding_result_rows() {
        let mut harness = opened_with_first_text("target target");
        assert_eq!(harness.app.flow_find.as_ref().unwrap().matches.len(), 1, "the result list stores one row per matching paragraph");

        harness.app.invoke_ui("layer.find.next", json!({}));
        let render = harness.sent();
        assert_eq!(methods(&render), ["render.get"]);
        harness.answer(
            &render,
            "render.get",
            Ok(page_snapshot(1, 0, 2, Some(TextRange { paragraph_id: "first".into(), start: 0, end: 13 }))),
        );
        let first = harness.sent();
        assert_eq!(methods(&first), ["selection.set"]);
        assert_eq!(first[0].2["selection"]["text_range"], json!({ "paragraph_id": "first", "start": 0, "end": 6 }));

        for (command, expected_range) in [
            ("layer.find.next", (7, 13)),
            ("layer.find.next", (0, 6)),
            ("layer.find.previous", (7, 13)),
        ] {
            harness.app.invoke_ui(command, json!({}));
            let selection = harness.sent();
            assert_eq!(methods(&selection), ["selection.set"], "the cached page source map serves adjacent matches");
            assert_eq!(
                selection[0].2["selection"]["text_range"],
                json!({ "paragraph_id": "first", "start": expected_range.0, "end": expected_range.1 }),
                "{command}"
            );
        }
        assert_eq!(harness.app.artifact.as_ref().map(|document| document.revision), Some(1));
        assert!(harness.message().is_none());
    }

    #[test]
    fn empty_and_unmatched_find_queries_do_not_move_selection_or_request_pages() {
        let mut harness = opened();
        harness.app.layer_find_query.clear();
        harness.app.sync_flow_find_search();
        harness.app.invoke_ui("layer.find.next", json!({}));
        harness.app.invoke_ui("layer.find.previous", json!({}));
        assert!(harness.sent().is_empty(), "an empty query has no navigation target");

        harness.app.layer_find_query = "not in this document".into();
        harness.app.sync_flow_find_search();
        harness.app.invoke_ui("layer.find.next", json!({}));
        harness.app.invoke_ui("layer.find.previous", json!({}));
        assert!(harness.sent().is_empty(), "a query with no results has no navigation target");
        assert!(harness.app.flow_find_cursor.is_none());
        assert_eq!(harness.app.page_index, 0);
        assert_eq!(harness.app.artifact.as_ref().map(|document| document.revision), Some(1));
    }

    #[test]
    fn switching_documents_drops_the_old_find_cursor_and_pending_page_before_selecting() {
        let mut harness = opened_with_first_text("target target");
        harness.app.invoke_ui("layer.find.next", json!({}));
        let old_page = harness.sent();
        assert_eq!(old_page[0].2["artifact_id"], "a1");

        let mut next = artifact_with_first_text(2, "new document");
        next.id = "a2".into();
        harness.app.active.as_mut().expect("active document").id = "a2".into();
        harness.app.active.as_mut().unwrap().revision = 2;
        harness.app.artifact = Some(next);
        harness.app.context.as_mut().expect("context").artifact_id = "a2".into();
        harness.app.context.as_mut().unwrap().revision = 2;
        harness.app.layer_find_query = "new".into();
        harness.app.sync_flow_find_search();
        assert!(harness.app.flow_find_cursor.is_none(), "the old selection cursor does not cross documents");
        harness.app.invoke_ui("layer.find.next", json!({}));
        assert!(harness.sent().is_empty(), "the stale request still holds the one in-flight slot");

        harness.answer(
            &old_page,
            "render.get",
            Ok(page_snapshot_for("a1", 1, 0, 2, Some(TextRange { paragraph_id: "first".into(), start: 0, end: 13 }))),
        );
        assert_eq!(harness.app.page_index, 0, "the old document response cannot move the current page");
        let new_page = harness.sent();
        assert_eq!(methods(&new_page), ["render.get"]);
        assert_eq!(new_page[0].2["artifact_id"], "a2");
        assert_eq!(new_page[0].2["expected_revision"], 2);

        harness.answer(
            &new_page,
            "render.get",
            Ok(page_snapshot_for("a2", 2, 0, 2, Some(TextRange { paragraph_id: "first".into(), start: 0, end: 3 }))),
        );
        let selection = harness.sent();
        assert_eq!(methods(&selection), ["selection.set"]);
        assert_eq!(selection[0].2["artifact_id"], "a2");
        assert_eq!(selection[0].2["expected_revision"], 2);
        assert_eq!(selection[0].2["selection"]["text_range"], json!({ "paragraph_id": "first", "start": 0, "end": 3 }));
    }
}

mod url_post_commit_selection_tests {
    use designllm_contracts::{
        Artifact, ArtifactBody, ArtifactKind, CanvasDocument, Node, NodeKind, Page, Rect, Size, Style,
    };
    use serde_json::json;

    use super::{context, methods, summary, Harness};
    use super::super::{DesktopApp, PendingAction, Screen};
    use crate::rpc::RpcWorker;

    fn canvas() -> Artifact {
        let node = Node {
            id: "node".into(),
            kind: NodeKind::Rectangle,
            bounds: Rect { x: 0, y: 0, width: 100_000, height: 100_000 },
            text: String::new(),
            style: Style::default(),
            asset: None,
            children: Vec::new(),
            layout: None,
            interactions: Vec::new(),
            variants: Default::default(),
            ext: Default::default(),
        };
        let mut artifact = Artifact::new(
            ArtifactKind::Canvas,
            "Canvas",
            "engine",
            ArtifactBody::Canvas(CanvasDocument {
                pages: vec![Page { nodes: vec![node], ..Page::new("page", Size::a4()) }],
                styles: Default::default(),
                ..Default::default()
            }),
        );
        artifact.id = "a1".into();
        artifact.revision = 1;
        artifact
    }

    fn configure(app: &mut DesktopApp, with_context: bool) {
        app.screen = Screen::Editor;
        app.active = Some(serde_json::from_value(summary("a1", "canvas", 1)).expect("summary"));
        app.artifact = Some(canvas());
        app.context = with_context.then(|| serde_json::from_value(context("a1", &app.view_id, 1)).expect("context"));
        app.selected_node = Some("node".into());
        app.selected_nodes = vec!["node".into()];
    }

    #[test]
    fn missing_context_does_not_replace_an_existing_pending_selection() {
        let mut harness = Harness::new();
        harness.sent();
        configure(&mut harness.app, false);
        harness.app.select_after_commit = Some(vec!["previous-target".into()]);
        harness.app.select_after_commit_ticket = Some(71);

        let result = harness.app.invoke("canvas.open_url", json!({ "url": "https://example.test/" }));
        assert!(result.is_err(), "the editor is still loading its context");
        assert_eq!(harness.app.select_after_commit, Some(vec!["previous-target".into()]));
        assert_eq!(harness.app.select_after_commit_ticket, Some(71));
        assert!(harness.sent().is_empty(), "no request was enqueued");
    }

    #[test]
    fn an_rpc_enqueue_failure_keeps_the_previous_apply_selection_owner() {
        let (rpc, _replies, work) = RpcWorker::detached();
        let mut app = DesktopApp::new(rpc, "작업공간".into(), None, None);
        drop(work);
        configure(&mut app, true);
        app.select_after_commit = Some(vec!["previous-target".into()]);
        app.select_after_commit_ticket = Some(72);
        let result = app.invoke("canvas.open_url", json!({ "url": "https://example.test/" }));
        assert!(result.is_ok(), "the RPC layer already shows the enqueue failure");
        assert_eq!(app.select_after_commit, Some(vec!["previous-target".into()]));
        assert_eq!(app.select_after_commit_ticket, Some(72));
        assert!(app.held_edits.is_empty(), "an unsent URL change is not held for retry");
        assert!(app.pending.values().all(|pending| pending.method != "changes.apply"), "failed enqueue is not pending");
        assert!(app.notices.current(std::time::Instant::now()).is_some(), "the transport error is shown");
    }

    #[test]
    fn url_apply_enqueued_while_another_apply_is_pending_does_not_take_its_selection() {
        let mut harness = Harness::new();
        harness.open_document("canvas", 1);
        configure(&mut harness.app, true);
        let existing_ticket = harness
            .app
            .call("changes.apply", json!({}), PendingAction::Apply)
            .expect("an earlier apply is queued");
        let existing = harness.sent();
        assert_eq!(methods(&existing), ["changes.apply"]);
        harness.app.select_after_commit = Some(vec!["previous-target".into()]);
        harness.app.select_after_commit_ticket = Some(existing_ticket);

        assert!(harness.app.invoke("canvas.open_url", json!({ "url": "https://example.test/" })).is_ok());
        let url_change = harness.sent();
        assert_eq!(methods(&url_change), ["changes.apply"]);
        assert_eq!(harness.app.select_after_commit, Some(vec!["previous-target".into()]));
        assert_eq!(harness.app.select_after_commit_ticket, Some(existing_ticket));
        let url_ticket = url_change[0].0;
        assert!(matches!(
            harness.app.pending.get(&url_ticket).map(|pending| &pending.action),
            Some(PendingAction::ApplyWithSelectionIfUnchanged { node_ids, .. }) if node_ids.len() == 1 && node_ids[0] == "node"
        ));
    }
}

mod page_reorder_race_tests {
    use designllm_contracts::{Artifact, ArtifactBody, ArtifactKind, CanvasDocument, DltError, ErrorCode, Page, Size};
    use serde_json::json;

    use super::{context, methods, summary, Harness, RpcReply};
    use super::super::{DesktopApp, Screen};

    fn canvas_pages() -> Artifact {
        let page = |id: &str| Page::new(id, Size::a4());
        let mut artifact = Artifact::new(
            ArtifactKind::Canvas,
            "Page reorder race",
            "engine",
            ArtifactBody::Canvas(CanvasDocument { pages: vec![page("page-a"), page("page-b"), page("page-c")], ..Default::default() }),
        );
        artifact.id = "a1".into();
        artifact.revision = 1;
        artifact
    }

    fn configure(app: &mut DesktopApp) {
        app.screen = Screen::Editor;
        app.active = Some(serde_json::from_value(summary("a1", "canvas", 1)).expect("summary"));
        app.artifact = Some(canvas_pages());
        app.context = Some(serde_json::from_value(context("a1", &app.view_id, 1)).expect("context"));
        app.view().page_index = 1;
        app.page_index = 1;
    }

    fn external_order(app: &mut DesktopApp, ids: [&str; 3], revision: u64) {
        let artifact = app.artifact.as_mut().expect("artifact");
        let ArtifactBody::Canvas(document) = &mut artifact.body else { panic!("Canvas body") };
        let current = document.pages.clone();
        document.pages = ids.iter().map(|id| current.iter().find(|page| page.id == *id).unwrap().clone()).collect();
        artifact.revision = revision;
        app.active.as_mut().expect("active summary").revision = revision;
        let view_id = app.view_id.clone();
        app.context = Some(serde_json::from_value(context("a1", &view_id, revision)).expect("updated context"));
    }

    #[test]
    fn page_reorder_command_owns_pending_state_and_late_failure_cannot_clear_new_order() {
        let mut harness = Harness::new();
        harness.sent();
        configure(&mut harness.app);
        assert!(harness.app.pending_page_reorder_selection.is_none(), "the drop/command path starts without a pre-created pending selection");

        assert!(harness.app.invoke("page.reorder", json!({ "page_index": 0, "to_index": 2 })).is_ok());
        let first_request = harness.sent();
        assert_eq!(methods(&first_request), ["changes.apply"]);
        assert_eq!(first_request[0].2["operations"][0]["op"], "reorder_pages");
        let first_token = harness.app.pending_page_reorder_selection.as_ref().expect("command initializes pending selection").token.clone();
        assert_eq!(harness.app.pending_page_reorder_selection.as_ref().unwrap().selected_page_id.as_deref(), Some("page-b"));

        let duplicate = harness.app.invoke("page.reorder", json!({ "page_index": 0, "to_index": 2 }));
        assert!(duplicate.is_err(), "a competing reorder is refused while the first is pending");
        assert!(harness.sent().is_empty(), "the competing request is not sent to Core");
        assert_eq!(harness.app.pending_page_reorder_selection.as_ref().unwrap().token, first_token);

        harness.app.invoke_ui("nav.page", json!({ "page_index": 2 }));
        assert_eq!(harness.app.pending_page_reorder_selection.as_ref().unwrap().selected_page_id.as_deref(), Some("page-c"),
            "direct nav.page updates the stable selected page ID");

        external_order(&mut harness.app, ["page-c", "page-a", "page-b"], 2);
        harness.app.apply_pending_page_reorder_selection();
        assert_eq!(harness.app.view_ref().page_index, 0, "an unrelated current order remaps the still-present selected page ID");
        assert!(harness.app.pending_page_reorder_selection.is_none(), "the mismatched reorder state is cleared after remapping");

        assert!(harness.app.invoke("page.reorder", json!({ "page_index": 0, "to_index": 1 })).is_ok());
        let second_request = harness.sent();
        assert_eq!(methods(&second_request), ["changes.apply"]);
        let second_token = harness.app.pending_page_reorder_selection.as_ref().expect("new reorder pending").token.clone();
        assert_ne!(second_token, first_token);

        harness.replies.send(RpcReply {
            ticket: first_request[0].0,
            result: Err(DltError::new(ErrorCode::RevisionConflict, "late stale page reorder")),
        }).expect("app listens for the old reply");
        harness.app.process_replies(&harness.ctx);
        assert_eq!(harness.app.pending_page_reorder_selection.as_ref().map(|pending| pending.token.as_str()), Some(second_token.as_str()),
            "a late failure for the old token cannot clear the newer page reorder selection");
    }

    #[test]
    fn page_remove_updates_the_pending_reorder_selection_by_page_id() {
        let mut harness = Harness::new();
        harness.sent();
        configure(&mut harness.app);

        assert!(harness.app.invoke("page.reorder", json!({ "page_index": 0, "to_index": 2 })).is_ok());
        let reorder = harness.sent();
        assert_eq!(methods(&reorder), ["changes.apply"]);
        assert_eq!(harness.app.pending_page_reorder_selection.as_ref().unwrap().selected_page_id.as_deref(), Some("page-b"));

        assert!(harness.app.invoke("page.remove", json!({ "page_index": 1 })).is_ok());
        let removal = harness.sent();
        assert_eq!(methods(&removal), ["selection.set", "changes.apply"]);
        let removal_change = removal.iter().find(|request| request.1 == "changes.apply").expect("page removal ChangeSet");
        assert_eq!(removal_change.2["operations"][0]["op"], "remove_page");
        assert_eq!(harness.app.pending_page_reorder_selection.as_ref().unwrap().selected_page_id.as_deref(), Some("page-a"),
            "page.remove's direct page-index change records the previous page's stable ID");
    }
}

#[path = "board_memo_edit_tests.rs"]
mod board_memo_edit_tests;
