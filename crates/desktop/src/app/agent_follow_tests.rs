use super::{test_support::*, *};

fn event(seq: u64, revision: u64, artifact: &str) -> Value {
    json!({ "events": [{ "event_seq": seq, "artifact_id": artifact, "revision": revision,
        "event": "artifact.changed", "detail": { "actor": "mcp:c1:claude" } }] })
}

#[test]
fn listing_before_event_still_reloads_and_duplicate_or_older_revisions_do_not() {
    let mut h = Harness::new();
    h.open_document("canvas", 1);
    h.app.handle_events(event(1, 1, "a1"));
    let listing = h.sent();
    h.answer(&listing, "workspace.list", Ok(json!({ "workspace_id": "w1", "artifacts": [summary("a1", "canvas", 2)] })));
    assert_eq!(h.app.active.as_ref().unwrap().revision, 1);
    h.app.handle_events(event(2, 2, "a1"));
    let reload = h.sent();
    assert_eq!(methods(&reload), reload_methods());
    assert_eq!(reload[3].2["expected_revision"], 2);
    h.app.handle_events(event(3, 2, "a1"));
    assert_eq!(methods(&h.sent()), ["workspace.list"]);
    h.app.handle_events(event(4, 1, "a1"));
    assert_eq!(methods(&h.sent()), ["workspace.list"]);
    assert_eq!(h.app.active.as_ref().unwrap().revision, 2);
}

#[test]
fn late_context_cannot_replace_newer_revision_or_another_document() {
    let mut h = Harness::new();
    let (_, view) = h.open_document("canvas", 1);
    h.app.handle_events(event(1, 2, "a1"));
    let reload = h.sent();
    h.answer(&reload, "context.read", Ok(context("a1", &view, 2)));
    h.app.handle_payload(&h.ctx, 0, PendingAction::Context, RpcPayload::Json(context("a1", &view, 1)));
    assert_eq!(h.app.context.as_ref().unwrap().revision, 2);
    assert_eq!(h.app.active.as_ref().unwrap().revision, 2);
    h.app.handle_payload(&h.ctx, 0, PendingAction::Context, RpcPayload::Json(context("other", &view, 3)));
    assert_eq!(h.app.context.as_ref().unwrap().revision, 2);
}

#[test]
fn late_inspect_does_not_replace_a_newer_artifact() {
    let mut h = Harness::new();
    h.open_document("canvas", 1);
    let mut artifact = Artifact::new(ArtifactKind::Canvas, "new", "engine",
        ArtifactBody::Canvas(designllm_contracts::CanvasDocument::default()));
    artifact.id = "a1".into();
    artifact.revision = 2;
    h.app.handle_events(event(1, 2, "a1"));
    h.app.handle_payload(&h.ctx, 0, PendingAction::Inspect, RpcPayload::Json(serde_json::to_value(&artifact).unwrap()));
    artifact.revision = 1;
    artifact.title = "old".into();
    h.app.handle_payload(&h.ctx, 0, PendingAction::Inspect, RpcPayload::Json(serde_json::to_value(&artifact).unwrap()));
    assert_eq!(h.app.artifact.as_ref().unwrap().revision, 2);
    assert_eq!(h.app.artifact.as_ref().unwrap().title, "new");
    assert_eq!(h.app.active.as_ref().unwrap().revision, 2);
}

fn created(seq: u64, artifact: &str) -> Value {
    json!({ "events": [{ "event_seq": seq, "artifact_id": artifact, "revision": 0,
        "event": "artifact.created", "detail": { "actor": "mcp:c1:claude" } }] })
}

fn listing() -> Value {
    json!({ "workspace_id": "w1", "artifacts": [summary("a1", "canvas", 1), summary("a2", "canvas", 0)] })
}

fn job_event(seq: u64, status: &str) -> Value {
    json!({ "events": [{ "event_seq": seq, "artifact_id": "a1", "revision": 1,
        "event": "job.status", "detail": { "job_id": "job-1", "status": status } }] })
}

fn job(status: &str) -> Value {
    json!({ "id": "job-1", "attempt_id": "attempt-1", "artifact_id": "a1", "base_revision": 1,
        "status": status, "allowed_intents": ["content"], "model": "mock", "effort": "high", "error": null })
}

#[test]
fn agent_created_document_opens_from_home_and_idle_editor() {
    for home in [true, false] {
        let mut h = Harness::new();
        h.open_document("canvas", 1);
        if home { h.app.screen = Screen::Home; }
        h.app.handle_events(created(1, "a2"));
        let requests = h.sent();
        h.answer(&requests, "workspace.list", Ok(listing()));
        assert_eq!(h.app.active.as_ref().map(|active| active.id.as_str()), Some("a2"));
        assert_eq!(h.app.screen, Screen::Editor);
    }
}

#[test]
fn editing_keeps_document_and_offers_view_without_dropping_text() {
    let mut h = Harness::new();
    h.open_document("canvas", 1);
    h.app.text_target = Some(TextTarget::Node { node_id: "n".into(), original: "old".into(), editable: true });
    h.app.text_draft = "unsaved".into();
    h.app.handle_events(created(1, "a2"));
    let requests = h.sent();
    h.answer(&requests, "workspace.list", Ok(listing()));
    assert_eq!(h.app.active.as_ref().unwrap().id, "a1");
    assert_eq!(h.app.agent_follow.notices.front().map(String::as_str), Some("a2"));
    assert_eq!(h.message().unwrap().1, messages::AGENT_FOLLOW_NOTICE);
    let ctx = h.ctx.clone();
    crate::theme::install(&ctx);
    let _ = ctx.run(screen(1.0, 900.0, 600.0), |ctx| h.app.draw_error_callout(ctx));
    let output = ctx.run(screen(1.0, 900.0, 600.0), |ctx| h.app.draw_error_callout(ctx));
    assert!(drawn_text(&output).iter().any(|text| text == "보기"));
    h.app.open_agent_notice();
    assert_eq!(h.app.active.as_ref().unwrap().id, "a1");
    assert_eq!(h.app.text_draft, "unsaved");
    h.app.text_draft = "old".into();
    h.app.open_agent_notice();
    assert_eq!(h.app.active.as_ref().unwrap().id, "a2");
}

#[test]
fn changed_other_document_follows_and_stale_listing_cannot_switch_back() {
    let mut h = Harness::new();
    h.open_document("canvas", 1);
    h.app.handle_events(event(1, 1, "a2"));
    let older = h.sent();
    h.app.handle_events(event(2, 2, "a3"));
    let newer = h.sent();
    h.answer(&newer, "workspace.list", Ok(json!({ "workspace_id": "w1", "artifacts": [
        summary("a1", "canvas", 1), summary("a2", "canvas", 1), summary("a3", "canvas", 2)] })));
    assert_eq!(h.app.active.as_ref().unwrap().id, "a3");
    h.answer(&older, "workspace.list", Ok(listing()));
    assert_eq!(h.app.active.as_ref().unwrap().id, "a3");
    h.app.handle_events(event(3, 1, "a2"));
    assert_eq!(methods(&h.sent()).iter().filter(|method| **method == "workspace.list").count(), 2);
    assert_eq!(h.app.active.as_ref().unwrap().id, "a3");
}

#[test]
fn external_job_is_shown_and_late_status_cannot_restart_it() {
    let mut h = Harness::new();
    h.open_document("canvas", 1);
    h.app.handle_events(job_event(1, "running"));
    let running = h.sent();
    h.app.handle_events(job_event(2, "completed"));
    let completed = h.sent();
    h.answer(&completed, "job.get", Ok(job("completed")));
    assert_eq!(h.app.job.as_ref().unwrap().status, JobStatus::Completed);
    h.answer(&running, "job.get", Ok(job("running")));
    assert_eq!(h.app.job.as_ref().unwrap().status, JobStatus::Completed);
}

#[test]
fn external_running_job_sets_status_and_faster_polling_then_stops() {
    let mut h = Harness::new();
    h.open_document("canvas", 1);
    assert_eq!(h.app.agent_poll_interval(), Duration::from_secs(2));
    h.app.handle_events(job_event(1, "running"));
    let requests = h.sent();
    h.answer(&requests, "job.get", Ok(job("running")));
    assert_eq!(h.app.job.as_ref().unwrap().id, "job-1");
    assert_eq!(h.app.agent_poll_interval(), Duration::from_millis(500));
    assert!(h.app.agent_status(Instant::now()).unwrap().contains("AI 편집 중"));
    h.app.handle_events(job_event(2, "completed"));
    let requests = h.sent();
    h.answer(&requests, "job.get", Ok(job("completed")));
    assert_eq!(h.app.agent_poll_interval(), Duration::from_secs(2));
    assert!(h.app.agent_status(Instant::now()).is_none());
}

#[test]
fn figure_export_popover_survives_agent_status_start_and_finish() {
    let mut h = Harness::new();
    h.open_document("figure", 1);
    crate::theme::install(&h.ctx);
    h.app.exit_ctx = Some(h.ctx.clone());
    h.app.smoke_open = Some("export".into());
    let ctx = h.ctx.clone();
    let draw = |app: &mut DesktopApp| ctx.run(screen(1.0, 1440.0, 900.0), |ctx| { app.draw_frame(ctx); });
    let _ = draw(&mut h.app);
    let before = draw(&mut h.app);
    for text in ["SVG", "PNG", "PDF", "PPTX"] {
        assert!(drawn_text(&before).iter().any(|label| label == text), "missing {text}");
    }

    h.app.job = Some(serde_json::from_value(job("running")).unwrap());
    assert!(h.app.agent_status(Instant::now()).is_some());
    let started = draw(&mut h.app);
    for text in ["SVG", "PNG", "PDF", "PPTX"] {
        assert!(drawn_text(&started).iter().any(|label| label == text), "missing {text} after agent starts");
    }

    h.app.job.as_mut().unwrap().status = JobStatus::Completed;
    assert!(h.app.agent_status(Instant::now()).is_none());
    let finished = draw(&mut h.app);
    for text in ["SVG", "PNG", "PDF", "PPTX"] {
        assert!(drawn_text(&finished).iter().any(|label| label == text), "missing {text} after agent finishes");
    }
}

#[test]
fn agent_refresh_defers_selection_sync_while_a_popover_is_open() {
    let mut h = Harness::new();
    let (_, view) = h.open_document("figure", 1);
    crate::theme::install(&h.ctx);
    h.app.exit_ctx = Some(h.ctx.clone());
    h.app.set_selected_ids(vec!["old".into()]);
    h.app.smoke_open = Some("export".into());
    let ctx = h.ctx.clone();
    let _ = ctx.run(screen(1.0, 1440.0, 900.0), |ctx| { h.app.draw_frame(ctx); });
    assert!(egui::Popup::is_any_open(&ctx));

    h.app.handle_events(event(1, 2, "a1"));
    let requests = h.sent();
    let mut updated = context("a1", &view, 2);
    updated["selection"]["node_ids"] = json!(["new"]);
    h.answer(&requests, "context.read", Ok(updated));
    assert_eq!(h.app.context.as_ref().unwrap().revision, 2);
    assert_eq!(h.app.selected_node.as_deref(), Some("old"));
    let shown = ctx.run(screen(1.0, 1440.0, 900.0), |ctx| { h.app.draw_frame(ctx); });
    assert!(drawn_text(&shown).iter().any(|label| label == "SVG"));

    egui::Popup::close_all(&ctx);
    h.app.resume_deferred_agent_revision();
    assert_eq!(h.app.selected_node.as_deref(), Some("new"));
}

#[test]
fn late_job_reply_cannot_follow_a_document_switch() {
    let mut h = Harness::new();
    h.open_document("canvas", 1);
    h.app.handle_events(job_event(1, "running"));
    let requests = h.sent();
    h.app.artifacts.push(serde_json::from_value(summary("a2", "canvas", 0)).unwrap());
    h.app.invoke_ui("nav.open_project", json!({ "artifact_id": "a2" }));
    h.answer(&requests, "job.get", Ok(job("running")));
    assert_eq!(h.app.active.as_ref().unwrap().id, "a2");
    assert!(h.app.job.is_none());
}

#[test]
fn core_created_events_include_the_actor_for_direct_and_job_creation() {
    let mut host = Host::start();
    host.call("artifact.create", json!({ "request_id": new_id(), "actor": "claude", "kind": "canvas",
        "title": "direct", "size": null, "template": null })).unwrap();
    host.call("job.start", json!({ "request_id": new_id(), "artifact_id": "", "base_revision": 0,
        "allowed_intents": ["content"], "runtime": "mock", "model": "mock", "effort": "high", "prompt": "draw",
        "create": { "actor": "codex", "kind": "canvas", "title": "job", "size": null, "template": null } })).unwrap();
    let batch = host.call("events.poll", json!({ "after_seq": 0, "limit": 100 })).unwrap();
    let actors: Vec<&str> = batch["events"].as_array().unwrap().iter()
        .filter(|event| event["event"] == "artifact.created")
        .filter_map(|event| event["detail"]["actor"].as_str()).collect();
    assert_eq!(actors, ["claude", "codex"]);
}

#[test]
fn older_workspace_listing_cannot_replace_the_newly_opened_document() {
    let mut h = Harness::new();
    h.open_document("canvas", 1);
    h.app.handle_events(event(1, 1, "a1"));
    let old_listing = h.sent();
    h.app.handle_events(created(2, "a2"));
    let new_listing = h.sent();
    h.answer(&new_listing, "workspace.list", Ok(listing()));
    assert_eq!(h.app.active.as_ref().unwrap().id, "a2");
    h.answer(&old_listing, "workspace.list", Ok(json!({ "workspace_id": "w1", "artifacts": [summary("a1", "canvas", 1)] })));
    assert_eq!(h.app.active.as_ref().unwrap().id, "a2");
}

#[test]
fn refreshing_the_open_document_keeps_an_unsubmitted_text_draft() {
    let mut h = Harness::new();
    let (_, view) = h.open_document("canvas", 1);
    h.app.text_target = Some(TextTarget::Node { node_id: "n".into(), original: "old".into(), editable: true });
    h.app.text_draft = "unfinished".into();
    h.app.text_editor_focused = true;
    h.app.handle_events(event(1, 2, "a1"));
    let requests = h.sent();
    h.answer(&requests, "context.read", Ok(context("a1", &view, 2)));
    let mut artifact = Artifact::new(ArtifactKind::Canvas, "changed", "engine",
        ArtifactBody::Canvas(designllm_contracts::CanvasDocument::default()));
    artifact.id = "a1".into();
    artifact.revision = 2;
    h.answer(&requests, "artifact.inspect", Ok(serde_json::to_value(artifact).unwrap()));
    assert_eq!(h.app.text_draft, "unfinished");
    assert!(h.app.text_editor_focused);
    assert!(matches!(&h.app.text_target, Some(TextTarget::Node { .. })));
}

#[test]
fn agent_status_tracks_commit_and_expires_when_idle() {
    let mut h = Harness::new();
    h.open_document("canvas", 1);
    let before = Instant::now();
    h.app.handle_events(json!({ "events": [{ "event_seq": 1, "artifact_id": "a1", "revision": 2,
        "event": "artifact.changed", "detail": { "actor": "mcp:c1:claude", "changed_ids": ["x", "y"] } }] }));
    let status = h.app.agent_status(Instant::now()).unwrap();
    assert!(status.contains("리비전 2"));
    assert!(status.contains("변경 2개"));
    assert_eq!(h.app.agent_poll_interval(), Duration::from_millis(500));
    assert!(h.app.agent_status(before + Duration::from_secs(10)).is_none());
}

#[test]
fn a_newer_regular_listing_does_not_cancel_the_pending_follow() {
    let mut h = Harness::new();
    h.open_document("canvas", 1);
    h.app.handle_events(created(1, "a2"));
    let follow = h.sent();
    h.app.call("workspace.list", json!({}), PendingAction::WorkspaceList);
    let newer = h.sent();
    h.answer(&newer, "workspace.list", Ok(listing()));
    h.answer(&follow, "workspace.list", Ok(listing()));
    let retry = h.sent();
    h.answer(&retry, "workspace.list", Ok(listing()));
    assert_eq!(h.app.active.as_ref().unwrap().id, "a2");
}

#[test]
fn agent_follow_keeps_a_running_figure_job_in_the_background() {
    let mut h = Harness::new();
    h.open_document("figure", 1);
    h.app.record_started_job(serde_json::from_value(job("running")).unwrap());
    h.app.handle_events(created(1, "a2"));
    let requests = h.sent();
    h.answer(&requests, "workspace.list", Ok(json!({ "workspace_id": "w1", "artifacts": [
        summary("a1", "figure", 1), summary("a2", "canvas", 0)] })));
    assert_eq!(h.app.active.as_ref().unwrap().id, "a2");
    assert!(h.app.pending_switch.is_none());
    assert!(h.app.parked_jobs.iter().any(|job| job.id == "job-1" && job.status == JobStatus::Running));
}

#[test]
fn a_document_created_by_the_window_is_not_followed_as_an_agent_event() {
    let mut h = Harness::new();
    h.open_document("canvas", 1);
    let mut value = created(1, "a2");
    value["events"][0]["detail"]["actor"] = json!("desktop");
    h.app.handle_events(value);
    let requests = h.sent();
    h.answer(&requests, "workspace.list", Ok(listing()));
    assert_eq!(h.app.active.as_ref().unwrap().id, "a1");
}

#[test]
fn ui_channel_commit_is_not_followed_but_offscreen_commit_from_same_actor_is() {
    let mut h = Harness::new();
    h.open_document("canvas", 1);
    h.app.run_context = Some(super::ui_channel::RunContext {
        run_id: "run-1".into(),
        deadline: Instant::now() + Duration::from_secs(30),
        command_id: "home.create".into(),
        actor: "mcp:c1:agent".into(),
        allowed_intents: Default::default(),
        await_rpc_reply: true,
        mutation_attempted: false,
        rpc_ticket: None,
        enqueue_error: None,
        expected: None,
        prepared_asset: None,
    });
    h.app.call("artifact.create", json!({ "request_id": "ui-request", "actor": "desktop" }), PendingAction::Create);
    let sent = h.sent();
    assert_eq!(sent[0].2["actor"], "mcp:c1:agent");
    h.app.run_context = None;
    h.app.ai_prompt = "editing".into();

    let mut from_ui = created(1, "a2");
    from_ui["events"][0]["detail"]["actor"] = json!("mcp:c1:agent");
    from_ui["events"][0]["detail"]["request_id"] = json!("ui-request");
    h.app.handle_events(from_ui);
    assert!(h.app.agent_follow.notices.is_empty());

    let mut offscreen = created(2, "a3");
    offscreen["events"][0]["detail"]["actor"] = json!("mcp:c1:agent");
    offscreen["events"][0]["detail"]["request_id"] = json!("external-request");
    h.app.handle_events(offscreen);
    assert_eq!(h.app.agent_follow.notices.front().map(String::as_str), Some("a3"));
}

#[test]
fn window_commit_is_recognized_after_its_reply_and_navigation() {
    let mut h = Harness::new();
    h.open_document("canvas", 1);
    h.app.call("artifact.create", json!({ "request_id": "ABCDEFAB-1234-4000-8000-ABCDEFABCDEF", "actor": "mcp:c1:agent" }), PendingAction::Create);
    let sent = h.sent();
    h.answer(&sent, "artifact.create", Ok(summary("a2", "canvas", 0)));
    h.app.invoke_ui("nav.home", json!({}));
    let mut value = created(1, "a2");
    value["events"][0]["detail"]["actor"] = json!("mcp:c1:agent");
    value["events"][0]["detail"]["request_id"] = json!("abcdefab-1234-4000-8000-abcdefabcdef");
    h.app.handle_events(value);
    let requests = h.sent();
    h.answer(&requests, "workspace.list", Ok(listing()));
    assert_eq!(h.app.screen, Screen::Home);
    assert!(h.app.agent_follow.notices.is_empty());
}

#[test]
fn window_started_home_job_does_not_follow_its_creation_event() {
    let mut h = Harness::new();
    h.open_document("canvas", 1);
    let mut started = job("running");
    started["artifact_id"] = json!("a2");
    h.app.handle_payload(&h.ctx, 0, PendingAction::HomeCreate { prompt: "test".into() },
        RpcPayload::Json(json!({ "artifact": summary("a2", "canvas", 0), "job": started })));
    h.app.invoke_ui("nav.home", json!({}));
    let mut value = created(1, "a2");
    value["events"][0]["detail"]["job_id"] = json!("job-1");
    value["events"][0]["detail"]["request_id"] = json!("generated-in-worker");
    h.app.handle_events(value);
    let requests = h.sent();
    h.answer(&requests, "workspace.list", Ok(listing()));
    assert_eq!(h.app.screen, Screen::Home);
    assert!(h.app.agent_follow.notices.is_empty());
}

#[test]
fn applied_proposal_reply_marks_its_commit_as_the_windows() {
    let mut h = Harness::new();
    h.open_document("canvas", 1);
    h.app.handle_payload(&h.ctx, 0, PendingAction::Apply,
        RpcPayload::Json(json!({ "request_id": "proposal-request", "artifact_id": "a1", "revision": 2 })));
    h.app.ai_prompt = "editing".into();
    let mut value = event(1, 2, "a2");
    value["events"][0]["detail"]["request_id"] = json!("proposal-request");
    h.app.handle_events(value);
    assert!(h.app.agent_follow.notices.is_empty());
}

#[test]
fn window_commit_is_kept_until_a_split_event_batch_is_drained() {
    let mut h = Harness::new();
    h.open_document("canvas", 1);
    h.app.call("changes.apply", json!({ "request_id": "grouped", "actor": "mcp:c1:agent" }), PendingAction::Apply);
    h.sent();
    h.app.ai_prompt = "editing".into();
    let mut first = event(1, 1, "a2");
    first["events"][0]["detail"]["request_id"] = json!("grouped");
    first["last_seq"] = json!(2);
    h.app.handle_events(first);
    let mut second = event(2, 1, "a3");
    second["events"][0]["detail"]["request_id"] = json!("grouped");
    second["last_seq"] = json!(2);
    h.app.handle_events(second);
    assert!(h.app.agent_follow.notices.is_empty());
}

#[test]
fn home_in_a_paginated_poll_does_not_suppress_the_next_page() {
    let mut h = Harness::new();
    h.open_document("canvas", 1);
    let run = designllm_contracts::UiRun {
        run_id: new_id(), view_id: new_id(), expected: None, command_id: "nav.home".into(), args: json!({}),
        actor: "mcp:c1:agent".into(), permission: designllm_contracts::UiPermission::Read,
        allowed_intents: Default::default(), status: designllm_contracts::UiRunStatus::Queued,
        timeout_ms: 10_000, result: None, error: None,
    };
    let events: Vec<_> = (1..=256).map(|seq| json!({ "event_seq": seq, "event": "other" })).collect();
    h.app.handle_events(json!({ "events": events, "last_seq": 700, "ui_runs": [run] }));
    assert_eq!(h.app.screen, Screen::Home);
    h.sent();
    let mut next = created(257, "a2");
    next["last_seq"] = json!(700);
    h.app.handle_events(next);
    let requests = h.sent();
    h.answer(&requests, "workspace.list", Ok(listing()));
    assert_eq!(h.app.active.as_ref().unwrap().id, "a2");
    assert_eq!(h.app.screen, Screen::Editor);
}

#[test]
fn completed_window_commits_are_released_during_continuous_pagination() {
    let mut h = Harness::new();
    h.open_document("canvas", 1);
    h.app.ai_prompt = "editing".into();
    for page in 0..30_u64 {
        let id = format!("window-{page}");
        h.app.call("changes.apply", json!({ "request_id": id, "actor": "mcp:c1:agent" }), PendingAction::Apply).unwrap();
        let request = h.sent();
        h.answer(&request, "changes.apply", Ok(json!({ "request_id": id, "artifact_id": "a1", "revision": 2 })));
        h.sent();
        h.app.call("events.poll", json!({}), PendingAction::PollEvents).unwrap();
        let poll = h.sent();
        let start = page * 256 + 1;
        let end = start + 255;
        let mut events: Vec<_> = (start..=end).map(|seq| json!({ "event_seq": seq, "event": "other" })).collect();
        events[0] = json!({ "event_seq": start, "event": "artifact.changed", "artifact_id": "a2", "revision": page + 1,
            "detail": { "actor": "mcp:c1:agent", "request_id": id } });
        h.answer(&poll, "events.poll", Ok(json!({ "events": events, "last_seq": end + 768 })));
        assert!(h.app.tracked_window_commits() <= 4);
        assert!(h.app.agent_follow.notices.is_empty());
    }
}

#[test]
fn delayed_window_commit_stays_tracked_until_completion_and_delivery() {
    let mut h = Harness::new();
    h.open_document("canvas", 1);
    h.app.call("artifact.create", json!({ "request_id": "delayed", "actor": "mcp:c1:agent" }), PendingAction::Create).unwrap();
    let request = h.sent();
    h.app.prune_agent_follow(Instant::now() + Duration::from_secs(7200));
    assert_eq!(h.app.tracked_window_commits(), 1);
    h.answer(&request, "artifact.create", Ok(summary("a2", "canvas", 0)));
    h.app.invoke_ui("nav.home", json!({}));
    h.app.prune_agent_follow(Instant::now() + Duration::from_secs(7200));
    assert_eq!(h.app.tracked_window_commits(), 1);
    h.sent();
    h.app.call("events.poll", json!({}), PendingAction::PollEvents).unwrap();
    let poll = h.sent();
    let mut value = created(1, "a2");
    value["last_seq"] = json!(1);
    value["events"][0]["detail"]["request_id"] = json!("delayed");
    h.answer(&poll, "events.poll", Ok(value));
    assert_eq!(h.app.screen, Screen::Home);
    assert!(h.app.agent_follow.notices.is_empty());
    assert_eq!(h.app.tracked_window_commits(), 0);
}

#[test]
fn a_poll_sent_before_the_window_commit_cannot_retire_its_id() {
    let mut h = Harness::new();
    h.open_document("canvas", 1);
    h.app.call("events.poll", json!({}), PendingAction::PollEvents).unwrap();
    let old_poll = h.sent();
    h.app.call("artifact.create", json!({ "request_id": "late", "actor": "mcp:c1:agent" }), PendingAction::Create).unwrap();
    let request = h.sent();
    h.answer(&request, "artifact.create", Ok(summary("a2", "canvas", 0)));
    h.app.invoke_ui("nav.home", json!({}));
    h.answer(&old_poll, "events.poll", Ok(json!({ "events": [], "last_seq": 0 })));
    assert_eq!(h.app.tracked_window_commits(), 1);
    h.sent();
    h.app.call("events.poll", json!({}), PendingAction::PollEvents).unwrap();
    let new_poll = h.sent();
    let mut value = created(1, "a2");
    value["last_seq"] = json!(1);
    value["events"][0]["detail"]["request_id"] = json!("late");
    h.answer(&new_poll, "events.poll", Ok(value));
    assert_eq!(h.app.screen, Screen::Home);
    assert!(h.app.agent_follow.notices.is_empty());
    assert_eq!(h.app.tracked_window_commits(), 0);
}

#[test]
fn failed_window_request_is_released_after_a_later_poll() {
    let mut h = Harness::new();
    h.open_document("canvas", 1);
    h.app.call("artifact.create", json!({ "request_id": "refused" }), PendingAction::Create).unwrap();
    let request = h.sent();
    h.answer(&request, "artifact.create", Err(DltError::new(ErrorCode::InvalidArgument, "refused")));
    assert_eq!(h.app.tracked_window_commits(), 1);
    h.sent();
    h.app.call("events.poll", json!({}), PendingAction::PollEvents).unwrap();
    let poll = h.sent();
    h.answer(&poll, "events.poll", Ok(json!({ "events": [], "last_seq": 0 })));
    assert_eq!(h.app.tracked_window_commits(), 0);
}

#[test]
fn tracking_limit_refuses_new_commits_without_forgetting_existing_ones() {
    let mut h = Harness::new();
    h.open_document("canvas", 1);
    for ticket in 1..=4096 {
        h.app.remember_window_commit(format!("window-{ticket}"), ticket);
    }
    assert!(h.app.call("changes.apply", json!({ "request_id": "overflow" }), PendingAction::Apply).is_none());
    assert!(h.sent().is_empty());
    assert_eq!(h.app.tracked_window_commits(), 4096);
    h.app.ai_prompt = "editing".into();
    let mut value = event(1, 1, "a2");
    value["events"][0]["detail"]["request_id"] = json!("window-1");
    h.app.handle_events(value);
    assert!(h.app.agent_follow.notices.is_empty());
}

#[test]
fn explicit_home_cancels_pending_follow_and_events_delivered_with_the_command() {
    let mut h = Harness::new();
    h.open_document("canvas", 1);
    h.app.handle_events(created(1, "a2"));
    let pending = h.sent();
    let run = designllm_contracts::UiRun {
        run_id: new_id(), view_id: new_id(), expected: None, command_id: "nav.home".into(), args: json!({}),
        actor: "mcp:c1:agent".into(), permission: designllm_contracts::UiPermission::Read,
        allowed_intents: Default::default(), status: designllm_contracts::UiRunStatus::Queued,
        timeout_ms: 10_000, result: None, error: None,
    };
    let mut batch = created(2, "a3");
    batch["last_seq"] = json!(2);
    batch["ui_runs"] = json!([run]);
    h.app.handle_events(batch);
    h.answer(&pending, "workspace.list", Ok(json!({ "workspace_id": "w1", "artifacts": [
        summary("a1", "canvas", 1), summary("a2", "canvas", 0), summary("a3", "canvas", 0)] })));
    assert_eq!(h.app.screen, Screen::Home);
    assert!(h.app.agent_follow.notices.is_empty());
    h.sent();
    h.app.handle_events(created(3, "a3"));
    let next = h.sent();
    h.answer(&next, "workspace.list", Ok(json!({ "workspace_id": "w1", "artifacts": [
        summary("a1", "canvas", 1), summary("a2", "canvas", 0), summary("a3", "canvas", 0)] })));
    assert_eq!(h.app.active.as_ref().unwrap().id, "a3");
}

#[test]
fn home_button_discards_a_follow_whose_listing_arrives_late() {
    let mut h = Harness::new();
    h.open_document("canvas", 1);
    h.app.handle_events(created(1, "a2"));
    let pending = h.sent();
    h.app.invoke_ui("nav.home", json!({}));
    h.answer(&pending, "workspace.list", Ok(listing()));
    assert_eq!(h.app.screen, Screen::Home);
    assert!(h.app.agent_follow.notices.is_empty());
}

#[test]
fn agent_revision_waits_for_an_in_flight_user_commit() {
    let mut h = Harness::new();
    h.open_document("canvas", 1);
    let (operations, intents) = edit();
    h.app.apply_operations(operations, intents);
    let user_edit = h.sent();
    h.app.handle_events(event(1, 3, "a1"));
    assert_eq!(h.app.active.as_ref().unwrap().revision, 1);
    h.answer(&user_edit, "changes.apply", Ok(json!({ "revision": 2 })));
    h.app.resume_deferred_agent_revision();
    assert_eq!(h.app.active.as_ref().unwrap().revision, 3);
    let requests = h.sent();
    assert!(requests.iter().any(|(_, method, params)| method == "render.get" && params["expected_revision"] == 3));
}

#[test]
fn following_a_new_document_discovers_its_running_job() {
    let mut h = Harness::new();
    h.open_document("canvas", 1);
    h.app.handle_events(created(1, "a2"));
    let requests = h.sent();
    let mut new_document = summary("a2", "canvas", 0);
    new_document["active_jobs"] = json!([{ "job_id": "job-1", "runtime": "mock", "state": "running" }]);
    h.answer(&requests, "workspace.list", Ok(json!({ "workspace_id": "w1", "artifacts": [
        summary("a1", "canvas", 1), new_document] })));
    let requests = h.sent();
    let mut running = job("running");
    running["artifact_id"] = json!("a2");
    h.answer(&requests, "job.get", Ok(running));
    assert_eq!(h.app.active.as_ref().unwrap().id, "a2");
    assert_eq!(h.app.job.as_ref().unwrap().id, "job-1");
}

#[test]
fn a_follow_listing_missing_the_open_document_keeps_its_unsaved_input() {
    let mut h = Harness::new();
    h.open_document("canvas", 1);
    h.app.text_target = Some(TextTarget::Node { node_id: "n".into(), original: "old".into(), editable: true });
    h.app.text_draft = "unsaved".into();
    h.app.handle_events(created(1, "a2"));
    let requests = h.sent();
    h.answer(&requests, "workspace.list", Ok(json!({ "workspace_id": "w1", "artifacts": [summary("a2", "canvas", 0)] })));
    assert_eq!(h.app.active.as_ref().unwrap().id, "a1");
    assert_eq!(h.app.text_draft, "unsaved");
    assert_eq!(h.app.agent_follow.notices.front().map(String::as_str), Some("a2"));
    assert!(!h.app.artifacts.iter().any(|artifact| artifact.id == "a1"));
}

#[test]
fn a_listing_missing_an_idle_open_document_removes_it_from_the_active_view() {
    let mut h = Harness::new();
    h.open_document("canvas", 1);
    h.app.call("workspace.list", json!({}), PendingAction::WorkspaceList);
    let requests = h.sent();
    h.answer(&requests, "workspace.list", Ok(json!({ "workspace_id": "w1", "artifacts": [summary("a2", "canvas", 0)] })));
    assert!(h.app.active.is_none());
    assert!(h.app.artifacts.iter().all(|artifact| artifact.id != "a1"));
}

#[test]
fn context_arriving_before_the_event_does_not_hide_an_undrawn_revision() {
    let mut h = Harness::new();
    let (_, view) = h.open_document("canvas", 1);
    h.app.handle_payload(&h.ctx, 0, PendingAction::Context, RpcPayload::Json(context("a1", &view, 2)));
    assert_eq!(h.app.active.as_ref().unwrap().revision, 2);
    h.app.handle_events(event(1, 2, "a1"));
    let requests = h.sent();
    assert_eq!(methods(&requests), reload_methods());
    assert_eq!(requests[3].2["expected_revision"], 2);
}

#[test]
fn draft_started_while_waiting_for_the_list_prevents_following() {
    let mut h = Harness::new();
    h.open_document("canvas", 1);
    h.app.handle_events(created(1, "a2"));
    let requests = h.sent();
    h.app.ai_prompt = "작성 중인 요청".into();
    h.answer(&requests, "workspace.list", Ok(listing()));
    assert_eq!(h.app.active.as_ref().unwrap().id, "a1");
    assert_eq!(h.app.ai_prompt, "작성 중인 요청");
    assert_eq!(h.app.agent_follow.notices.front().map(String::as_str), Some("a2"));
}

#[test]
fn manual_navigation_while_waiting_for_the_list_is_not_reversed() {
    let mut h = Harness::new();
    h.open_document("canvas", 1);
    h.app.handle_events(created(1, "a2"));
    let requests = h.sent();
    h.app.artifacts.push(serde_json::from_value(summary("a3", "canvas", 0)).unwrap());
    h.app.invoke_ui("nav.open_project", json!({ "artifact_id": "a3" }));
    h.answer(&requests, "workspace.list", Ok(listing()));
    assert_eq!(h.app.active.as_ref().unwrap().id, "a3");
}

#[test]
fn canceled_external_job_is_not_revived_by_an_older_reply() {
    let mut h = Harness::new();
    h.open_document("canvas", 1);
    h.app.handle_events(job_event(1, "running"));
    let running = h.sent();
    h.app.handle_events(job_event(2, "canceled"));
    let canceled = h.sent();
    h.answer(&canceled, "job.get", Ok(job("canceled")));
    h.answer(&running, "job.get", Ok(job("running")));
    assert_eq!(h.app.job.as_ref().unwrap().status, JobStatus::Canceled);
}

#[test]
fn repeated_commits_do_not_postpone_opening_the_same_document() {
    let mut h = Harness::new();
    h.open_document("canvas", 1);
    h.app.handle_events(created(1, "a2"));
    let listing_request = h.sent();
    h.app.handle_events(event(2, 1, "a2"));
    assert_eq!(methods(&h.sent()), ["workspace.list"]);
    h.answer(&listing_request, "workspace.list", Ok(listing()));
    assert_eq!(h.app.active.as_ref().unwrap().id, "a2");
}

fn web_artifact(revision: u64) -> Value {
    let mut artifact = Artifact::new(ArtifactKind::WebCode, "web", "engine",
        ArtifactBody::WebCode(designllm_contracts::WebDocument::default()));
    artifact.id = "a1".into();
    artifact.revision = revision;
    serde_json::to_value(artifact).unwrap()
}

#[test]
fn web_inspect_before_event_retries_a_stale_pending_context() {
    let mut h = Harness::new();
    let (opening, view) = h.open_document("web_code", 1);
    h.app.fetch_context();
    let old_context = h.sent();
    h.answer(&opening, "artifact.inspect", Ok(web_artifact(2)));
    h.sent();
    h.app.handle_events(event(1, 2, "a1"));
    h.sent();
    h.answer(&old_context, "context.read", Ok(context("a1", &view, 1)));
    let retry = h.sent();
    assert!(retry.iter().any(|(_, method, _)| method == "context.read"));
    h.answer(&retry, "context.read", Ok(context("a1", &view, 2)));
    assert_eq!(h.app.context.as_ref().unwrap().revision, 2);
    assert_eq!(h.app.artifact.as_ref().unwrap().revision, 2);
}

#[test]
fn new_context_and_render_request_do_not_mask_an_old_inspect() {
    let mut h = Harness::new();
    let (opening, view) = h.open_document("canvas", 1);
    let mut artifact = Artifact::new(ArtifactKind::Canvas, "canvas", "engine",
        ArtifactBody::Canvas(designllm_contracts::CanvasDocument::default()));
    artifact.id = "a1".into();
    artifact.revision = 1;
    h.answer(&opening, "artifact.inspect", Ok(serde_json::to_value(artifact).unwrap()));
    h.sent();
    h.app.handle_payload(&h.ctx, 0, PendingAction::Context, RpcPayload::Json(context("a1", &view, 2)));
    h.app.fetch_render(0);
    h.sent();
    h.app.handle_events(event(1, 2, "a1"));
    let requests = h.sent();
    assert!(requests.iter().any(|(_, method, _)| method == "artifact.inspect"));
}

#[test]
fn new_render_before_event_retries_a_stale_pending_inspect() {
    let mut h = Harness::new();
    let (opening, view) = h.open_document("canvas", 1);
    h.app.handle_payload(&h.ctx, 0, PendingAction::Context, RpcPayload::Json(context("a1", &view, 2)));
    h.app.fetch_render(0);
    let render = h.sent();
    h.app.handle_events(event(1, 2, "a1"));
    h.sent();
    h.answer(&render, "render.get", Ok(snapshot("a1", 2)));
    let mut old = Artifact::new(ArtifactKind::Canvas, "old", "engine",
        ArtifactBody::Canvas(designllm_contracts::CanvasDocument::default()));
    old.id = "a1".into();
    old.revision = 1;
    h.answer(&opening, "artifact.inspect", Ok(serde_json::to_value(old).unwrap()));
    let retry = h.sent();
    assert!(retry.iter().any(|(_, method, _)| method == "artifact.inspect"));
}

#[test]
fn separate_documents_keep_ordered_notices_through_out_of_order_listings() {
    let mut h = Harness::new();
    h.open_document("canvas", 1);
    h.app.text_target = Some(TextTarget::Node { node_id: "n".into(), original: "old".into(), editable: true });
    h.app.text_draft = "unsaved".into();
    h.app.handle_events(created(1, "a2"));
    let first = h.sent();
    h.app.handle_events(created(2, "a3"));
    let second = h.sent();
    assert_eq!(h.app.agent_follow.notices.iter().map(String::as_str).collect::<Vec<_>>(), ["a2", "a3"]);
    let listing = json!({ "workspace_id": "w1", "artifacts": [summary("a1", "canvas", 1),
        summary("a2", "canvas", 0), summary("a3", "canvas", 0)] });
    h.answer(&second, "workspace.list", Ok(listing.clone()));
    h.answer(&first, "workspace.list", Ok(listing.clone()));
    let retry = h.sent();
    h.answer(&retry, "workspace.list", Ok(listing));
    assert_eq!(h.app.agent_follow.notices.iter().map(String::as_str).collect::<Vec<_>>(), ["a2", "a3"]);
    h.app.text_draft = "old".into();
    h.app.open_agent_notice();
    assert_eq!(h.app.active.as_ref().unwrap().id, "a2");
    assert_eq!(h.app.agent_follow.notices.front().map(String::as_str), Some("a3"));
    h.app.open_agent_notice();
    assert_eq!(h.app.active.as_ref().unwrap().id, "a3");
}

#[test]
fn repeated_commits_of_one_document_share_one_notice() {
    let mut h = Harness::new();
    h.open_document("canvas", 1);
    h.app.text_target = Some(TextTarget::Node { node_id: "n".into(), original: "old".into(), editable: true });
    h.app.text_draft = "unsaved".into();
    h.app.handle_events(created(1, "a2"));
    let first = h.sent();
    h.app.handle_events(event(2, 1, "a2"));
    assert_eq!(methods(&h.sent()), ["workspace.list"]);
    assert_eq!(h.app.agent_follow.notices.iter().map(String::as_str).collect::<Vec<_>>(), ["a2"]);
    h.answer(&first, "workspace.list", Ok(listing()));
    assert_eq!(h.app.agent_follow.notices.iter().map(String::as_str).collect::<Vec<_>>(), ["a2"]);
}

#[test]
fn only_host_fixed_agent_origins_can_follow() {
    for actor in ["cli", "cli:claude", "desktop", "desktop:inline:123", "claude", "mcp:", "job:"] {
        let mut h = Harness::new();
        h.open_document("canvas", 1);
        let mut value = created(1, "a2");
        value["events"][0]["detail"]["actor"] = json!(actor);
        h.app.handle_events(value);
        let requests = h.sent();
        h.answer(&requests, "workspace.list", Ok(listing()));
        assert_eq!(h.app.active.as_ref().unwrap().id, "a1", "{actor}");
    }
    for actor in ["mcp:c1", "mcp:c1:desktop", "job:j1:claude"] {
        let mut h = Harness::new();
        h.open_document("canvas", 1);
        let mut value = created(1, "a2");
        value["events"][0]["detail"]["actor"] = json!(actor);
        h.app.handle_events(value);
        let requests = h.sent();
        h.answer(&requests, "workspace.list", Ok(listing()));
        assert_eq!(h.app.active.as_ref().unwrap().id, "a2", "{actor}");
    }
}

#[test]
fn job_id_does_not_override_an_untrusted_actor() {
    let mut h = Harness::new();
    h.open_document("canvas", 1);
    let mut value = created(1, "a2");
    value["events"][0]["detail"]["actor"] = json!("cli:user");
    value["events"][0]["detail"]["job_id"] = json!("job-1");
    h.app.handle_events(value);
    let requests = h.sent();
    h.answer(&requests, "workspace.list", Ok(listing()));
    assert_eq!(h.app.active.as_ref().unwrap().id, "a1");
}

#[test]
fn open_document_activity_survives_another_documents_commit() {
    let mut h = Harness::new();
    h.open_document("canvas", 1);
    h.app.handle_events(event(1, 2, "a1"));
    h.sent();
    h.app.text_draft = "editing".into();
    h.app.text_target = Some(TextTarget::Node { node_id: "n".into(), original: "old".into(), editable: true });
    h.app.handle_events(event(2, 1, "a2"));
    assert!(h.app.agent_status(Instant::now()).unwrap().contains("리비전 2"));
    assert_eq!(h.app.agent_follow.notices.front().map(String::as_str), Some("a2"));
}

#[test]
fn pending_write_in_another_document_does_not_block_the_open_document_refresh() {
    let mut h = Harness::new();
    h.open_document("canvas", 1);
    let (operations, intents) = edit();
    h.app.apply_operations(operations, intents);
    h.sent();
    h.app.artifacts.push(serde_json::from_value(summary("a2", "canvas", 0)).unwrap());
    h.app.invoke_ui("nav.open_project", json!({ "artifact_id": "a2" }));
    h.sent();
    h.app.handle_events(event(1, 2, "a2"));
    let requests = h.sent();
    assert!(requests.iter().any(|(_, method, params)| method == "render.get" && params["expected_revision"] == 2));
}

#[test]
fn ai_panel_shows_all_running_jobs_of_only_the_open_document() {
    let mut h = Harness::new();
    h.open_document("canvas", 1);
    h.app.handle_events(job_event(1, "running"));
    let first = h.sent();
    let mut second_event = job_event(2, "running");
    second_event["events"][0]["detail"]["job_id"] = json!("job-2");
    h.app.handle_events(second_event);
    let second = h.sent();
    let mut first_job = job("running");
    first_job["client_name"] = json!("worker one");
    h.answer(&first, "job.get", Ok(first_job));
    let mut second_job = job("running");
    second_job["id"] = json!("job-2");
    second_job["client_name"] = json!("worker two");
    h.answer(&second, "job.get", Ok(second_job));
    let mut other_job = job("running");
    other_job["id"] = json!("job-3");
    other_job["artifact_id"] = json!("other");
    other_job["client_name"] = json!("outside worker");
    h.app.record_started_job(serde_json::from_value(other_job).unwrap());
    let ctx = h.ctx.clone();
    crate::theme::install(&ctx);
    let output = ctx.run(screen(1.0, 900.0, 900.0), |ctx| {
        egui::CentralPanel::default().show(ctx, |ui| {
            let palette = crate::theme::palette(ctx);
            h.app.draw_ai_tab(ui, &palette);
        });
    });
    let text = drawn_text(&output).join(" ");
    assert!(text.contains("worker one"), "{text}");
    assert!(text.contains("worker two"), "{text}");
    assert!(!text.contains("outside worker"), "{text}");
}

#[test]
fn out_of_order_status_replies_are_tracked_per_job() {
    let mut h = Harness::new();
    h.open_document("canvas", 1);
    h.app.handle_events(job_event(1, "running"));
    let first = h.sent();
    let mut second_event = job_event(2, "running");
    second_event["events"][0]["detail"]["job_id"] = json!("job-2");
    h.app.handle_events(second_event);
    let second = h.sent();
    h.app.handle_events(job_event(3, "canceled"));
    let canceled = h.sent();
    let mut second_job = job("running");
    second_job["id"] = json!("job-2");
    h.answer(&second, "job.get", Ok(second_job));
    h.answer(&canceled, "job.get", Ok(job("canceled")));
    h.answer(&first, "job.get", Ok(job("running")));
    assert_eq!(h.app.job.as_ref().unwrap().id, "job-2");
    assert!(h.app.job.iter().chain(&h.app.parked_jobs).all(|job| job.id != "job-1" || job.status == JobStatus::Canceled));
}

#[test]
fn icon_picker_blocks_follow_and_preserves_the_insertion_position() {
    use super::toolbox::{AddAt, PendingIcon, PENDING_ICON};
    let mut h = Harness::new();
    h.open_document("canvas", 1);
    h.app.exit_ctx = Some(h.ctx.clone());
    let at = AddAt { page_index: 2, center: (123, 456) };
    h.ctx.data_mut(|data| data.insert_temp(egui::Id::new(PENDING_ICON), PendingIcon {
        at, artifact_id: "a1".into(), anchor: egui::Pos2::ZERO, frame: 0, drawn: 0,
    }));
    h.app.handle_events(created(1, "a2"));
    let requests = h.sent();
    h.answer(&requests, "workspace.list", Ok(listing()));
    assert_eq!(h.app.active.as_ref().unwrap().id, "a1");
    assert_eq!(h.ctx.data(|data| data.get_temp::<PendingIcon>(egui::Id::new(PENDING_ICON))).unwrap().at, at);
    assert_eq!(h.app.agent_follow.notices.front().map(String::as_str), Some("a2"));
}

#[test]
fn property_focus_defers_refresh_until_focus_leaves() {
    use designllm_contracts::{CanvasDocument, Node, NodeKind, Page, Rect as ModelRect, Size, Style};
    let mut h = Harness::new();
    h.open_document("canvas", 1);
    h.app.exit_ctx = Some(h.ctx.clone());
    h.app.selected_node = Some("node".into());
    h.app.selected_nodes = vec!["node".into()];
    h.app.context.as_mut().unwrap().selection.node_ids = vec!["node".into()];
    h.app.style_fill = "#111111".into();
    let node = Node {
        id: "node".into(), kind: NodeKind::Rectangle,
        bounds: ModelRect { x: 0, y: 0, width: 100, height: 100 }, text: String::new(),
        style: Style { fill: "#AABBCC".into(), ..Default::default() }, asset: None,
        children: vec![], layout: None, interactions: vec![], variants: Default::default(), ext: Default::default(),
    };
    let mut artifact = Artifact::new(ArtifactKind::Canvas, "canvas", "engine", ArtifactBody::Canvas(CanvasDocument {
        pages: vec![Page { nodes: vec![node], ..Page::new("page", Size::a4()) }], ..Default::default()
    }));
    artifact.id = "a1".into();
    artifact.revision = 1;
    let field = egui::Id::new("property-field");
    let mut text = String::new();
    let _ = h.ctx.run(screen(1.0, 500.0, 500.0), |ctx| {
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.add(egui::TextEdit::singleline(&mut text).id(field)).request_focus();
        });
    });
    assert!(h.ctx.wants_keyboard_input());
    h.app.handle_payload(&h.ctx, 0, PendingAction::Inspect, RpcPayload::Json(serde_json::to_value(artifact).unwrap()));
    assert_eq!(h.app.style_fill, "#111111");
    h.ctx.memory_mut(|memory| memory.surrender_focus(field));
    h.app.resume_deferred_agent_revision();
    assert_eq!(h.app.style_fill, "#AABBCC");
}

#[test]
fn deferred_refresh_does_not_restore_a_selection_the_user_changed() {
    let mut h = Harness::new();
    let (_, view) = h.open_document("canvas", 1);
    h.app.text_editor_focused = true;
    let mut snapshot = context("a1", &view, 1);
    snapshot["selection"]["node_ids"] = json!(["old"]);
    h.app.handle_payload(&h.ctx, 0, PendingAction::Context, RpcPayload::Json(snapshot));
    h.app.selected_node = Some("new".into());
    h.app.selected_nodes = vec!["new".into()];
    h.app.selection_generation += 1;
    let mut artifact = Artifact::new(ArtifactKind::Canvas, "canvas", "engine",
        ArtifactBody::Canvas(designllm_contracts::CanvasDocument::default()));
    artifact.id = "a1".into();
    artifact.revision = 1;
    h.app.handle_payload(&h.ctx, 0, PendingAction::Inspect, RpcPayload::Json(serde_json::to_value(artifact).unwrap()));
    h.app.text_editor_focused = false;
    h.app.resume_deferred_agent_revision();
    assert_eq!(h.app.selected_node.as_deref(), Some("new"));
}

#[test]
fn context_skipped_during_selection_write_does_not_replace_the_new_selection() {
    let mut h = Harness::new();
    let (_, view) = h.open_document("canvas", 1);
    h.app.selected_node = Some("new".into());
    h.app.selected_nodes = vec!["new".into()];
    h.app.selection_generation += 1;
    let ticket = h.app.call("selection.set", json!({ "artifact_id": "a1" }), PendingAction::Selection).unwrap();
    let mut snapshot = context("a1", &view, 1);
    snapshot["selection"]["node_ids"] = json!(["old"]);
    h.app.handle_payload(&h.ctx, 0, PendingAction::Context, RpcPayload::Json(snapshot));
    h.app.pending.remove(&ticket);
    h.app.resume_deferred_agent_revision();
    assert_eq!(h.app.selected_node.as_deref(), Some("new"));
}

#[test]
fn first_poll_baselines_the_host_head_without_replaying_old_events() {
    let mut host = Host::start();
    host.call("artifact.create", json!({ "request_id": new_id(), "actor": "old", "kind": "canvas",
        "title": "old", "size": null, "template": null })).unwrap();
    let mut h = Harness::new();
    h.sent();
    h.app.last_event_poll = Instant::now() - Duration::from_secs(3);
    h.app.poll_events();
    let requests = h.sent();
    let poll = requests.iter().find(|request| request.1 == "events.poll").unwrap();
    let batch = host.call("events.poll", poll.2.clone()).unwrap();
    let head = batch["last_seq"].as_u64().unwrap();
    assert!(head > 0 && batch["resync_required"] == true);
    h.answer(&requests, "events.poll", Ok(batch));
    assert_eq!(h.app.last_event_seq, head);
    assert!(h.app.agent_follow.notices.is_empty());
    assert_eq!(h.app.screen, Screen::Home);
    h.app.handle_events(created(head + 1, "new"));
    let requests = h.sent();
    h.answer(&requests, "workspace.list", Ok(json!({ "workspace_id": "w1", "artifacts": [summary("new", "canvas", 0)] })));
    assert_eq!(h.app.active.as_ref().unwrap().id, "new");
}

#[test]
fn first_poll_with_an_unchanged_workspace_does_not_request_document_reads() {
    let mut h = Harness::new();
    h.open_document("canvas", 1);
    h.app.last_event_poll = Instant::now() - Duration::from_secs(3);
    h.app.poll_events();
    let requests = h.sent();
    h.answer(&requests, "events.poll", Ok(json!({ "events": [], "last_seq": 42,
        "resync_required": true, "artifacts": [summary("a1", "canvas", 1)] })));
    assert_eq!(h.app.last_event_seq, 42);
    assert!(h.sent().is_empty());
}

#[test]
fn first_poll_updates_a_changed_work_list_without_rereading_the_open_document() {
    let mut h = Harness::new();
    h.open_document("canvas", 1);
    h.app.last_event_poll = Instant::now() - Duration::from_secs(3);
    h.app.poll_events();
    let requests = h.sent();
    h.answer(&requests, "events.poll", Ok(json!({ "events": [], "last_seq": 42,
        "resync_required": true, "artifacts": [summary("a1", "canvas", 1), summary("a2", "canvas", 0)] })));
    assert_eq!(methods(&h.sent()), ["workspace.list"]);
    assert!(h.app.agent_follow.notices.is_empty());
}

#[test]
fn first_poll_refreshes_an_open_document_changed_since_it_was_read() {
    let mut h = Harness::new();
    h.open_document("canvas", 1);
    h.app.last_event_poll = Instant::now() - Duration::from_secs(3);
    h.app.poll_events();
    let requests = h.sent();
    h.answer(&requests, "events.poll", Ok(json!({ "events": [], "last_seq": 42,
        "resync_required": true, "artifacts": [summary("a1", "canvas", 2)] })));
    assert_eq!(h.app.active.as_ref().unwrap().revision, 2);
    assert_eq!(methods(&h.sent()), reload_methods());
}

#[test]
fn later_resync_still_refreshes_even_when_the_listing_is_unchanged() {
    let mut h = Harness::new();
    h.open_document("canvas", 1);
    h.app.last_event_poll = Instant::now() - Duration::from_secs(3);
    h.app.poll_events();
    let requests = h.sent();
    h.answer(&requests, "events.poll", Ok(json!({ "events": [], "last_seq": 42,
        "resync_required": true, "artifacts": [summary("a1", "canvas", 1)] })));
    h.sent();
    h.app.handle_events(json!({ "events": [], "last_seq": 60,
        "resync_required": true, "artifacts": [summary("a1", "canvas", 1)] }));
    assert_eq!(h.app.last_event_seq, 60);
    assert_eq!(methods(&h.sent()), reload_methods());
}

#[test]
fn first_poll_defers_a_changed_document_while_a_user_commit_is_pending() {
    let mut h = Harness::new();
    h.open_document("canvas", 1);
    let (operations, intents) = edit();
    h.app.apply_operations(operations, intents);
    let edit = h.sent();
    h.app.last_event_poll = Instant::now() - Duration::from_secs(3);
    h.app.poll_events();
    let requests = h.sent();
    h.answer(&requests, "events.poll", Ok(json!({ "events": [], "last_seq": 42,
        "resync_required": true, "artifacts": [summary("a1", "canvas", 3)] })));
    assert_eq!(h.app.active.as_ref().unwrap().revision, 1);
    assert!(!h.sent().iter().any(|(_, method, _)| method == "artifact.inspect" || method == "context.read"));
    h.answer(&edit, "changes.apply", Ok(json!({ "revision": 2 })));
    h.app.resume_deferred_agent_revision();
    assert_eq!(h.app.active.as_ref().unwrap().revision, 3);
}

#[test]
fn resync_advances_cursor_and_later_events_are_seen() {
    let mut h = Harness::new();
    h.open_document("canvas", 1);
    h.app.handle_events(json!({ "events": [], "last_seq": 500, "resync_required": true }));
    assert_eq!(h.app.last_event_seq, 500);
    assert_eq!(h.app.agent_poll_cursor(), 500);
    h.sent();
    h.app.handle_events(created(501, "a2"));
    let requests = h.sent();
    h.answer(&requests, "workspace.list", Ok(listing()));
    assert_eq!(h.app.active.as_ref().unwrap().id, "a2");
}

#[test]
fn host_head_advances_without_skipping_paginated_events() {
    let mut h = Harness::new();
    h.open_document("canvas", 1);
    let mut first = event(1, 2, "a1");
    first["last_seq"] = json!(3);
    h.app.handle_events(first);
    assert_eq!(h.app.last_event_seq, 3);
    assert_eq!(h.app.agent_poll_cursor(), 1);
    let mut second = created(2, "a2");
    second["last_seq"] = json!(3);
    h.app.handle_events(second);
    assert_eq!(h.app.agent_poll_cursor(), 2);
    let mut third = created(3, "a3");
    third["last_seq"] = json!(3);
    h.app.handle_events(third);
    assert_eq!(h.app.agent_poll_cursor(), 3);
    assert!(h.app.agent_follow.notices.is_empty());
}

#[test]
fn one_listing_handles_two_hundred_documents_and_the_active_refresh() {
    let mut h = Harness::new();
    h.open_document("canvas", 1);
    h.app.text_target = Some(TextTarget::Node { node_id: "n".into(), original: "old".into(), editable: true });
    h.app.text_draft = "unfinished".into();
    let mut events = vec![event(1, 2, "a1")["events"][0].clone()];
    events.extend((2..202).map(|seq| created(seq, &format!("doc-{seq}"))["events"][0].clone()));
    h.app.handle_events(json!({ "events": events, "last_seq": 201 }));
    let requests = h.sent();
    assert_eq!(requests.iter().filter(|request| request.1 == "workspace.list").count(), 1);
    let mut artifacts = vec![summary("a1", "canvas", 2)];
    artifacts.extend((2..202).map(|seq| summary(&format!("doc-{seq}"), "canvas", 0)));
    h.answer(&requests, "workspace.list", Ok(json!({ "workspace_id": "w1", "artifacts": artifacts })));
    assert_eq!(h.app.active.as_ref().unwrap().id, "a1");
    assert_eq!(h.app.agent_follow.notices.len(), 200);
    assert_eq!(h.app.text_draft, "unfinished");
}

#[test]
fn unrelated_drafts_and_requests_do_not_block_current_document_sync() {
    use designllm_contracts::{CanvasDocument, Node, NodeKind, Page, Rect as ModelRect, Size, Style};
    let mut h = Harness::new();
    let (_, view) = h.open_document("canvas", 1);
    h.app.ai_prompt = "unfinished request".into();
    h.app.drafts.keep_recovered(super::drafts::Live {
        artifact_id: Some("a1".into()), revision: Some(1),
        body: super::drafts::Body::Ai { prompt: "recovered".into(), figure_refs: Vec::new() },
    }, Instant::now());
    h.app.text_target = Some(TextTarget::Node { node_id: "node".into(), original: "old".into(), editable: true });
    h.app.text_draft = "unsaved text".into();
    h.app.call("unrelated.request", json!({}), PendingAction::Create);
    let mut snapshot = context("a1", &view, 1);
    snapshot["selection"]["node_ids"] = json!(["node"]);
    h.app.handle_payload(&h.ctx, 0, PendingAction::Context, RpcPayload::Json(snapshot));
    let node = Node {
        id: "node".into(), kind: NodeKind::Text,
        bounds: ModelRect { x: 0, y: 0, width: 100, height: 100 }, text: "agent text".into(),
        style: Style { fill: "#AABBCC".into(), ..Default::default() }, asset: None,
        children: vec![], layout: None, interactions: vec![], variants: Default::default(), ext: Default::default(),
    };
    let mut artifact = Artifact::new(ArtifactKind::Canvas, "canvas", "engine", ArtifactBody::Canvas(CanvasDocument {
        pages: vec![Page { nodes: vec![node], ..Page::new("page", Size::a4()) }], ..Default::default()
    }));
    artifact.id = "a1".into();
    artifact.revision = 1;
    h.app.reselect = Some(Selection { node_ids: vec!["node".into()], text_range: None });
    h.app.handle_payload(&h.ctx, 0, PendingAction::Inspect, RpcPayload::Json(serde_json::to_value(artifact).unwrap()));
    assert_eq!(h.app.selected_node.as_deref(), Some("node"));
    assert_eq!(h.app.style_fill, "#AABBCC");
    assert_eq!(h.app.text_draft, "unsaved text");
    assert_eq!(h.app.drafts.recovered().len(), 1);
    assert!(!h.app.can_follow_agent());
    h.app.text_draft = "old".into();
    h.app.resume_deferred_agent_revision();
    assert_eq!(h.app.text_draft, "agent text");
}

#[test]
fn newest_deferred_context_tracks_the_new_selection_generation() {
    let mut h = Harness::new();
    let (_, view) = h.open_document("canvas", 1);
    h.app.text_editor_focused = true;
    let mut old = context("a1", &view, 1);
    old["selection"]["node_ids"] = json!(["old"]);
    h.app.handle_payload(&h.ctx, 0, PendingAction::Context, RpcPayload::Json(old));
    h.app.set_selection(Selection { node_ids: vec!["new".into()], text_range: None });
    let requests = h.sent();
    let mut new = context("a1", &view, 1);
    new["selection"]["node_ids"] = json!(["new"]);
    h.answer(&requests, "selection.set", Ok(new));
    h.app.text_editor_focused = false;
    h.app.resume_deferred_agent_revision();
    assert_eq!(h.app.selected_node.as_deref(), Some("new"));
}

#[test]
fn pending_follow_listing_does_not_hide_a_new_workspace_update() {
    let mut h = Harness::new();
    h.open_document("canvas", 1);
    h.app.handle_events(created(1, "a2"));
    let first = h.sent();
    h.app.handle_events(json!({ "events": [{ "event_seq": 2, "event": "job.status", "artifact_id": "other", "detail": {} }] }));
    let update = h.sent();
    assert_eq!(methods(&update), ["workspace.list"]);
    let mut active = summary("a1", "canvas", 1);
    active["active_jobs"] = json!([{ "job_id": "job-new", "runtime": "mock", "state": "running" }]);
    h.answer(&update, "workspace.list", Ok(json!({ "workspace_id": "w1", "artifacts": [active, summary("a2", "canvas", 0)] })));
    assert!(h.sent().iter().any(|(_, method, params)| method == "job.get" && params["job_id"] == "job-new"));
    h.answer(&first, "workspace.list", Ok(listing()));
    assert_eq!(methods(&h.sent()), ["workspace.list"]);
}

#[test]
fn parked_running_jobs_drive_polling_and_only_their_document_status() {
    let mut h = Harness::new();
    h.open_document("canvas", 1);
    h.app.job = Some(serde_json::from_value(job("completed")).unwrap());
    let mut active_job = job("running");
    active_job["id"] = json!("parked-active");
    h.app.parked_jobs.push(serde_json::from_value(active_job).unwrap());
    let mut other_job = job("running");
    other_job["id"] = json!("parked-other");
    other_job["artifact_id"] = json!("other");
    h.app.parked_jobs.push(serde_json::from_value(other_job).unwrap());
    assert_eq!(h.app.agent_poll_interval(), Duration::from_millis(500));
    assert!(h.app.agent_status(Instant::now()).is_some());
    h.app.parked_jobs[0].status = JobStatus::Completed;
    assert!(h.app.agent_status(Instant::now()).is_none());
    assert_eq!(h.app.agent_poll_interval(), Duration::from_millis(500));
    h.app.parked_jobs[1].status = JobStatus::Completed;
    assert_eq!(h.app.agent_poll_interval(), Duration::from_secs(2));
}

#[test]
fn deleted_notice_is_removed_and_the_next_document_opens() {
    let mut h = Harness::new();
    h.open_document("canvas", 1);
    h.app.ai_prompt = "draft".into();
    let mut batch = created(1, "a2");
    batch["events"].as_array_mut().unwrap().push(created(2, "a3")["events"][0].clone());
    h.app.handle_events(batch);
    let requests = h.sent();
    h.answer(&requests, "workspace.list", Ok(json!({ "workspace_id": "w1", "artifacts": [
        summary("a1", "canvas", 1), summary("a2", "canvas", 0), summary("a3", "canvas", 0)] })));
    h.app.call("workspace.list", json!({}), PendingAction::WorkspaceList);
    let requests = h.sent();
    h.answer(&requests, "workspace.list", Ok(json!({ "workspace_id": "w1", "artifacts": [
        summary("a1", "canvas", 1), summary("a3", "canvas", 0)] })));
    assert_eq!(h.app.agent_follow.notices.iter().map(String::as_str).collect::<Vec<_>>(), ["a3"]);
    h.app.ai_prompt.clear();
    h.app.open_agent_notice();
    assert_eq!(h.app.active.as_ref().unwrap().id, "a3");
}

#[test]
fn missing_notice_at_open_is_skipped_without_losing_the_next() {
    let mut h = Harness::new();
    h.open_document("canvas", 1);
    h.app.ai_prompt = "draft".into();
    let mut batch = created(1, "a2");
    batch["events"].as_array_mut().unwrap().push(created(2, "a3")["events"][0].clone());
    h.app.handle_events(batch);
    let requests = h.sent();
    h.answer(&requests, "workspace.list", Ok(json!({ "workspace_id": "w1", "artifacts": [
        summary("a1", "canvas", 1), summary("a2", "canvas", 0), summary("a3", "canvas", 0)] })));
    h.app.artifacts.retain(|artifact| artifact.id != "a2");
    h.app.ai_prompt.clear();
    h.app.open_agent_notice();
    assert_eq!(h.app.active.as_ref().unwrap().id, "a3");
    assert!(h.app.agent_follow.notices.is_empty());
}

#[test]
fn listing_while_a_candidate_is_pending_does_not_discard_its_notice() {
    let mut h = Harness::new();
    h.open_document("canvas", 1);
    h.app.ai_prompt = "draft".into();
    h.app.handle_events(created(1, "a2"));
    let pending = h.sent();
    h.app.call("workspace.list", json!({}), PendingAction::WorkspaceList);
    let update = h.sent();
    h.answer(&update, "workspace.list", Ok(json!({ "workspace_id": "w1", "artifacts": [summary("a1", "canvas", 1)] })));
    assert_eq!(h.app.agent_follow.notices.front().map(String::as_str), Some("a2"));
    h.answer(&pending, "workspace.list", Ok(listing()));
    let retry = h.sent();
    h.answer(&retry, "workspace.list", Ok(listing()));
    assert_eq!(h.app.agent_follow.notices.front().map(String::as_str), Some("a2"));
}

#[test]
fn hello_starts_the_baseline_poll_immediately_and_only_once() {
    let mut h = Harness::new();
    let startup = h.sent();
    h.answer(&startup, "system.hello", Ok(json!({ "workspace_id": "w1" })));
    let requests = h.sent();
    let poll = requests.iter().find(|(_, method, _)| method == "events.poll").unwrap();
    assert_eq!(poll.2["after_seq"], json!(i64::MAX));
    h.answer(&requests, "events.poll", Ok(json!({ "events": [], "last_seq": 5, "resync_required": true, "artifacts": [] })));
    h.sent();
    h.app.handle_payload(&h.ctx, 0, PendingAction::Hello, RpcPayload::Json(json!({ "workspace_id": "w1" })));
    assert!(!h.sent().iter().any(|(_, method, _)| method == "events.poll"));
    h.app.handle_events(created(6, "a2"));
    let requests = h.sent();
    h.answer(&requests, "workspace.list", Ok(json!({ "workspace_id": "w1", "artifacts": [summary("a2", "canvas", 0)] })));
    assert_eq!(h.app.active.as_ref().unwrap().id, "a2");
}

#[test]
fn proposals_in_a_replayed_batch_are_recorded_once_without_following_old_work() {
    let mut h = Harness::new();
    h.open_document("canvas", 1);
    h.app.handle_events(json!({ "events": [], "last_seq": 5, "resync_required": true }));
    h.sent();
    let batch = json!({ "events": [
        { "event_seq": 1, "artifact_id": "a2", "revision": 0, "event": "artifact.created",
          "detail": { "actor": "mcp:c1:claude" } },
        { "event_seq": 2, "artifact_id": "a1", "revision": 1, "event": "proposal.created",
          "detail": { "proposal_id": "p1", "status": "validated" } }
    ], "last_seq": 5 });
    h.app.handle_events(batch.clone());
    assert_eq!(h.app.listed_proposal_ids(), ["p1"]);
    assert_eq!(h.app.active.as_ref().unwrap().id, "a1");
    assert!(h.app.agent_follow.notices.is_empty());
    h.app.invoke("proposal.discard", json!({ "proposal_id": "p1" })).unwrap();
    h.app.handle_events(batch);
    assert!(h.app.listed_proposal_ids().is_empty());
    assert_eq!(h.app.agent_poll_cursor(), 5);
}

#[test]
fn newest_document_in_one_batch_is_followed_and_older_one_is_notified() {
    let mut h = Harness::new();
    h.open_document("canvas", 1);
    let mut batch = created(1, "a2");
    batch["events"].as_array_mut().unwrap().push(created(2, "a3")["events"][0].clone());
    h.app.handle_events(batch);
    let requests = h.sent();
    h.answer(&requests, "workspace.list", Ok(json!({ "workspace_id": "w1", "artifacts": [
        summary("a1", "canvas", 1), summary("a2", "canvas", 0), summary("a3", "canvas", 0)] })));
    assert_eq!(h.app.active.as_ref().unwrap().id, "a3");
    assert_eq!(h.app.agent_follow.notices.front().map(String::as_str), Some("a2"));
}

#[test]
fn repeated_document_in_one_batch_is_ranked_by_its_latest_commit() {
    let mut h = Harness::new();
    h.open_document("canvas", 1);
    let mut batch = created(1, "a2");
    batch["events"].as_array_mut().unwrap().push(created(2, "a3")["events"][0].clone());
    batch["events"].as_array_mut().unwrap().push(event(3, 1, "a2")["events"][0].clone());
    h.app.handle_events(batch);
    let requests = h.sent();
    h.answer(&requests, "workspace.list", Ok(json!({ "workspace_id": "w1", "artifacts": [
        summary("a1", "canvas", 1), summary("a2", "canvas", 1), summary("a3", "canvas", 0)] })));
    assert_eq!(h.app.active.as_ref().unwrap().id, "a2");
    assert_eq!(h.app.agent_follow.notices.front().map(String::as_str), Some("a3"));
}

#[test]
fn empty_event_page_advances_to_the_host_head() {
    let mut h = Harness::new();
    h.open_document("canvas", 1);
    let mut first = event(1, 2, "a1");
    first["last_seq"] = json!(5);
    h.app.handle_events(first);
    assert_eq!(h.app.agent_poll_cursor(), 1);
    h.app.handle_events(json!({ "events": [], "last_seq": 5, "resync_required": false }));
    assert_eq!(h.app.agent_poll_cursor(), 5);
}
