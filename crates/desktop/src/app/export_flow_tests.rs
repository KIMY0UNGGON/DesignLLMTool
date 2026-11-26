//! S28/S29: loss questions of several exports are asked one after another, each naming its whole path; a retry keeps the
//! losses approved before; a page range is checked by the window only against the count a render of the request's revision
//! reported; a preview reply the window drops frees its key, a refused one is not asked again, and none is pending without a
//! ticket; the window does not close under a submitted export or an unanswered loss question; a deck's PPTX is the whole deck.

use designllm_contracts::{DltError, ErrorCode};
use serde_json::{json, Value};
use std::time::Instant;

use super::save_export::export_destination_label;
use super::test_support::*;
use crate::rpc::{RpcPayload, RpcReply};

fn dry_run(losses: Value) -> Value {
    json!({
        "dry_run": true, "artifact_id": "a1", "revision": 5, "format": "pdf", "mime": "application/pdf",
        "bytes": 10, "editability": "vector", "losses": losses, "diagnostics": [],
    })
}

fn loss(code: &str) -> Value {
    json!([{ "code": code, "message": code, "node_id": null, "blocking": false }])
}

fn reply(harness: &mut Harness, ticket: u64, result: Result<Value, DltError>) {
    harness.replies.send(RpcReply { ticket, result: result.map(RpcPayload::Json) }).expect("the window still listens");
    harness.app.process_replies(&harness.ctx);
}

/// Sends the export of the shown document to `path` and returns the ticket of its dry run.
fn submit(harness: &mut Harness, path: &str) -> u64 {
    harness.app.export_format = "pdf".into();
    harness.app.export_scope = "page".into();
    harness.app.export_path = path.into();
    harness.app.export_active_with(None).expect("the export is sent");
    let sent = harness.sent();
    assert_eq!(methods(&sent), ["artifact.export"]);
    assert_eq!(sent[0].2["dry_run"], json!(true));
    sent[0].0
}

fn opened() -> Harness {
    let mut harness = Harness::new();
    harness.open_document("canvas", 5);
    harness.sent();
    harness
}

fn shown_target(harness: &Harness) -> Option<String> {
    harness.app.export_confirmation.as_ref().map(|confirmation| confirmation.request.target_path.clone())
}

#[test]
fn loss_questions_of_two_exports_are_asked_one_after_the_other() {
    let mut harness = opened();
    let first = submit(&mut harness, "C:/out/a.pdf");
    let second = submit(&mut harness, "C:/out/b.pdf");
    reply(&mut harness, first, Ok(dry_run(loss("font"))));
    reply(&mut harness, second, Ok(dry_run(loss("image"))));
    assert_eq!(shown_target(&harness).as_deref(), Some("C:/out/a.pdf"), "the first answer is asked first");
    assert_eq!(harness.app.export_confirmations_waiting.len(), 1, "the second waits instead of replacing it");

    harness.app.cancel_export_confirmation();
    assert!(harness.sent().is_empty(), "a cancelled export writes nothing");
    assert_eq!(shown_target(&harness).as_deref(), Some("C:/out/b.pdf"), "the next export is asked about");

    harness.app.export_confirmation.as_mut().unwrap().allowed.insert("image".into());
    harness.app.apply_export_losses();
    let write = harness.sent();
    assert_eq!(methods(&write), ["artifact.export"]);
    assert_eq!(write[0].2["target_path"], json!("C:/out/b.pdf"));
    assert!(write[0].2["dry_run"].is_null());
    assert!(harness.app.export_confirmation.is_none() && harness.app.export_confirmations_waiting.is_empty());
}

#[test]
fn accepting_a_loss_question_moves_on_to_the_next_one() {
    let mut harness = opened();
    let first = submit(&mut harness, "C:/out/a.pdf");
    let second = submit(&mut harness, "C:/out/b.pdf");
    reply(&mut harness, first, Ok(dry_run(loss("font"))));
    reply(&mut harness, second, Ok(dry_run(loss("image"))));
    harness.app.export_confirmation.as_mut().unwrap().allowed.insert("font".into());
    harness.app.apply_export_losses();
    let write = harness.sent();
    assert_eq!(write[0].2["target_path"], json!("C:/out/a.pdf"));
    assert_eq!(shown_target(&harness).as_deref(), Some("C:/out/b.pdf"));
}

#[test]
fn the_loss_question_names_the_whole_path_it_writes() {
    // Two folders holding the same file name read apart.
    assert_eq!(export_destination_label("C:\\Draft\\report.pdf"), "C:\\Draft\\report.pdf");
    assert_ne!(export_destination_label("C:\\Draft\\report.pdf"), export_destination_label("C:\\Final\\report.pdf"));
    // A long path keeps its start and its end, the middle left out.
    let long = format!("C:\\{}\\보고서.pdf", "폴더".repeat(60));
    let label = export_destination_label(&long);
    assert!(label.starts_with("C:\\폴더") && label.ends_with("\\보고서.pdf") && label.contains('…'));
    assert!(label.chars().count() <= 72);
}

#[test]
fn a_retry_keeps_the_losses_approved_before() {
    let mut harness = opened();
    let dry = submit(&mut harness, "C:/out/a.pdf");
    reply(&mut harness, dry, Ok(dry_run(loss("font"))));
    harness.app.export_confirmation.as_mut().unwrap().allowed.insert("font".into());
    harness.app.apply_export_losses();
    let write = harness.sent();
    assert_eq!(write[0].2["options"]["allowed_losses"], json!(["font"]));
    // The engine finds another loss on the write; the user approves it too.
    let mut refusal = DltError::new(ErrorCode::ExportLossRequired, "손실");
    refusal.diagnostics = serde_json::from_value(loss("image")).unwrap();
    reply(&mut harness, write[0].0, Err(refusal));
    let confirmation = harness.app.export_confirmation.as_mut().expect("the new loss is asked about");
    assert_eq!(confirmation.request.options.allowed_losses, ["font"]);
    confirmation.allowed.insert("image".into());
    harness.app.apply_export_losses();
    let retry = harness.sent();
    assert_eq!(methods(&retry), ["artifact.export"]);
    assert_eq!(retry[0].2["options"]["allowed_losses"], json!(["font", "image"]), "the first approval is kept");
}

#[test]
fn a_page_range_is_checked_by_the_window_only_at_the_revision_it_holds() {
    let mut harness = opened();
    let dry = submit(&mut harness, "C:/out/a.pdf");
    let mut request = harness.app.pending_exports.get(&dry).cloned().expect("the dry run is recorded");
    request.options.page_indices = Some(vec![0, 4]);
    // A render of revision 5 reported one page: page 5 is not there.
    let rendered: designllm_contracts::RenderSnapshot = serde_json::from_value(snapshot("a1", 5)).unwrap();
    harness.app.remember_page_count(&rendered);
    assert_eq!(harness.app.page_count_at("a1", 5), Some(1));
    harness.app.send_export(request.clone());
    assert!(harness.sent().is_empty(), "a range past the known pages is not sent");
    assert!(harness.message().is_some());

    // At a revision the window has not read, the count is not known: the engine checks the range at that revision.
    request.options.expected_revision = 6;
    assert_eq!(harness.app.page_count_at("a1", 6), None);
    harness.app.send_export(request.clone());
    assert_eq!(methods(&harness.sent()), ["artifact.export"]);

    // A render of revision 6 reports five pages while revision 5's snapshot is still on screen: that count is revision 6's,
    // not the request's.
    let mut later = snapshot("a1", 6);
    later["scene"]["metadata"]["total_pages"] = json!(5);
    harness.app.remember_page_count(&serde_json::from_value(later).unwrap());
    harness.app.total_pages = 5;
    request.options.expected_revision = 5;
    assert_eq!(harness.app.page_count_at("a1", 5), None);
    assert_eq!(harness.app.page_count_at("a1", 6), Some(5));
    harness.app.send_export(request.clone());
    assert_eq!(methods(&harness.sent()), ["artifact.export"], "the engine checks revision 5");

    // A render that reports no count leaves it unknown (the count shown is then a guess).
    let mut silent = snapshot("a1", 5);
    silent["scene"]["metadata"] = json!({});
    harness.app.remember_page_count(&serde_json::from_value(silent).unwrap());
    assert_eq!(harness.app.page_count_at("a1", 5), None);
    harness.app.send_export(request);
    assert_eq!(methods(&harness.sent()), ["artifact.export"]);
}

#[test]
fn a_preview_reply_dropped_for_a_document_left_frees_its_key() {
    let mut harness = opened();
    harness.app.export_format = "pdf".into();
    harness.app.export_scope = "page".into();
    harness.app.preview_export_losses("");
    let preview = harness.sent();
    assert_eq!(methods(&preview), ["artifact.export"]);
    assert!(harness.app.export_preview_pending.is_some());
    // Another document is on screen when the answer comes: it is dropped, and its key with it.
    harness.app.active.as_mut().unwrap().id = "a2".into();
    reply(&mut harness, preview[0].0, Ok(dry_run(loss("font"))));
    assert!(harness.app.export_preview_pending.is_none());
    assert!(harness.app.export_preview.is_none(), "the dropped answer is not shown");
    // Back on the first document, the same settings are previewed again.
    harness.app.active.as_mut().unwrap().id = "a1".into();
    harness.app.preview_export_losses("");
    assert_eq!(methods(&harness.sent()), ["artifact.export"]);
}

#[test]
fn opening_another_document_frees_the_preview_key() {
    let mut harness = opened();
    harness.app.export_format = "pdf".into();
    harness.app.export_scope = "page".into();
    harness.app.preview_export_losses("");
    harness.sent();
    assert!(harness.app.export_preview_pending.is_some());
    harness.app.show_artifact(serde_json::from_value(summary("a2", "canvas", 3)).unwrap());
    assert!(harness.app.export_preview_pending.is_none());
}

#[test]
fn the_window_does_not_close_under_a_submitted_export() {
    let mut harness = opened();
    assert!(!harness.app.has_close_work());
    let dry = submit(&mut harness, "C:/out/a.pdf");
    assert!(harness.app.has_close_work(), "the dry run is on its way");
    assert!(harness.app.held_edit_close_warning().is_some_and(|text| text.contains("내보내기")));
    reply(&mut harness, dry, Ok(dry_run(json!([]))));
    let write = harness.sent();
    assert!(harness.app.has_close_work(), "the write is on its way");
    reply(&mut harness, write[0].0, Ok(json!({ "artifact_id": "a1" })));
    assert!(!harness.app.export_in_flight(), "the write's answer ends it");
    assert!(!harness.app.has_close_work());
}

#[test]
fn an_agent_export_with_a_target_holds_the_window_open_until_its_reply() {
    let mut harness = opened();
    harness.app.invoke("file.export", json!({ "format": "pdf", "scope": "page", "target_path": "C:/out/agent.pdf" })).expect("sent");
    let sent = harness.sent();
    assert_eq!(methods(&sent), ["artifact.export"]);
    assert!(harness.app.export_in_flight() && harness.app.has_close_work());
    reply(&mut harness, sent[0].0, Ok(json!({ "artifact_id": "a1", "target_path": "C:/out/agent.pdf" })));
    assert!(!harness.app.export_in_flight());
}

#[test]
fn an_unanswered_loss_question_holds_the_window_until_its_export_is_cancelled() {
    let mut harness = opened();
    let first = submit(&mut harness, "C:/out/a.pdf");
    let second = submit(&mut harness, "C:/out/b.pdf");
    // The answers that need approval come in the frame of the close: nothing is on its way any more.
    reply(&mut harness, first, Ok(dry_run(loss("font"))));
    reply(&mut harness, second, Ok(dry_run(loss("image"))));
    assert!(!harness.app.export_in_flight());
    assert!(harness.app.has_close_work(), "the questions are close work");
    assert!(harness.app.held_edit_close_warning().is_some_and(|text| text.contains("손실 확인")));
    harness.app.cancel_exports_and_close(&harness.ctx.clone());
    assert!(!harness.app.export_confirmations_open(), "both exports are dropped");
    assert!(!harness.app.has_close_work());
    assert!(harness.app.permit_close_once, "the window closes");
    assert!(harness.sent().is_empty(), "nothing is written");
}

#[test]
fn a_preview_is_not_pending_when_no_request_went_out() {
    let mut harness = opened();
    harness.app.export_format = "pdf".into();
    harness.app.export_scope = "page".into();
    harness.app.host_link.lost = true;
    harness.app.preview_export_losses("");
    assert!(harness.sent().is_empty());
    assert!(harness.app.export_preview_pending.is_none(), "no ticket, nothing pending");
    harness.app.host_link.lost = false;
    harness.app.preview_export_losses("");
    assert_eq!(methods(&harness.sent()), ["artifact.export"], "back with the host, the preview is asked");
}

#[test]
fn a_refused_preview_is_not_asked_again_until_the_settings_change() {
    let mut harness = opened();
    harness.app.export_format = "pdf".into();
    harness.app.export_scope = "page".into();
    harness.app.preview_export_losses("");
    let preview = harness.sent();
    reply(&mut harness, preview[0].0, Err(DltError::unsupported("no")));
    for _ in 0..3 {
        harness.app.preview_export_losses("");
    }
    assert!(harness.sent().is_empty(), "the open menu does not ask every frame");
    harness.app.export_format = "svg".into();
    harness.app.preview_export_losses("");
    assert_eq!(methods(&harness.sent()), ["artifact.export"], "other settings are asked");
}

fn deck_harness(slides: usize) -> Harness {
    use designllm_contracts::{Artifact, ArtifactBody, ArtifactKind, Page, Size, SlideDocument};
    let mut harness = Harness::new();
    let startup = harness.sent();
    harness.answer(&startup, "workspace.list", Ok(json!({ "workspace_id": "w1", "artifacts": [summary("a1", "slides", 1)] })));
    harness.app.invoke_ui("nav.open_project", json!({ "artifact_id": "a1" }));
    let opening = harness.sent();
    let view_id = opening.iter().find(|request| request.1 == "context.read").and_then(|request| request.2["view_id"].as_str()).unwrap().to_owned();
    harness.answer(&opening, "context.read", Ok(context("a1", &view_id, 1)));
    let pages = (1..=slides).map(|number| Page::new(format!("slide-{number}"), Size::slide())).collect();
    let mut deck = Artifact::new(ArtifactKind::Slides, "덱", "engine", ArtifactBody::Slides(SlideDocument { pages, styles: Default::default(), theme: None }));
    deck.id = "a1".into();
    deck.revision = 1;
    harness.answer(&opening, "artifact.inspect", Ok(serde_json::to_value(&deck).unwrap()));
    harness.sent();
    harness
}

#[test]
fn a_one_slide_deck_exports_pptx_as_the_whole_deck() {
    for slides in [1, 3] {
        let mut harness = deck_harness(slides);
        harness.app.export_format = "pptx".into();
        harness.app.export_scope = "all".into();
        assert_eq!(harness.app.export_scopes(), [("all", "모든 페이지")]);
        let request = harness.app.export_request("C:/out/deck.pptx".into(), None).expect("a request");
        assert_eq!(request.options.page_indices, None, "PPTX names no slide ({slides} slides)");
        // The agent's export without a scope is the whole deck.
        let named = harness.app.export_request("C:/out/deck.pptx".into(), Some(&json!({ "format": "pptx" }))).expect("a request");
        assert_eq!(named.options.page_indices, None);
        // One slide as PPTX makes no request (S30: refused, not widened).
        assert!(harness.app.export_request("C:/out/deck.pptx".into(), Some(&json!({ "format": "pptx", "scope": "page" }))).is_none());
        // PDF still names the page.
        harness.app.export_format = "pdf".into();
        harness.app.export_scope = "page".into();
        let pdf = harness.app.export_request("C:/out/deck.pdf".into(), None).expect("a request");
        assert_eq!(pdf.options.page_indices, Some(vec![0]));
    }
}

#[test]
fn an_explicit_slide_scope_for_a_deck_pptx_is_refused_and_not_widened() {
    use super::save_export::DECK_PPTX_SCOPE;
    let mut harness = deck_harness(3);
    for scope in ["page", "selection", "range"] {
        let mut args = json!({ "format": "pptx", "scope": scope, "target_path": "C:/out/deck.pptx" });
        if scope == "range" {
            args["pages"] = json!("2");
        }
        assert_eq!(harness.app.invoke("file.export", args).expect_err("refused"), DECK_PPTX_SCOPE, "scope {scope}");
        // The window's own export (no target) refuses before its save dialog.
        assert_eq!(harness.app.invoke("file.export", json!({ "format": "pptx", "scope": scope })).expect_err("refused"), DECK_PPTX_SCOPE);
    }
    assert!(harness.sent().iter().all(|request| request.1 != "artifact.export"), "nothing is exported");
    harness.app.invoke("file.export", json!({ "format": "pptx", "scope": "all", "target_path": "C:/out/deck.pptx" })).expect("the whole deck");
    let sent = harness.sent();
    let export = sent.iter().find(|request| request.1 == "artifact.export").expect("sent");
    assert!(export.2["options"]["page_indices"].is_null());
    crate::i18n::set_language(crate::i18n::Language::English);
    assert_eq!(crate::i18n::tr(DECK_PPTX_SCOPE), "PPTX exports all slides; use scope \"all\", or PDF/PNG for one slide");
    crate::i18n::set_language(crate::i18n::Language::Korean);
}

#[test]
fn the_slide_shortcut_opens_a_page_format_not_the_deck_pptx() {
    let mut harness = deck_harness(3);
    harness.app.export_format = "pptx".into();
    harness.app.export_scope = "all".into();
    harness.app.open_page_export();
    assert_eq!(harness.app.export_format, "pdf");
    assert_eq!(harness.app.export_scope, "page");
    // Another kind keeps its format.
    let mut canvas = opened();
    canvas.app.export_format = "png".into();
    canvas.app.open_page_export();
    assert_eq!(canvas.app.export_format, "png");
}

#[test]
fn back_on_a_deck_this_page_is_the_slide_the_board_selects() {
    let mut harness = deck_harness(3);
    harness.app.view().page_index = 2;
    harness.app.page_index = 2;
    harness.app.show_artifact(serde_json::from_value(summary("a2", "canvas", 1)).unwrap());
    harness.app.show_artifact(serde_json::from_value(summary("a1", "slides", 1)).unwrap());
    harness.sent();
    assert_eq!(harness.app.view_ref().page_index, 2, "the board selects slide 3");
    harness.app.export_format = "pdf".into();
    harness.app.export_scope = "page".into();
    let request = harness.app.export_request("C:/out/deck.pdf".into(), None).expect("a request");
    assert_eq!(request.options.page_indices, Some(vec![2]));
    let key = harness.app.export_key("");
    harness.app.view().page_index = 0;
    assert_ne!(harness.app.export_key(""), key, "the preview key follows the selected slide");
}

#[test]
fn an_agent_export_keeps_the_window_open_until_its_outcome_reaches_the_host() {
    use crate::rpc::Work;
    let root = std::env::temp_dir().join(format!("designllm-export-complete-{}", designllm_contracts::new_id()));
    std::fs::create_dir_all(&root).unwrap();
    let mut host = Host::start();
    let canvas = canvas_with_page(&mut host, "Board");
    let mut window = Window::open(&mut host, &canvas);
    window.app.screen = super::Screen::Editor;
    window.draw(&mut host, 1.0);
    window.app.publish_ui_if_due();
    window.settle(&mut host);
    let view_id = window.app.ui.view_id.clone();
    let listing = host.call("ui.command.list", json!({ "view_id": view_id })).unwrap();
    let target = root.join("agent.png").to_string_lossy().into_owned();
    host.call("ui.command.run", json!({
        "view_id": view_id, "expected_state_seq": listing["state_seq"], "command_id": "file.export", "actor": "agent",
        "permission": "file_write", "allowed_intents": [], "args": { "format": "png", "target_path": target },
    })).expect("queued");
    window.app.last_event_poll = Instant::now() - std::time::Duration::from_secs(3);
    window.app.poll_events();
    // The host answers everything but the run's report, which the close comes before.
    let mut held = None;
    let Window { work, app, ctx, replies, .. } = &mut window;
    settle_work(work, app, ctx, |item| {
        if let Work::Call { ticket, method, params, .. } = item {
            if method == "ui.command.complete" {
                held = Some((ticket, params));
            } else {
                replies.send(RpcReply { ticket, result: host.call(&method, params).map(RpcPayload::Json) }).unwrap();
            }
        }
    });
    let (ticket, params) = held.expect("the run's outcome is reported");
    assert!(!window.app.pending.values().any(|pending| matches!(pending.action, super::state::PendingAction::UiExport)), "the export answered");
    assert!(window.app.export_in_flight() && window.app.has_close_work(), "its report still has to reach the host");
    window.replies.send(RpcReply { ticket, result: host.call("ui.command.complete", params).map(RpcPayload::Json) }).unwrap();
    window.app.process_replies(&window.ctx);
    assert!(!window.app.export_in_flight() && !window.app.has_close_work());
}

#[test]
fn a_document_of_unknown_page_count_keeps_every_page_and_checks_ranges_only_against_its_revision() {
    let mut harness = Harness::new();
    let startup = harness.sent();
    harness.answer(&startup, "workspace.list", Ok(json!({ "workspace_id": "w1", "artifacts": [summary("a1", "docx", 1)] })));
    harness.app.invoke_ui("nav.open_project", json!({ "artifact_id": "a1" }));
    let opening = harness.sent();
    let view_id = opening.iter().find(|request| request.1 == "context.read").and_then(|request| request.2["view_id"].as_str()).unwrap().to_owned();
    harness.answer(&opening, "context.read", Ok(context("a1", &view_id, 1)));
    harness.sent();
    harness.app.export_format = "pdf".into();
    // No render reported the count: every page and a range stay offered, and "PDF로 인쇄" keeps `all`.
    assert_eq!(harness.app.shown_page_count(), None);
    let scopes: Vec<&str> = harness.app.export_scopes().iter().map(|(key, _)| *key).collect();
    assert!(scopes.contains(&"all") && scopes.contains(&"range"), "{scopes:?}");
    assert!(harness.app.check_page_range("pdf", "2-7").is_ok(), "the engine checks the range");
    // A count of another revision does not bound the range either.
    harness.app.page_count_read = Some(("a1".into(), 0, 2));
    assert!(harness.app.check_page_range("pdf", "2-7").is_ok());
    // The count of the shown revision does.
    harness.app.page_count_read = Some(("a1".into(), 1, 4));
    assert!(harness.app.check_page_range("pdf", "2-7").is_err());
    assert!(harness.app.check_page_range("pdf", "2-4").is_ok());
}

#[test]
fn a_passing_preview_failure_is_asked_again_and_a_reconnect_clears_a_refusal() {
    let mut harness = opened();
    harness.app.export_format = "pdf".into();
    harness.app.export_scope = "page".into();
    harness.app.preview_export_losses("");
    let preview = harness.sent();
    reply(&mut harness, preview[0].0, Err(DltError::new(ErrorCode::Unavailable, "link")));
    harness.app.preview_export_losses("");
    assert!(harness.sent().is_empty(), "not at once");
    // After the short wait it is asked again.
    if let Some((_, retry)) = harness.app.export_preview_failed.as_mut() {
        *retry = Some(Instant::now() - std::time::Duration::from_millis(1));
    }
    harness.app.preview_export_losses("");
    let again = harness.sent();
    assert_eq!(methods(&again), ["artifact.export"]);
    // A refusal holds until the host comes back.
    reply(&mut harness, again[0].0, Err(DltError::unsupported("no")));
    assert_eq!(harness.app.export_preview_failed.as_ref().map(|(_, retry)| retry.is_none()), Some(true));
    harness.app.preview_export_losses("");
    assert!(harness.sent().is_empty());
    harness.app.host_link.lost = true;
    let (answer, receive) = std::sync::mpsc::channel();
    answer.send(Ok(())).unwrap();
    harness.app.host_link.reconnecting = Some(receive);
    harness.app.poll_reconnect();
    assert!(harness.app.export_preview_failed.is_none(), "the new host is asked");
}

#[test]
fn the_close_button_names_how_many_exports_it_cancels() {
    let mut harness = opened();
    let first = submit(&mut harness, "C:/out/a.pdf");
    reply(&mut harness, first, Ok(dry_run(loss("font"))));
    assert_eq!(harness.app.cancel_exports_label(), "내보내기 취소하고 닫기");
    let second = submit(&mut harness, "C:/out/b.pdf");
    reply(&mut harness, second, Ok(dry_run(loss("image"))));
    assert_eq!(harness.app.cancel_exports_label(), "내보내기 2개 취소하고 닫기");
    crate::i18n::set_language(crate::i18n::Language::English);
    assert_eq!(crate::i18n::tr(&harness.app.cancel_exports_label()), "Cancel 2 exports and close");
    assert_eq!(crate::i18n::tr("내보내기 취소하고 닫기"), "Cancel export and close");
    crate::i18n::set_language(crate::i18n::Language::Korean);
}

// S31

#[test]
fn the_slide_shortcut_sets_pdf_and_one_slide_whatever_was_chosen_before() {
    for chosen in ["html", "pptx", "png"] {
        let mut harness = deck_harness(3);
        harness.app.export_format = chosen.into();
        harness.app.export_scope = "all".into();
        harness.app.open_page_export();
        assert_eq!((harness.app.export_format.as_str(), harness.app.export_scope.as_str()), ("pdf", "page"), "after {chosen}");
        // The menu keeps both: PDF is a deck format and one slide is one of its scopes.
        assert!(harness.app.export_format_supported("pdf"));
        assert!(harness.app.export_scopes().iter().any(|(key, _)| *key == "page"));
    }
    // A format the shown kind does not write (a Figure's PPTX on a Canvas) is not kept either.
    let mut canvas = opened();
    canvas.app.export_format = "zip".into();
    canvas.app.open_page_export();
    assert_eq!(canvas.app.export_format, "pdf");
}

#[test]
fn a_huge_range_is_refused_without_expanding_it() {
    use super::print_range::{parse_page_range, validate_page_range, PageRangeError, MAX_RANGE_PAGES, UNKNOWN_PAGE_COUNT};
    let started = Instant::now();
    assert_eq!(validate_page_range("1-999999999", UNKNOWN_PAGE_COUNT), Err(PageRangeError::TooMany { limit: MAX_RANGE_PAGES }));
    assert_eq!(parse_page_range("1-999999999", UNKNOWN_PAGE_COUNT), Err(PageRangeError::TooMany { limit: MAX_RANGE_PAGES }));
    assert_eq!(parse_page_range("1-4000000000, 4000000001-4294967295", UNKNOWN_PAGE_COUNT), Err(PageRangeError::TooMany { limit: MAX_RANGE_PAGES }));
    assert!(started.elapsed() < std::time::Duration::from_secs(1), "nothing was expanded");
    assert_eq!(PageRangeError::TooMany { limit: MAX_RANGE_PAGES }.to_string(), "쪽 범위는 한 번에 10000쪽까지 내보낼 수 있습니다");
    // Past any page a request can name, the words do not invent a page count.
    assert_eq!(validate_page_range("5000000000", UNKNOWN_PAGE_COUNT).unwrap_err().to_string(), "쪽 번호가 너무 큽니다");
    // A known count still bounds it, and a range under the cap is built.
    assert_eq!(validate_page_range("1-20", 10), Err(PageRangeError::OutOfRange { count: 10 }));
    assert_eq!(parse_page_range("2-4, 9", 10).unwrap(), vec![1, 2, 3, 8]);
    assert_eq!(validate_page_range(&format!("1-{MAX_RANGE_PAGES}"), UNKNOWN_PAGE_COUNT), Ok(MAX_RANGE_PAGES));
    // The window's check before a request, on a document whose count is not known yet.
    let harness = opened();
    assert_eq!(harness.app.check_page_range("pdf", "1-999999999").unwrap_err(), "쪽 범위는 한 번에 10000쪽까지 내보낼 수 있습니다");
    crate::i18n::set_language(crate::i18n::Language::English);
    assert_eq!(crate::i18n::tr("쪽 범위는 한 번에 10000쪽까지 내보낼 수 있습니다"), "A page range can export at most 10000 pages at a time");
    crate::i18n::set_language(crate::i18n::Language::Korean);
}

#[test]
fn the_selection_export_opens_in_png_with_the_selection_kept() {
    let mut harness = opened();
    harness.app.selected_nodes = vec!["n1".into()];
    harness.app.export_format = "pdf".into();
    harness.app.export_scope = "page".into();
    harness.app.open_selection_export();
    assert_eq!((harness.app.export_format.as_str(), harness.app.export_scope.as_str()), ("png", "selection"));
    assert!(harness.app.export_scopes().iter().any(|(key, _)| *key == "selection"), "the menu keeps the selection");
    harness.app.export_format = "svg".into();
    harness.app.export_scope = "page".into();
    harness.app.open_selection_export();
    assert_eq!((harness.app.export_format.as_str(), harness.app.export_scope.as_str()), ("svg", "selection"));
}

#[test]
fn a_deck_pptx_without_a_scope_is_the_whole_deck_and_an_unbuildable_request_is_an_error() {
    let mut harness = deck_harness(3);
    harness.app.export_scope = "page".into();
    let root = std::env::temp_dir().join(format!("designllm-deck-scope-{}", designllm_contracts::new_id()));
    std::fs::create_dir_all(&root).unwrap();
    crate::dialogs::set_test_answers(Some([root.join("deck.pptx")].into_iter().collect()));
    let outcome = harness.app.invoke("file.export", json!({ "format": "pptx" }));
    crate::dialogs::set_test_answers(None);
    outcome.expect("the whole deck is exported");
    assert_eq!(harness.app.export_scope, "all");
    let sent = harness.sent();
    let dry = sent.iter().find(|request| request.1 == "artifact.export").expect("its dry run is sent");
    assert!(dry.2["options"]["page_indices"].is_null());
    // Settings the window cannot build a request for are an error, never a success without a file.
    harness.app.export_path = "C:/out/deck.pdf".into();
    harness.app.export_format = "bogus".into();
    assert_eq!(harness.app.export_active_with(None), Err(super::save_export::EXPORT_NOT_BUILT.into()));
}

#[test]
fn a_document_still_loading_keeps_every_page_offered() {
    // The canvas's body is not read yet: its page count is unknown, and the board's one fallback page does not narrow `all`.
    let mut harness = opened();
    assert_eq!(harness.app.page_sizes().len(), 1);
    assert_eq!(harness.app.shown_page_count(), None);
    harness.app.export_format = "pdf".into();
    let scopes: Vec<&str> = harness.app.export_scopes().iter().map(|(key, _)| *key).collect();
    assert!(scopes.contains(&"all") && scopes.contains(&"range"), "{scopes:?}");
    // A deck read at its revision has a known count: one slide offers one slide only.
    let mut deck = deck_harness(1);
    deck.app.export_format = "pdf".into();
    let scopes: Vec<&str> = deck.app.export_scopes().iter().map(|(key, _)| *key).collect();
    assert_eq!(scopes, ["page"]);
    harness.sent();
}

#[test]
fn ctrl_e_opens_the_export_menu() {
    use eframe::egui::{Event, Key, Modifiers};
    let mut harness = opened();
    let ctrl = Modifiers { ctrl: true, command: true, ..Modifiers::NONE };
    let mut input = screen(1.0, 1280.0, 800.0);
    input.modifiers = ctrl;
    input.events = vec![Event::Key { key: Key::E, physical_key: Some(Key::E), pressed: true, repeat: false, modifiers: ctrl }];
    let ctx = harness.ctx.clone();
    let _ = ctx.run(input, |ctx| harness.app.handle_shortcuts(ctx));
    assert_eq!(harness.app.smoke_open.as_deref(), Some("export"), "the menu opens");
    assert!(harness.message().is_none(), "no refusal, got {:?}", harness.message());
    assert!(harness.sent().iter().all(|request| request.1 != "artifact.export"));
}

/// The bridge's own transport failure: the pipe failed, the report did not reach the host.
fn pipe_broke() -> DltError {
    let mut error = DltError::new(ErrorCode::Io, "the pipe broke");
    error.retryable = true;
    error
}

fn agent_export_window(host: &mut Host) -> Window {
    let canvas = canvas_with_page(host, "Board");
    let mut window = Window::open(host, &canvas);
    window.app.screen = super::Screen::Editor;
    window.draw(host, 1.0);
    window.app.publish_ui_if_due();
    window.settle(host);
    window
}

/// Queues the agent's `file.export` run and lets the window run it, the host answering everything but the reports of the
/// run's outcome, which are returned.
fn run_holding_reports(window: &mut Window, host: &mut Host, args: Value) -> Vec<(u64, Value)> {
    let view_id = window.app.ui.view_id.clone();
    let listing = host.call("ui.command.list", json!({ "view_id": view_id })).unwrap();
    host.call("ui.command.run", json!({
        "view_id": view_id, "expected_state_seq": listing["state_seq"], "command_id": "file.export", "actor": "agent",
        "permission": "file_write", "allowed_intents": [], "args": args,
    })).expect("queued");
    window.app.last_event_poll = Instant::now() - std::time::Duration::from_secs(3);
    window.app.poll_events();
    hold_reports(window, host)
}

fn hold_reports(window: &mut Window, host: &mut Host) -> Vec<(u64, Value)> {
    use crate::rpc::Work;
    let mut held = Vec::new();
    let Window { work, app, ctx, replies, .. } = window;
    settle_work(work, app, ctx, |item| {
        if let Work::Call { ticket, method, params, .. } = item {
            if method == "ui.command.complete" {
                held.push((ticket, params));
            } else {
                replies.send(RpcReply { ticket, result: host.call(&method, params).map(RpcPayload::Json) }).unwrap();
            }
        }
    });
    held
}

#[test]
fn a_local_refusal_of_an_agent_export_is_close_work_until_the_host_has_it() {
    let mut host = Host::start();
    let mut window = agent_export_window(&mut host);
    // A Canvas writes no PPTX: the window refuses the run itself, and its report is queued.
    let held = run_holding_reports(&mut window, &mut host, json!({ "format": "pptx", "target_path": "C:/out/never.pptx" }));
    let [(ticket, params)] = held.as_slice() else { panic!("one report, got {held:?}") };
    assert!(params["error"].is_object(), "the refusal is reported");
    assert!(window.app.has_close_work(), "the report has to reach the host first");
    window.replies.send(RpcReply { ticket: *ticket, result: host.call("ui.command.complete", params.clone()).map(RpcPayload::Json) }).unwrap();
    window.app.process_replies(&window.ctx);
    assert!(!window.app.has_close_work());
}

#[test]
fn a_report_lost_in_transit_is_sent_again_a_bounded_number_of_times() {
    let root = std::env::temp_dir().join(format!("designllm-export-retry-{}", designllm_contracts::new_id()));
    std::fs::create_dir_all(&root).unwrap();
    let mut host = Host::start();
    let mut window = agent_export_window(&mut host);
    let target = root.join("agent.png").to_string_lossy().into_owned();
    let mut held = run_holding_reports(&mut window, &mut host, json!({ "format": "png", "target_path": target }));
    // The written file's report fails in transit twice: it is sent again each time, the same report.
    for attempt in 0..2 {
        let [(ticket, params)] = held.as_slice() else { panic!("one report, got {held:?}") };
        let first = params.clone();
        window.replies.send(RpcReply { ticket: *ticket, result: Err(pipe_broke()) }).unwrap();
        window.app.process_replies(&window.ctx);
        assert!(window.app.has_close_work(), "attempt {attempt}: still on its way");
        held = hold_reports(&mut window, &mut host);
        assert_eq!(held.len(), 1, "sent again");
        assert_eq!(held[0].1, first, "the same outcome");
    }
    // The third failure is the last: the window gives up and the close is no longer held.
    let (ticket, _) = held[0].clone();
    window.replies.send(RpcReply { ticket, result: Err(pipe_broke()) }).unwrap();
    window.app.process_replies(&window.ctx);
    assert!(hold_reports(&mut window, &mut host).is_empty(), "three attempts in all");
    assert!(!window.app.has_close_work());
}

#[test]
fn a_report_the_host_refuses_is_not_sent_again() {
    let mut host = Host::start();
    let mut window = agent_export_window(&mut host);
    let held = run_holding_reports(&mut window, &mut host, json!({ "format": "pptx", "target_path": "C:/out/never.pptx" }));
    let (ticket, _) = held[0].clone();
    window.replies.send(RpcReply { ticket, result: Err(DltError::new(ErrorCode::NotFound, "no such run")) }).unwrap();
    window.app.process_replies(&window.ctx);
    assert!(hold_reports(&mut window, &mut host).is_empty());
    assert!(!window.app.has_close_work());
}

// S32

/// The rectangle of the first text shape that reads `text`.
fn text_rect(output: &eframe::egui::FullOutput, text: &str) -> Option<eframe::egui::Rect> {
    use eframe::egui::epaint::Shape;
    fn walk(shape: &Shape, text: &str) -> Option<eframe::egui::Rect> {
        match shape {
            Shape::Text(shown) if shown.galley.text() == text => Some(eframe::egui::Rect::from_min_size(shown.pos, shown.galley.size())),
            Shape::Vec(shapes) => shapes.iter().find_map(|shape| walk(shape, text)),
            _ => None,
        }
    }
    output.shapes.iter().find_map(|clipped| walk(&clipped.shape, text))
}

/// Draws one `내보내기 ›` row of the properties panel (`scope`) and clicks it when it is drawn. Returns whether it was drawn.
fn click_export_row(harness: &mut Harness, scope: &str) -> bool {
    use eframe::egui::{CentralPanel, Event, Modifiers, PointerButton};
    crate::theme::install(&harness.ctx);
    let ctx = harness.ctx.clone();
    let palette = crate::theme::palette(&ctx);
    let draw = |harness: &mut Harness, events: Vec<Event>| {
        let mut input = screen(1.0, 400.0, 300.0);
        input.events = events;
        ctx.run(input, |ctx| {
            CentralPanel::default().show(ctx, |ui| {
                let mut first = true;
                harness.app.export_row(ui, &palette, &mut first, scope);
            });
        })
    };
    let shown = draw(harness, Vec::new());
    let Some(rect) = text_rect(&shown, "내보내기") else {
        return false;
    };
    let at = rect.center();
    let button = |pressed| Event::PointerButton { pos: at, button: PointerButton::Primary, pressed, modifiers: Modifiers::NONE };
    draw(harness, vec![Event::PointerMoved(at), button(true), button(false)]);
    true
}

#[test]
fn the_properties_page_row_of_a_deck_opens_one_slide_in_pdf_after_pptx() {
    let mut harness = deck_harness(3);
    harness.app.export_format = "pptx".into();
    harness.app.export_scope = "all".into();
    assert!(click_export_row(&mut harness, "page"), "the page row is drawn");
    assert_eq!((harness.app.export_format.as_str(), harness.app.export_scope.as_str()), ("pdf", "page"));
    assert_eq!(harness.app.smoke_open.as_deref(), Some("export"));
    // The menu keeps both (no widening to every slide).
    assert!(harness.app.export_scopes().iter().any(|(key, _)| *key == "page"));
}

#[test]
fn only_a_canvas_object_offers_the_selection_export_row() {
    // A Figure panel and a slide object export their whole Figure or slide: no row.
    for kind in [designllm_contracts::ArtifactKind::Figure, designllm_contracts::ArtifactKind::Slides] {
        let mut harness = opened();
        harness.app.active.as_mut().unwrap().kind = kind;
        harness.app.selected_nodes = vec!["n1".into()];
        assert!(!harness.app.selection_export_supported());
        assert!(!click_export_row(&mut harness, "selection"), "{kind:?}: no selection row");
        assert!(harness.app.smoke_open.is_none());
    }
    // A Canvas object: the row, and the menu opens on the selection in PNG.
    let mut harness = opened();
    harness.app.selected_nodes = vec!["n1".into()];
    harness.app.export_format = "pdf".into();
    assert!(click_export_row(&mut harness, "selection"));
    assert_eq!((harness.app.export_format.as_str(), harness.app.export_scope.as_str()), ("png", "selection"));
    assert!(harness.app.export_scopes().iter().any(|(key, _)| *key == "selection"), "the selection is kept");
}

#[test]
fn a_reopened_document_with_a_known_count_keeps_every_page_before_its_render() {
    let mut harness = Harness::new();
    let startup = harness.sent();
    harness.answer(&startup, "workspace.list", Ok(json!({ "workspace_id": "w1", "artifacts": [summary("a1", "docx", 1)] })));
    harness.app.invoke_ui("nav.open_project", json!({ "artifact_id": "a1" }));
    let opening = harness.sent();
    let view_id = opening.iter().find(|request| request.1 == "context.read").and_then(|request| request.2["view_id"].as_str()).unwrap().to_owned();
    harness.answer(&opening, "context.read", Ok(context("a1", &view_id, 1)));
    harness.sent();
    // The five pages read before at this revision; the board has no render yet and shows one fallback page.
    harness.app.page_count_read = Some(("a1".into(), 1, 5));
    harness.app.total_pages = 0;
    assert_eq!(harness.app.page_sizes().len(), 1);
    assert!(harness.app.shown_document_multipage());
    harness.app.export_format = "pdf".into();
    harness.app.export_scope = "all".into();
    let scopes: Vec<&str> = harness.app.export_scopes().iter().map(|(key, _)| *key).collect();
    assert!(scopes.contains(&"all") && scopes.contains(&"range"), "{scopes:?}");
    // A count of one page is one page.
    harness.app.page_count_read = Some(("a1".into(), 1, 1));
    assert!(!harness.app.shown_document_multipage());
    // Every slide of a deck of three in one PNG is refused on the deck's count.
    let mut deck = deck_harness(3);
    let error = deck.app.invoke("file.export", json!({ "format": "png", "scope": "all", "target_path": "C:/out/a.png" })).expect_err("refused");
    assert_eq!(error, super::save_export::ONE_PAGE_FORMAT);
}

#[test]
fn a_report_survives_a_disconnect_and_is_sent_after_the_reconnect() {
    let root = std::env::temp_dir().join(format!("designllm-export-reconnect-{}", designllm_contracts::new_id()));
    std::fs::create_dir_all(&root).unwrap();
    let mut host = Host::start();
    let mut window = agent_export_window(&mut host);
    let target = root.join("agent.png").to_string_lossy().into_owned();
    let held = run_holding_reports(&mut window, &mut host, json!({ "format": "png", "target_path": target }));
    let (ticket, report) = held[0].clone();
    // The link is lost: the failed report waits, nothing is sent, and the close still waits for it.
    window.app.host_link.lost = true;
    window.replies.send(RpcReply { ticket, result: Err(pipe_broke()) }).unwrap();
    window.app.process_replies(&window.ctx);
    for _ in 0..5 {
        window.app.process_replies(&window.ctx);
    }
    assert!(hold_reports(&mut window, &mut host).is_empty(), "nothing goes out while the link is lost");
    assert_eq!(window.app.export_reports_waiting.len(), 1);
    assert_eq!(window.app.export_reports_waiting[0].1, 1, "only the send that went out is counted");
    assert!(window.app.has_close_work());
    // Back: the same report goes again.
    window.app.host_link.lost = false;
    window.app.host_link.probe = None;
    window.app.process_replies(&window.ctx);
    let again = hold_reports(&mut window, &mut host);
    assert_eq!(again.len(), 1);
    assert_eq!(again[0].1, report, "the same outcome");
    window.replies.send(RpcReply { ticket: again[0].0, result: host.call("ui.command.complete", again[0].1.clone()).map(RpcPayload::Json) }).unwrap();
    window.app.process_replies(&window.ctx);
    assert!(!window.app.has_close_work());
}

#[test]
fn an_expired_view_refusing_a_report_ends_it() {
    let root = std::env::temp_dir().join(format!("designllm-export-expired-{}", designllm_contracts::new_id()));
    std::fs::create_dir_all(&root).unwrap();
    let mut host = Host::start();
    let mut window = agent_export_window(&mut host);
    let target = root.join("agent.png").to_string_lossy().into_owned();
    let held = run_holding_reports(&mut window, &mut host, json!({ "format": "png", "target_path": target }));
    let (ticket, _) = held[0].clone();
    window.replies.send(RpcReply { ticket, result: Err(DltError::new(ErrorCode::Unavailable, "the view expired")) }).unwrap();
    window.app.process_replies(&window.ctx);
    assert!(hold_reports(&mut window, &mut host).is_empty(), "not sent again");
    assert!(window.app.export_reports_waiting.is_empty());
    assert!(!window.app.has_close_work());
}
