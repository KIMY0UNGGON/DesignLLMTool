use super::*;
use crate::app::test_support::{summary, Harness, Host, Window};
use crate::app::web_preview::tests::{fake_child, shared_stderr_file};

fn open_preview(h: &mut Harness) {
    h.app.fake_preview_command = Some(fake_child());
    h.app.launch_web_preview(Path::new("unused.html"), None).unwrap();
}

fn materialized(revision: u64) -> Value {
    json!({ "artifact_id": "a1", "revision": revision, "entry_path": "unused.html", "source_hash": "hash" })
}

fn agent_context() -> ui_channel::RunContext {
    ui_channel::RunContext {
        run_id: "agent-run".into(), deadline: Instant::now() + Duration::from_secs(30), command_id: "web.preview".into(),
        actor: "agent".into(), allowed_intents: Default::default(), await_rpc_reply: true, mutation_attempted: false,
        rpc_ticket: None, enqueue_error: None, expected: None, prepared_asset: None,
    }
}

#[test]
fn revision_change_while_waiting_for_agent_ready_keeps_and_refreshes_the_window() {
    let _shared = shared_stderr_file();
    let mut host = Host::start();
    let web = host.create("web_code", "웹");
    let mut window = Window::open(&mut host, &web);
    window.app.web_runtime_unavailable = None;
    window.app.fake_preview_command = Some(fake_child());
    let run = window.agent_run_granted(&mut host, "web.preview", json!({}), "read", &[]);
    let original = window.app.web_preview_child.as_ref().unwrap().id();
    let artifact = host.artifact(&web);
    let ArtifactBody::WebCode(document) = artifact.body else { panic!("web document") };
    let path = document.entry;
    let hash = document.files[&path].sha256.clone();
    let revision = host.apply(&web, vec![Operation::ReplaceWebSource {
        path, expected_hash: hash, content: "<!doctype html><title>updated</title>".into(),
    }], [Intent::Content].into_iter().collect()).unwrap();
    window.app.active.as_mut().unwrap().revision = revision;
    window.app.poll_web_preview_child();
    window.settle(&mut host);
    let outcome = host.call("ui.command.get", json!({ "run_id": run["run_id"] })).unwrap();
    assert_eq!(outcome["error"]["code"], "REVISION_CONFLICT");
    assert_eq!(window.app.web_preview_child.as_ref().unwrap().id(), original);
    window.app.web_preview_state.changed_at = Some(Instant::now() - Duration::from_secs(1));
    window.app.fake_preview_command = Some(fake_child());
    window.app.update_web_preview();
    window.settle(&mut host);
    assert_ne!(window.app.web_preview_child.as_ref().unwrap().id(), original);
    assert!(window.calls.iter().any(|(method, params, _)| method == "web.materialize" && params["expected_revision"] == revision));
}

#[test]
fn automatic_refresh_waits_for_the_in_flight_agent_request() {
    let _shared = shared_stderr_file();
    let mut h = Harness::new();
    h.open_document("web_code", 1);
    open_preview(&mut h);
    h.sent();
    h.app.active.as_mut().unwrap().revision = 2;
    h.app.update_web_preview();
    h.app.run_context = Some(agent_context());
    h.app.start_materialized_web_preview();
    h.app.run_context = None;
    let explicit = h.sent();
    let ticket = explicit.iter().find(|(_, method, _)| method == "web.materialize").unwrap().0;
    h.app.web_preview_state.changed_at = Some(Instant::now() - Duration::from_secs(1));
    h.app.update_web_preview();
    assert!(h.app.pending.get(&ticket).is_some_and(|pending| pending.ui_run.is_some()));
    assert!(!h.sent().iter().any(|(_, method, _)| method == "web.materialize"));
    h.app.fake_preview_command = Some(fake_child());
    h.answer(&explicit, "web.materialize", Ok(materialized(2)));
    assert_eq!(h.app.web_preview_run.as_ref().unwrap().id, "agent-run");
}

#[test]
fn explicit_request_replaces_automatic_refresh_and_ignores_its_late_reply() {
    let _shared = shared_stderr_file();
    let mut h = Harness::new();
    h.open_document("web_code", 1);
    open_preview(&mut h);
    let original = h.app.web_preview_child.as_ref().unwrap().id();
    h.sent();
    h.app.active.as_mut().unwrap().revision = 2;
    h.app.update_web_preview();
    h.app.web_preview_state.changed_at = Some(Instant::now() - Duration::from_secs(1));
    h.app.update_web_preview();
    let automatic = h.sent();
    h.app.run_context = Some(agent_context());
    h.app.start_materialized_web_preview();
    h.app.run_context = None;
    let explicit = h.sent();
    h.answer(&automatic, "web.materialize", Ok(materialized(2)));
    assert_eq!(h.app.web_preview_child.as_ref().unwrap().id(), original);
    h.app.fake_preview_command = Some(fake_child());
    h.answer(&explicit, "web.materialize", Ok(materialized(2)));
    assert_eq!(h.app.web_preview_run.as_ref().unwrap().id, "agent-run");
}

#[test]
fn failed_refresh_releases_the_ticket_and_retries_then_offers_reopen() {
    let _shared = shared_stderr_file();
    let mut h = Harness::new();
    h.open_document("web_code", 1);
    open_preview(&mut h);
    h.sent();
    h.app.active.as_mut().unwrap().revision = 2;
    h.app.update_web_preview();
    h.app.web_preview_state.changed_at = Some(Instant::now() - Duration::from_secs(1));
    h.app.update_web_preview();
    let first = h.sent();
    h.answer(&first, "web.materialize", Err(DltError::new(ErrorCode::Unavailable, "temporary file error")));
    h.app.update_web_preview();
    assert!(h.app.web_preview_state.refresh_ticket.is_none());
    assert!(h.app.web_preview_state.requested_revision.is_none());
    assert!(h.app.web_preview_child.is_some());
    assert!(!h.sent().iter().any(|(_, method, _)| method == "web.materialize"));
    h.app.web_preview_state.changed_at = Some(Instant::now() - Duration::from_secs(2));
    h.app.update_web_preview();
    let second = h.sent();
    h.answer(&second, "web.materialize", Err(DltError::new(ErrorCode::Unavailable, "temporary file error")));
    h.app.update_web_preview();
    assert!(h.app.web_preview_state.refresh_ticket.is_none());
    assert!(h.message().unwrap().1.contains("다시 열어 주세요"));
    h.app.start_materialized_web_preview();
    let manual = h.sent();
    h.app.fake_preview_command = Some(fake_child());
    h.answer(&manual, "web.materialize", Ok(materialized(2)));
    assert_eq!(h.app.web_preview_state.displayed_revision, 2);
}

#[test]
fn transient_refresh_failure_retries_the_same_revision_successfully() {
    let _shared = shared_stderr_file();
    let mut h = Harness::new();
    h.open_document("web_code", 1);
    open_preview(&mut h);
    h.sent();
    h.app.active.as_mut().unwrap().revision = 2;
    h.app.update_web_preview();
    h.app.web_preview_state.changed_at = Some(Instant::now() - Duration::from_secs(1));
    h.app.update_web_preview();
    let first = h.sent();
    h.answer(&first, "web.materialize", Err(DltError::new(ErrorCode::Unavailable, "temporary error")));
    h.app.update_web_preview();
    h.app.web_preview_state.changed_at = Some(Instant::now() - Duration::from_secs(2));
    h.app.update_web_preview();
    let retry = h.sent();
    assert!(retry.iter().any(|(_, method, args)| method == "web.materialize" && args["expected_revision"] == 2));
    h.app.fake_preview_command = Some(fake_child());
    h.answer(&retry, "web.materialize", Ok(materialized(2)));
    assert_eq!(h.app.web_preview_state.displayed_revision, 2);
    assert!(h.app.web_preview_state.refresh_ticket.is_none());
}

#[test]
fn failed_explicit_request_releases_its_ticket_for_automatic_refresh() {
    let _shared = shared_stderr_file();
    let mut h = Harness::new();
    h.open_document("web_code", 1);
    open_preview(&mut h);
    h.sent();
    h.app.active.as_mut().unwrap().revision = 2;
    h.app.update_web_preview();
    h.app.run_context = Some(agent_context());
    h.app.start_materialized_web_preview();
    h.app.run_context = None;
    let explicit = h.sent();
    h.answer(&explicit, "web.materialize", Err(DltError::new(ErrorCode::Unavailable, "temporary error")));
    h.app.update_web_preview();
    assert!(h.app.web_preview_state.explicit_ticket.is_none());
    h.app.web_preview_state.changed_at = Some(Instant::now() - Duration::from_secs(1));
    h.app.update_web_preview();
    assert!(h.sent().iter().any(|(_, method, args)| method == "web.materialize" && args["expected_revision"] == 2));
}

#[test]
fn refresh_launch_failure_releases_state_and_says_how_to_recover() {
    let _shared = shared_stderr_file();
    let mut h = Harness::new();
    h.open_document("web_code", 1);
    open_preview(&mut h);
    h.sent();
    h.app.active.as_mut().unwrap().revision = 2;
    h.app.update_web_preview();
    h.app.web_preview_state.changed_at = Some(Instant::now() - Duration::from_secs(1));
    h.app.update_web_preview();
    let pending = h.sent();
    h.app.fake_preview_command = Some(Command::new("designllm-nonexistent-preview.exe"));
    h.answer(&pending, "web.materialize", Ok(materialized(2)));
    assert!(h.app.web_preview_state.refresh_ticket.is_none());
    assert!(h.app.web_preview_state.requested_revision.is_none());
    assert!(h.message().unwrap().1.contains("다시 열어 주세요"));
}

#[test]
fn leaving_a_document_cancels_its_pending_preview_and_closes_the_child() {
    let _shared = shared_stderr_file();
    let mut h = Harness::new();
    h.open_document("web_code", 1);
    open_preview(&mut h);
    h.app.start_materialized_web_preview();
    let pending = h.sent();
    h.app.invoke_ui("nav.home", json!({}));
    assert!(h.app.web_preview_child.is_none());
    h.app.fake_preview_command = Some(fake_child());
    h.answer(&pending, "web.materialize", Ok(materialized(1)));
    assert!(h.app.web_preview_child.is_none(), "a late reply cannot reopen the preview");

    h.app.show_artifact(serde_json::from_value(summary("a1", "web_code", 1)).unwrap());
    open_preview(&mut h);
    h.app.show_artifact(serde_json::from_value(summary("a2", "canvas", 1)).unwrap());
    assert!(h.app.web_preview_child.is_none(), "switching documents closes the preview");
}

#[test]
fn revision_refresh_is_debounced_and_a_late_reply_cannot_replace_the_latest() {
    let _shared = shared_stderr_file();
    let mut h = Harness::new();
    h.open_document("web_code", 1);
    open_preview(&mut h);
    let original = h.app.web_preview_child.as_ref().unwrap().id();
    h.sent();
    h.app.active.as_mut().unwrap().revision = 2;
    h.app.update_web_preview();
    assert!(!h.sent().iter().any(|(_, method, _)| method == "web.materialize"));
    h.app.web_preview_state.changed_at = Some(Instant::now() - Duration::from_secs(1));
    h.app.update_web_preview();
    let old = h.sent();
    assert!(old.iter().any(|(_, method, args)| method == "web.materialize" && args["expected_revision"] == 2));

    h.app.active.as_mut().unwrap().revision = 3;
    h.app.update_web_preview();
    h.app.web_preview_state.changed_at = Some(Instant::now() - Duration::from_secs(1));
    h.app.update_web_preview();
    let latest = h.sent();
    assert!(latest.iter().any(|(_, method, args)| method == "web.materialize" && args["expected_revision"] == 3));
    h.app.fake_preview_command = Some(fake_child());
    h.answer(&latest, "web.materialize", Ok(materialized(3)));
    let current = h.app.web_preview_child.as_ref().unwrap().id();
    assert_ne!(original, current);
    h.app.fake_preview_command = Some(fake_child());
    h.answer(&old, "web.materialize", Ok(materialized(2)));
    assert_eq!(h.app.web_preview_child.as_ref().unwrap().id(), current);
}

#[test]
fn closing_the_preview_during_refresh_does_not_reopen_it() {
    let _shared = shared_stderr_file();
    let mut h = Harness::new();
    h.open_document("web_code", 1);
    open_preview(&mut h);
    h.sent();
    h.app.active.as_mut().unwrap().revision = 2;
    h.app.update_web_preview();
    h.app.web_preview_state.changed_at = Some(Instant::now() - Duration::from_secs(1));
    h.app.update_web_preview();
    let pending = h.sent();
    h.app.stop_web_preview_child().unwrap();
    h.app.fake_preview_command = Some(fake_child());
    h.answer(&pending, "web.materialize", Ok(materialized(2)));
    assert!(h.app.web_preview_child.is_none());
}

#[test]
fn board_opens_the_existing_web_mapping_editor() {
    for unavailable in [false, true] {
        let mut host = Host::start();
        let web = host.create("web_code", "웹");
        let mut window = Window::open(&mut host, &web);
        let entry = window.app.web_document().unwrap().entry.clone();
        window.app.web_runtime_unavailable = unavailable.then(|| "미리보기 사용 불가".into());
        window.app.view().left_tab = board::LeftTab::Ai;
        window.app.view().left_open = false;
        window.draw(&mut host, 1.0);
        crate::ui_record::begin();
        window.frame(&mut host, 1.0, Vec::new());
        let controls = crate::ui_record::finish();
        let control = controls.iter().find(|control| control.label == "원본 요소 편집" && control.visible && control.enabled)
            .expect("the board shows an enabled mapping editor button");
        window.click(&mut host, 1.0, control.rect.center(), false);
        assert_eq!(window.app.active_web_path.as_deref(), Some(entry.as_str()));
        assert!(window.app.view_ref().left_open);
        assert_eq!(window.app.view_ref().left_tab, board::LeftTab::Layers);
    }
}

#[test]
fn leaving_cancels_a_pending_canvas_play_export() {
    let mut h = Harness::new();
    h.open_document("canvas", 1);
    h.app.play_canvas().unwrap();
    let sent = h.sent();
    let ticket = sent.iter().find(|(_, method, _)| method == "artifact.export").unwrap().0;
    assert!(h.app.pending.contains_key(&ticket));
    h.app.invoke_ui("nav.home", json!({}));
    assert!(!h.app.pending.contains_key(&ticket));
}

#[test]
fn reopen_request_survives_polling_a_successfully_closed_preview() {
    let _shared = shared_stderr_file();
    let mut h = Harness::new();
    h.open_document("web_code", 1);
    let mut closing = Command::new("cmd");
    closing.args(["/C", "exit", "0"]);
    h.app.fake_preview_command = Some(closing);
    h.app.launch_web_preview(Path::new("unused.html"), None).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while h.app.web_preview_child.as_mut().unwrap().try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline, "the fake preview did not exit");
        std::thread::sleep(Duration::from_millis(10));
    }
    h.sent();
    h.app.start_materialized_web_preview();
    let pending = h.sent();
    let ticket = pending.iter().find(|(_, method, _)| method == "web.materialize").unwrap().0;
    h.app.poll_web_preview_child();
    assert!(h.app.pending.contains_key(&ticket), "a new request must survive the old window's exit");
    assert!(h.message().is_none(), "a normal close must not show an error");
    h.app.fake_preview_command = Some(fake_child());
    h.answer(&pending, "web.materialize", Ok(materialized(1)));
    assert!(h.app.web_preview_child.is_some(), "the new request opens a preview");
}

#[test]
fn close_then_reopen_before_old_process_exits_keeps_the_new_request() {
    use std::io::Write;
    let _shared = shared_stderr_file();
    let mut h = Harness::new();
    h.open_document("web_code", 1);
    let mut closing = Command::new("powershell.exe");
    closing.args(["-NoProfile", "-NonInteractive", "-Command", "$null = [Console]::ReadLine(); $null = [Console]::ReadLine(); exit 0"])
        .stdin(std::process::Stdio::piped());
    h.app.fake_preview_command = Some(closing);
    h.app.launch_web_preview(Path::new("unused.html"), None).unwrap();
    h.sent();
    writeln!(h.app.web_preview_child.as_mut().unwrap().stdin.as_mut().unwrap(), "close").unwrap();
    h.app.start_materialized_web_preview();
    let pending = h.sent();
    let ticket = pending.iter().find(|(_, method, _)| method == "web.materialize").unwrap().0;
    assert!(h.app.web_preview_child.as_mut().unwrap().try_wait().unwrap().is_none());
    writeln!(h.app.web_preview_child.as_mut().unwrap().stdin.as_mut().unwrap(), "exit").unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while h.app.web_preview_child.as_mut().unwrap().try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    h.app.poll_web_preview_child();
    assert!(h.app.pending.contains_key(&ticket));
    h.app.fake_preview_command = Some(fake_child());
    h.answer(&pending, "web.materialize", Ok(materialized(1)));
    assert!(h.app.web_preview_child.is_some());
}

#[test]
fn old_preview_failure_is_reported_without_canceling_the_new_request() {
    use std::io::Write;
    let _shared = shared_stderr_file();
    for (reason, expected) in [
        (designllm_web_preview::PREVIEW_MEMORY_LIMIT, "미리보기가 메모리 상한을 넘어 창을 닫았습니다. 다시 열어 주세요"),
        (designllm_web_preview::PREVIEW_UNRESPONSIVE, "미리보기 화면이 응답하지 않아 창을 닫았습니다. 다시 열어 주세요"),
    ] {
        for poll_first in [false, true] {
            let mut h = Harness::new();
            h.open_document("web_code", 1);
            open_preview(&mut h);
            h.sent();
            h.app.start_materialized_web_preview();
            let pending = h.sent();
            let ticket = pending.iter().find(|(_, method, _)| method == "web.materialize").unwrap().0;
            let input = h.app.web_preview_child.as_mut().unwrap().stdin.as_mut().unwrap();
            writeln!(input, "{{\"message\":\"{reason}; closed\"}}").unwrap();
            writeln!(input, "exit").unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            while h.app.web_preview_child.as_mut().unwrap().try_wait().unwrap().is_none() {
                assert!(Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(10));
            }
            if poll_first { h.app.poll_web_preview_child(); }
            assert!(h.app.pending.contains_key(&ticket));
            h.app.fake_preview_command = Some(fake_child());
            h.answer(&pending, "web.materialize", Ok(materialized(1)));
            assert!(h.app.web_preview_child.is_some());
            assert_eq!(h.message().unwrap().1, expected);
        }
    }
}

#[test]
fn abandoned_explicit_requests_forget_deferred_exit_notices() {
    let mut h = Harness::new();
    h.open_document("web_code", 1);
    h.app.start_materialized_web_preview();
    let pending = h.sent();
    let ticket = pending.iter().find(|(_, method, _)| method == "web.materialize").unwrap().0;
    h.app.web_preview_state.deferred_exit_notice = Some("old error".into());
    h.app.pending.remove(&ticket);
    h.app.update_web_preview();
    assert!(h.app.web_preview_state.deferred_exit_notice.is_none());
    h.app.start_materialized_web_preview();
    h.app.web_preview_state.deferred_exit_notice = Some("old error".into());
    h.app.cancel_materializing_previews();
    assert!(h.app.web_preview_state.deferred_exit_notice.is_none());
    h.app.web_preview_state.deferred_exit_notice = Some("old error".into());
    h.app.end_preview_launch(false);
    assert!(h.app.web_preview_state.deferred_exit_notice.is_none());
}

#[test]
fn stopping_an_already_failed_preview_reports_its_error_and_keeps_the_request() {
    use std::io::Write;
    let _shared = shared_stderr_file();
    let mut h = Harness::new();
    h.open_document("web_code", 1);
    open_preview(&mut h);
    h.sent();
    h.app.start_materialized_web_preview();
    let pending = h.sent();
    let ticket = pending.iter().find(|(_, method, _)| method == "web.materialize").unwrap().0;
    let input = h.app.web_preview_child.as_mut().unwrap().stdin.as_mut().unwrap();
    writeln!(input, "{{\"message\":\"{}; closed\"}}", designllm_web_preview::PREVIEW_MEMORY_LIMIT).unwrap();
    writeln!(input, "exit").unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while h.app.web_preview_child.as_mut().unwrap().try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    h.app.stop_web_preview_child().unwrap();
    assert!(h.app.pending.contains_key(&ticket));
    assert_eq!(h.message().unwrap().1, "미리보기가 메모리 상한을 넘어 창을 닫았습니다. 다시 열어 주세요");
    h.app.fake_preview_command = Some(fake_child());
    h.answer(&pending, "web.materialize", Ok(materialized(1)));
    assert!(h.app.web_preview_child.is_some());
    assert_eq!(h.message().unwrap().1, "미리보기가 메모리 상한을 넘어 창을 닫았습니다. 다시 열어 주세요");
}

#[test]
fn old_exit_after_new_ready_cannot_change_the_new_launch() {
    use std::io::Write;
    let _shared = shared_stderr_file();
    let mut host = Host::start();
    let web = host.create("web_code", "웹");
    let mut window = Window::open(&mut host, &web);
    window.app.web_runtime_unavailable = None;
    window.app.fake_preview_command = Some(fake_child());
    window.step(&mut host, "web.preview", json!({})).unwrap();
    let old_generation = window.app.web_preview_state.child_generation;
    window.app.fake_preview_command = Some(fake_child());
    let run = window.agent_run_granted(&mut host, "web.preview", json!({}), "read", &[]);
    let pid = window.app.web_preview_child.as_ref().unwrap().id();
    let child = window.app.web_preview_child.as_mut().unwrap();
    writeln!(child.stdin.as_mut().unwrap(), "end-frame").unwrap();
    writeln!(child.stdin.as_mut().unwrap(), "out designllm.preview.ready").unwrap();
    writeln!(child.stdin.as_mut().unwrap(), "end-frame").unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while window.app.web_preview_run.is_some() {
        assert!(Instant::now() < deadline);
        window.app.poll_web_preview_child();
        std::thread::sleep(Duration::from_millis(10));
    }
    window.settle(&mut host);
    assert_eq!(host.call("ui.command.get", json!({ "run_id": run["run_id"] })).unwrap()["status"], "completed");
    window.app.poll_preview_generation(old_generation);
    assert_eq!(window.app.web_preview_child.as_ref().unwrap().id(), pid);
    assert!(window.message().is_none());
}

#[test]
fn two_quick_preview_requests_ignore_the_older_reply() {
    let _shared = shared_stderr_file();
    for latest_first in [false, true] {
        let mut h = Harness::new();
        h.open_document("web_code", 1);
        h.sent();
        h.app.start_materialized_web_preview();
        let old = h.sent();
        h.app.start_materialized_web_preview();
        let new = h.sent();
        h.app.fake_preview_command = Some(fake_child());
        if !latest_first {
            h.answer(&old, "web.materialize", Ok(materialized(1)));
            assert!(h.app.web_preview_child.is_none(), "the canceled request must not open a window");
        }
        h.answer(&new, "web.materialize", Ok(materialized(1)));
        let pid = h.app.web_preview_child.as_ref().unwrap().id();
        if latest_first {
            h.app.fake_preview_command = Some(fake_child());
            h.answer(&old, "web.materialize", Ok(materialized(1)));
            assert_eq!(h.app.web_preview_child.as_ref().unwrap().id(), pid);
        }
    }
}
