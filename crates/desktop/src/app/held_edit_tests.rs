//! Regressions for direct edits held by another GUI view's target reservation.

use std::sync::mpsc::{Receiver, Sender};

use designllm_contracts::{
    Artifact, ArtifactBody, ArtifactKind, ArtifactSummary, CanvasDocument, DltError, ErrorCode, Intent, Node, Page,
    Size,
};
use eframe::egui::{Context, RawInput, ViewportCommand, ViewportEvent, ViewportId};
use serde_json::{json, Value};

use super::{state::HeldEditState, DesktopApp, PendingAction};
use crate::rpc::{RpcPayload, RpcReply, RpcWorker, Work};
use crate::grapheme::TextInputEdit;

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
        let mut app = DesktopApp::new(rpc, "held edit fixture".into(), None, None);
        while work.try_recv().is_ok() {}
        install_document(&mut app, 1, "원문");
        Self { app, replies, work, ctx: Context::default() }
    }

    fn sent(&mut self) -> Vec<Request> {
        let mut requests = Vec::new();
        while let Ok(work) = self.work.try_recv() {
            if let Work::Call { ticket, method, params } = work {
                requests.push((ticket, method, params));
            }
        }
        requests
    }

    fn answer(&mut self, requests: &[Request], method: &str, result: Result<Value, DltError>) {
        let ticket = requests.iter().rev().find(|request| request.1 == method)
            .map(|request| request.0).expect("request exists");
        self.replies.send(RpcReply { ticket, result: result.map(RpcPayload::Json) }).expect("app is listening");
        self.app.process_replies(&self.ctx);
    }

    fn hold_one_edit(&mut self) -> String {
        self.app.apply_changes(
            vec![designllm_contracts::Operation::SetText { node_id: "text-1".into(), text: "보류 편집".into() }],
            [Intent::Content].into_iter().collect(),
            1,
            PendingAction::Apply,
        );
        let request = self.sent();
        let mut busy = DltError::new(ErrorCode::TargetBusy, "target reserved");
        busy.affected_ids = vec!["text-1".into()];
        busy.retryable = true;
        self.answer(&request, "changes.apply", Err(busy));
        self.app.held_edits.first().expect("held edit").id.clone()
    }
}

fn context_snapshot(view_id: &str, revision: u64) -> Value {
    json!({
        "workspace_id": "w1", "artifact_id": "a1", "revision": revision, "view_id": view_id,
        "selection": { "node_ids": ["text-1"], "text_range": null }, "visible_page": 0,
        "selected_properties": { "nodes": null, "stale_selection": null, "render_unavailable": null },
        "capabilities": {}, "render_snapshot": null, "view_transform": null,
    })
}

fn refresh_held_edit(harness: &mut Harness, held_id: &str, reservations: Value, revision: u64) {
    harness.app.check_held_edit(held_id);
    let refresh = harness.sent();
    assert!(refresh.iter().any(|request| request.1 == "context.read"));
    assert!(refresh.iter().any(|request| request.1 == "artifact.inspect"));
    assert!(refresh.iter().any(|request| request.1 == "targets.list"));
    let view_id = harness.app.view_id.clone();
    harness.answer(&refresh, "targets.list", Ok(reservations));
    harness.answer(&refresh, "context.read", Ok(context_snapshot(&view_id, revision)));
    let inspected = serde_json::to_value(harness.app.artifact.as_ref().expect("fixture artifact")).unwrap();
    harness.answer(&refresh, "artifact.inspect", Ok(inspected));
}

fn close_input() -> RawInput {
    let mut input = RawInput::default();
    input.viewports.get_mut(&ViewportId::ROOT).expect("root viewport").events.push(ViewportEvent::Close);
    input
}

fn has_viewport_command(output: &eframe::egui::FullOutput, expected: ViewportCommand) -> bool {
    output.viewport_output.get(&ViewportId::ROOT).is_some_and(|viewport| viewport.commands.contains(&expected))
}

fn install_document(app: &mut DesktopApp, revision: u64, text: &str) {
    let node: Node = serde_json::from_value(json!({
        "id": "text-1", "kind": "text", "text": text,
        "bounds": { "x": 1000000, "y": 1000000, "width": 3000000, "height": 500000 }
    })).expect("text node");
    let page: Page = serde_json::from_value(json!({
        "id": "page-1", "size": Size::slide(), "nodes": [node], "notes": ""
    })).expect("page");
    let mut artifact = Artifact::new(
        ArtifactKind::Canvas,
        "held edit fixture",
        "canvas.v1",
        ArtifactBody::Canvas(CanvasDocument { pages: vec![page], ..Default::default() }),
    );
    artifact.id = "a1".into();
    artifact.revision = revision;
    let summary = ArtifactSummary {
        id: artifact.id.clone(), kind: artifact.kind, title: artifact.title.clone(), revision,
        engine_id: artifact.engine_id.clone(), dirty: false, modified_at_ms: None, active_jobs: Vec::new(),
    };
    let features = [
        "text", "text_style", "geometry", "pages", "paragraph_format", "native_roundtrip",
        "render_svg", "render_pdf", "render_png", "dataset", "plot", "diagram", "figure_panels",
        "evidence", "figure_links",
    ];
    let supported = json!({ "read": "supported", "render": "supported", "edit": "supported", "preserve": "supported", "write": "supported", "reason": "" });
    let capabilities: serde_json::Map<String, Value> = features.iter().map(|name| ((*name).to_owned(), supported.clone())).collect();
    let mut view_id = app.view_id.clone();
    if view_id.is_empty() { view_id = designllm_contracts::new_id(); }
    app.view_id = view_id.clone();
    let context = serde_json::from_value(json!({
        "workspace_id": "w1", "artifact_id": "a1", "revision": revision, "view_id": view_id,
        "selection": { "node_ids": ["text-1"], "text_range": null }, "visible_page": 0,
        "selected_properties": { "nodes": null, "stale_selection": null, "render_unavailable": null },
        "capabilities": capabilities, "render_snapshot": null, "view_transform": null,
    })).expect("context snapshot");
    app.screen = super::Screen::Editor;
    app.active = Some(summary);
    app.artifact = Some(artifact);
    app.context = Some(context);
    app.reservations_for = Some("a1".into());
    app.selected_node = Some("text-1".into());
    app.selected_nodes = vec!["text-1".into()];
}

#[test]
fn user_reserved_edit_is_kept_until_explicit_refresh_retry_or_cancel() {
    let mut harness = Harness::new();
    let operation = designllm_contracts::Operation::SetText { node_id: "text-1".into(), text: "사용자 변경".into() };
    harness.app.apply_changes(
        vec![operation.clone()], [Intent::Content].into_iter().collect(), 1, PendingAction::Apply,
    );
    let first = harness.sent();
    assert_eq!(first.iter().filter(|request| request.1 == "changes.apply").count(), 1);
    let mut busy = DltError::new(ErrorCode::TargetBusy, "targets are reserved by a desktop inline editor");
    busy.affected_ids = vec!["text-1".into()];
    busy.retryable = true;
    harness.answer(&first, "changes.apply", Err(busy));

    assert_eq!(harness.app.held_edits.len(), 1);
    let held_id = harness.app.held_edits[0].id.clone();
    assert_eq!(harness.app.held_edits[0].state, HeldEditState::NeedsReview);
    assert_eq!(
        serde_json::to_value(&harness.app.held_edits[0].apply.operations).unwrap(),
        serde_json::to_value(vec![operation.clone()]).unwrap(),
    );
    assert!(harness.sent().iter().all(|request| request.1 != "targets.release"), "a refused view never releases another view's reservation");

    // Simulate view A canceling composition and releasing without a targets.released event.
    install_document(&mut harness.app, 1, "원문");
    harness.app.resume_held_edits();
    assert_eq!(harness.app.held_edits.len(), 1, "the user operation survives the silent release boundary");
    assert!(harness.sent().iter().all(|request| request.1 != "changes.apply"), "a stale base is not resent automatically");

    refresh_held_edit(&mut harness, &held_id, json!({ "artifact_id": "a1", "reservations": [{
        "reservation_id": "inline-a", "view_id": "view-a", "owner": "desktop:inline:view-a",
        "node_ids": ["text-1"], "expires_in_ms": 300000
    }] }), 1);
    assert_eq!(harness.app.held_edits[0].state, HeldEditState::NeedsReview, "a still-active user reservation blocks retry");
    assert!(harness.sent().iter().all(|request| request.1 != "changes.apply"), "checking a live reservation does not retry");

    // The owner cancels composition and releases without a targets.released event or a revision change.
    install_document(&mut harness.app, 1, "원문");
    harness.app.resume_held_edits();
    assert_eq!(harness.app.held_edits.len(), 1);
    assert!(harness.sent().iter().all(|request| request.1 != "changes.apply"));
    refresh_held_edit(&mut harness, &held_id, json!({ "artifact_id": "a1", "reservations": [] }), 1);
    assert_eq!(harness.app.held_edits[0].state, HeldEditState::Ready, "the user gets a separate confirmation after refresh");
    assert!(harness.sent().iter().all(|request| request.1 != "changes.apply"), "refresh alone never applies the held edit");

    harness.app.confirm_held_edit_retry(&held_id);
    let retry = harness.sent();
    let change = retry.iter().find(|request| request.1 == "changes.apply").expect("explicit confirmation sends the held operation");
    assert_eq!(change.2["base_revision"], 1);
    assert_eq!(change.2["operations"], serde_json::to_value(vec![operation.clone()]).unwrap());

    let mut stale = DltError::new(ErrorCode::RevisionConflict, "a third view committed after the refresh");
    stale.current_revision = Some(3);
    harness.answer(&retry, "changes.apply", Err(stale));
    assert_eq!(harness.app.held_edits.len(), 1, "a race after confirmation still preserves the operation");
    assert_eq!(harness.app.held_edits[0].state, HeldEditState::NeedsReview);
    assert!(!harness.app.held_edits[0].can_retry, "a revision conflict forbids rebasing this operation");
    assert_eq!(serde_json::to_value(&harness.app.held_edits[0].apply.operations).unwrap(), serde_json::to_value(vec![operation]).unwrap());

    harness.app.discard_held_edit(&held_id);
    assert!(harness.app.held_edits.is_empty(), "discard is explicit");
    assert!(harness.sent().iter().all(|request| request.1 != "targets.release"), "canceling a held change does not release someone else's target");
}

#[test]
fn a_revision_drifted_held_edit_cannot_target_a_recreated_same_id_and_can_be_copied() {
    let mut harness = Harness::new();
    let operation = designllm_contracts::Operation::SetText { node_id: "text-1".into(), text: "기존 대상에 쓸 내용".into() };
    harness.app.apply_changes(
        vec![operation.clone()], [Intent::Content].into_iter().collect(), 1, PendingAction::Apply,
    );
    let request = harness.sent();
    let mut busy = DltError::new(ErrorCode::TargetBusy, "target reserved");
    busy.affected_ids = vec!["text-1".into()];
    busy.retryable = true;
    harness.answer(&request, "changes.apply", Err(busy));
    let held_id = harness.app.held_edits[0].id.clone();

    // The same ID now names another image/text object at a newer revision.
    install_document(&mut harness.app, 2, "다시 만든 다른 객체");
    refresh_held_edit(&mut harness, &held_id, json!({ "artifact_id": "a1", "reservations": [] }), 2);
    let held = harness.app.held_edits.iter().find(|held| held.id == held_id).expect("stale operation remains");
    assert_eq!(held.state, HeldEditState::NeedsReview);
    assert!(!held.can_retry, "a new node with the same ID is not a safe replacement target");

    harness.app.confirm_held_edit_retry(&held_id);
    assert!(harness.sent().iter().all(|request| request.1 != "changes.apply"), "the old operation cannot be rebased to the new target");

    let ctx = harness.ctx.clone();
    let copied = ctx.run(RawInput::default(), |ctx| harness.app.copy_held_edit(ctx, &held_id));
    let text = copied.platform_output.commands.iter().find_map(|command| match command {
        eframe::egui::OutputCommand::CopyText(text) => Some(text.as_str()),
        _ => None,
    }).expect("the preserved operation can be copied");
    let value: Value = serde_json::from_str(text).expect("copied held operation is JSON");
    assert_eq!(value["base_revision"], 1);
    assert_eq!(value["operations"][0]["text"], "기존 대상에 쓸 내용");
    harness.app.discard_held_edit(&held_id);
}

#[test]
fn closing_with_held_edits_requires_a_choice_and_warns_about_unknown_commits() {
    let mut harness = Harness::new();
    harness.hold_one_edit();
    let ctx = harness.ctx.clone();

    let close = ctx.run(close_input(), |ctx| harness.app.guard_held_edit_close(ctx));
    assert!(has_viewport_command(&close, ViewportCommand::CancelClose));
    assert!(harness.app.close_confirmation_open);
    assert_eq!(harness.app.held_edits.len(), 1, "the close request does not erase the edit");
    let warning = harness.app.held_edit_close_warning().expect("close warning");
    assert!(warning.contains("아직 적용되지 않은"));

    let kept = ctx.run(RawInput::default(), |ctx| harness.app.keep_working_after_close_request(ctx));
    assert!(has_viewport_command(&kept, ViewportCommand::CancelClose));
    assert_eq!(harness.app.held_edits.len(), 1, "continuing keeps the edit");

    harness.app.held_edits[0].retry_same_request = true;
    let uncertain = harness.app.held_edit_close_warning().expect("unknown outcome warning");
    assert!(uncertain.contains("이미 작업물에 반영됐을 수"), "the warning says the result may already be committed");
    assert!(uncertain.contains("창을 닫아도 되돌려지지 않습니다"), "the warning says closing does not undo a commit");
    harness.app.close_confirmation_open = true;
    let discarded = ctx.run(RawInput::default(), |ctx| harness.app.discard_held_edits_and_close(ctx));
    assert!(has_viewport_command(&discarded, ViewportCommand::Close));
    assert!(harness.app.held_edits.is_empty(), "discard is explicit");
    assert!(harness.sent().iter().all(|request| request.1 != "targets.release"), "closing never releases a foreign reservation");
}

#[test]
fn closing_inline_preedit_discards_only_unsent_text_and_releases_its_own_reservation() {
    let mut harness = Harness::new();
    assert!(harness.app.begin_inline_text_edit(&harness.ctx, "text-1", 0, None));
    let _ = harness.sent();
    harness.app.text_draft = "원문ㅎ".into();
    let session = harness.app.inline_text_edit.as_mut().expect("inline session");
    session.composing = true;
    session.reservation_id = Some("owned-ime-reservation".into());
    session.reservation_expires = Some(std::time::Instant::now() + std::time::Duration::from_secs(60));

    let ctx = harness.ctx.clone();
    let close = ctx.run(close_input(), |ctx| harness.app.guard_held_edit_close(ctx));
    assert!(has_viewport_command(&close, ViewportCommand::CancelClose));
    assert!(harness.app.held_edit_close_warning().unwrap().contains("아직 적용되지 않은"));
    harness.app.discard_held_edits_and_close(&ctx);
    assert_eq!(harness.app.text_draft, "원문", "discard removes the active preedit from the draft");
    let mut late_text = RawInput::default();
    late_text.events.push(eframe::egui::Event::Text("늦은 입력".into()));
    eframe::App::raw_input_hook(&mut harness.app, &ctx, &mut late_text);
    assert!(late_text.events.is_empty(), "late IME/text events are ignored after explicit discard-to-close");
    assert_eq!(harness.app.text_draft, "원문");
    let release = harness.sent();
    assert!(release.iter().any(|request| request.1 == "targets.release" && request.2["reservation_id"] == "owned-ime-reservation"));
    assert!(release.iter().all(|request| request.1 != "changes.apply"), "an uncommitted preedit is not applied during close");
    harness.answer(&release, "targets.release", Ok(json!({ "released": true })));
    let output = ctx.run(RawInput::default(), |_| {});
    assert!(has_viewport_command(&output, ViewportCommand::Close), "the window closes after its own reservation is released");
    assert!(harness.app.held_edits.is_empty());
}

#[test]
fn closing_while_ime_reserve_is_in_flight_releases_only_its_late_reservation() {
    let mut harness = Harness::new();
    assert!(harness.app.begin_inline_text_edit(&harness.ctx, "text-1", 0, None));
    let _ = harness.sent();
    harness.app.text_draft = "원문ㅎ".into();
    harness.app.inline_text_edit.as_mut().expect("inline session").composing = true;
    harness.app.reserve_inline_text_target();
    let reserve = harness.sent();
    assert!(reserve.iter().any(|request| request.1 == "targets.reserve"));

    let ctx = harness.ctx.clone();
    let close = ctx.run(close_input(), |ctx| harness.app.guard_held_edit_close(ctx));
    assert!(has_viewport_command(&close, ViewportCommand::CancelClose));
    harness.app.discard_held_edits_and_close(&ctx);
    assert!(harness.app.close_after_inline_cleanup, "closing waits for the reservation result");
    assert!(harness.sent().iter().all(|request| request.1 != "targets.release"));

    harness.answer(&reserve, "targets.reserve", Ok(json!({ "reservation_id": "late-owned-reservation", "expires_in_ms": 300000 })));
    let _ = ctx.run(RawInput::default(), |ctx| harness.app.advance_inline_text_close_cleanup(ctx));
    let release = harness.sent();
    assert!(release.iter().any(|request| request.1 == "targets.release" && request.2["reservation_id"] == "late-owned-reservation"));
    harness.answer(&release, "targets.release", Ok(json!({ "released": true })));
    let output = ctx.run(RawInput::default(), |_| {});
    assert!(has_viewport_command(&output, ViewportCommand::Close), "late reservation reply is released before close");
}

#[test]
fn closing_with_inline_change_in_flight_waits_for_reply_and_releases_before_exit() {
    let mut harness = Harness::new();
    assert!(harness.app.begin_inline_text_edit(&harness.ctx, "text-1", 0, None));
    let _ = harness.sent();
    let edit = TextInputEdit {
        request_id: designllm_contracts::new_id(), range: 0..1, paragraph_index: 0, paragraph_range: Some(0..1),
        caret_after: 2, paragraph_after: 0, caret_after_in_paragraph: 2, text: "교체".into(),
        ime_commit: false, ime_generation: 0, after_text: None,
    };
    let session = harness.app.inline_text_edit.as_mut().expect("inline session");
    session.reservation_id = Some("owned-ime-reservation".into());
    session.queue.push_back(edit);
    harness.app.text_draft = "교체문".into();
    harness.app.start_next_inline_text_change();
    let change = harness.sent();
    assert!(change.iter().any(|request| request.1 == "changes.apply"));

    let ctx = harness.ctx.clone();
    let close = ctx.run(close_input(), |ctx| harness.app.guard_held_edit_close(ctx));
    assert!(has_viewport_command(&close, ViewportCommand::CancelClose));
    let warning = harness.app.held_edit_close_warning().unwrap();
    assert!(warning.contains("이미 작업물에 반영됐을 수"), "an in-flight edit may already be committed");
    assert!(warning.contains("창을 닫아도 되돌려지지 않습니다"), "closing does not undo a possible commit");
    harness.app.discard_held_edits_and_close(&ctx);
    assert!(harness.app.close_after_inline_cleanup, "discard waits for the in-flight result");
    assert!(harness.sent().iter().all(|request| request.1 != "targets.release"), "the own reservation stays until changes.apply resolves");
    let mut late_text = RawInput::default();
    late_text.events.push(eframe::egui::Event::Text("추가 입력".into()));
    eframe::App::raw_input_hook(&mut harness.app, &ctx, &mut late_text);
    assert!(late_text.events.is_empty(), "new typing cannot race the close cleanup");

    harness.answer(&change, "changes.apply", Ok(json!({ "revision": 2, "changed_ids": ["text-1"], "diagnostics": [] })));
    let release = harness.sent();
    assert!(release.iter().any(|request| request.1 == "targets.release" && request.2["reservation_id"] == "owned-ime-reservation"));
    assert!(release.iter().all(|request| request.1 != "changes.apply"), "close does not replay or undo committed input");
    harness.answer(&release, "targets.release", Ok(json!({ "released": true })));
    let output = ctx.run(RawInput::default(), |_| {});
    assert!(has_viewport_command(&output, ViewportCommand::Close), "close follows the in-flight result and own reservation release");
}

#[test]
fn closing_during_general_inline_release_waits_and_keeps_failed_release_retryable() {
    let mut harness = Harness::new();
    assert!(harness.app.begin_inline_text_edit(&harness.ctx, "text-1", 0, None));
    let _ = harness.sent();
    let session_id = harness.app.inline_text_edit.as_ref().expect("inline session").id;
    harness.app.inline_text_edit.as_mut().unwrap().reservation_id = Some("owned-ime-reservation".into());

    harness.app.release_inline_text_target(session_id);
    let first_release = harness.sent();
    assert!(first_release.iter().any(|request| request.1 == "targets.release"));
    let mut refused = DltError::new(ErrorCode::TargetBusy, "release was refused");
    refused.retryable = true;
    harness.answer(&first_release, "targets.release", Err(refused));
    let session = harness.app.inline_text_edit.as_ref().expect("failed release keeps its session");
    assert_eq!(session.reservation_id.as_deref(), Some("owned-ime-reservation"), "the owned lease ID is restored after failure");
    assert!(session.reservation_release_failed);

    harness.app.release_inline_text_target(session_id);
    let retry = harness.sent();
    assert!(retry.iter().any(|request| request.1 == "targets.release" && request.2["reservation_id"] == "owned-ime-reservation"));
    let ctx = harness.ctx.clone();
    let close = ctx.run(close_input(), |ctx| harness.app.guard_held_edit_close(ctx));
    assert!(has_viewport_command(&close, ViewportCommand::CancelClose));
    harness.app.discard_held_edits_and_close(&ctx);
    assert!(harness.app.close_release_pending, "close waits for the already-running general release request");
    assert!(harness.sent().iter().all(|request| request.1 != "targets.release"), "close does not send a duplicate release");

    let mut second_refusal = DltError::new(ErrorCode::TargetBusy, "release was refused again");
    second_refusal.retryable = true;
    harness.answer(&retry, "targets.release", Err(second_refusal));
    assert!(harness.app.close_release_failed);
    assert_eq!(harness.app.close_reservation_id.as_deref(), Some("owned-ime-reservation"), "close recovery keeps the failed lease ID");
    assert!(harness.app.held_edit_close_warning().unwrap().contains("만료"), "the user is told that waiting for expiry is an option");

    harness.app.retry_close_reservation_release();
    let final_release = harness.sent();
    assert!(final_release.iter().any(|request| request.1 == "targets.release" && request.2["reservation_id"] == "owned-ime-reservation"));
    harness.answer(&final_release, "targets.release", Ok(json!({ "released": true })));
    let output = ctx.run(RawInput::default(), |_| {});
    assert!(has_viewport_command(&output, ViewportCommand::Close), "close completes only after the retried release is acknowledged");
}
