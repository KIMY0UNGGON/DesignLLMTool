//! S27: GUI exports and profiled Figure creations sent one after the other each settle their own request, an export goes
//! on with its own document when another one is opened before its reply, and a loss warning read for one document is
//! never shown or accepted for another.

use serde_json::json;

use super::{test_support::*, Screen};
use crate::rpc::{RpcPayload, RpcReply};

fn open(host: &mut Host, artifact: &str) -> Window {
    let mut window = Window::open(host, artifact);
    window.app.screen = Screen::Editor;
    window.draw(host, 1.0);
    window
}

fn scratch(name: &str) -> std::path::PathBuf {
    let root = std::env::temp_dir().join(format!("designllm-{name}-{}", designllm_contracts::new_id()));
    std::fs::create_dir_all(&root).expect("a folder");
    root
}

/// Sends the GUI export of the shown document to `path` (as the save dialog's answer does), without letting the host answer.
fn submit_export(window: &mut Window, path: &std::path::Path) {
    window.app.export_format = "png".into();
    window.app.export_scope = "page".into();
    window.app.export_path = path.to_string_lossy().into_owned();
    window.app.export_active_with(None).expect("the export is sent");
}

#[test]
fn two_exports_sent_one_after_the_other_both_write_their_own_file() {
    let root = scratch("export-overlap");
    let mut host = Host::start();
    let canvas = canvas_with_page(&mut host, "Board");
    let mut window = open(&mut host, &canvas);
    let (first, second) = (root.join("a.png"), root.join("b.png"));
    submit_export(&mut window, &first);
    // B is submitted before A's dry run has an answer.
    submit_export(&mut window, &second);
    assert_eq!(window.app.pending_exports.len(), 2, "each export waits on its own reply");
    window.settle(&mut host);
    assert!(first.exists(), "A is written");
    assert!(second.exists(), "B is written");
    assert!(window.app.pending_exports.is_empty());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn an_export_goes_on_with_its_own_document_when_another_one_is_opened_before_its_reply() {
    let root = scratch("export-switch");
    let mut host = Host::start();
    let figure_a = host.create("figure", "A");
    let figure_b = host.create("figure", "B");
    let mut window = open(&mut host, &figure_a);
    let target = root.join("a.png");
    submit_export(&mut window, &target);
    // The person opens B before A's dry run is answered.
    let list = host.call("workspace.list", json!({})).expect("the work list");
    let summary = list["artifacts"].as_array().and_then(|all| all.iter().find(|item| item["id"] == figure_b)).cloned().expect("B listed");
    window.app.activate(serde_json::from_value(summary).expect("a summary"));
    window.settle(&mut host);
    assert_eq!(window.app.active.as_ref().map(|active| active.id.as_str()), Some(figure_b.as_str()));
    assert!(target.exists(), "A's export is written although B is shown");
    let written = window.calls.iter().filter(|(method, params, _)| method == "artifact.export" && params.get("dry_run").is_none_or(|dry| dry.is_null() || dry == false))
        .map(|(_, params, _)| params["options"]["artifact_id"].as_str().unwrap_or_default().to_owned()).collect::<Vec<_>>();
    assert_eq!(written, vec![figure_a.clone()], "the write was asked for A itself");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_loss_warning_read_for_one_document_is_never_shown_or_accepted_for_another() {
    let mut host = Host::start();
    let first = canvas_with_page(&mut host, "First");
    let second = canvas_with_page(&mut host, "Second");
    let mut window = open(&mut host, &first);
    window.app.export_format = "png".into();
    window.app.export_scope = "page".into();
    let first_key = window.app.export_key("");
    let loss = designllm_contracts::Diagnostic { code: "ROTATION_NOT_RENDERED".into(), message: "rotated".into(), node_id: None, blocking: true, params: Default::default() };
    window.app.export_preview = Some((first_key.clone(), vec![loss]));
    // The same settings, the same revision, another document.
    let list = host.call("workspace.list", json!({})).expect("the work list");
    let summary = list["artifacts"].as_array().and_then(|all| all.iter().find(|item| item["id"] == second)).cloned().expect("listed");
    window.app.activate(serde_json::from_value(summary).expect("a summary"));
    window.settle(&mut host);
    window.app.export_format = "png".into();
    window.app.export_scope = "page".into();
    assert_ne!(window.app.export_key(""), first_key, "the key names the document");
    window.app.export_path = std::env::temp_dir().join(format!("designllm-loss-{}.png", designllm_contracts::new_id())).to_string_lossy().into_owned();
    window.app.export_active_with(None).expect("the export is sent");
    let request = window.app.pending_exports.values().next().expect("the export waits for its dry run");
    assert_eq!(request.options.artifact_id, second);
    assert!(request.options.allowed_losses.is_empty(), "the first document's warning permits nothing here");
    // The selection is part of the key as well.
    window.app.export_scope = "selection".into();
    let none_selected = window.app.export_key("");
    window.app.selected_nodes = vec!["n1".into()];
    assert_ne!(window.app.export_key(""), none_selected);
}

/// Answers the request sent with `ticket` with `answer` and lets the window handle it.
fn answer_ticket(harness: &mut Harness, ticket: u64, answer: serde_json::Value) {
    harness.replies.send(RpcReply { ticket, result: Ok(RpcPayload::Json(answer)) }).expect("the window listens");
    harness.app.process_replies(&harness.ctx);
}

#[test]
fn two_profiled_figure_creations_each_keep_their_profile() {
    let mut harness = Harness::new();
    harness.sent();
    let create = |harness: &mut Harness, preset: &str| {
        harness.app.screen = Screen::Home;
        harness.app.home.tile = "figure".into();
        harness.app.invoke("home.preset", json!({ "preset": preset })).expect("the profile is accepted");
        harness.app.create_from_home_command(&json!({ "tile": "figure" })).expect("the figure is requested");
        harness.sent().into_iter().find(|(_, method, _)| method == "artifact.create").expect("the create request").0
    };
    let print = create(&mut harness, "high_res_print");
    // Home again, a second profiled Figure before the first reply.
    let single = create(&mut harness, "single_column");
    answer_ticket(&mut harness, print, summary("print-figure", "figure", 0));
    answer_ticket(&mut harness, single, summary("single-figure", "figure", 0));
    let view = |id: &str| harness.app.editor_views.get(id).map(|view| (view.figure_output_profile.clone(), view.figure_export_dpi));
    assert_eq!(view("print-figure"), Some((Some("high_res_print".to_owned()), 600)), "the first creation keeps its profile");
    assert_eq!(view("single-figure"), Some((Some("single_column".to_owned()), 300)));
    assert!(harness.app.pending_figure_create_profiles.is_empty());
}

/// Answers the window's queued requests once each (the host answers every one), without the follow-up rounds `settle` runs:
/// the requests these answers start stay queued, so a test can look at them before the host answers.
fn answer_queued_once(window: &mut Window, host: &mut Host) {
    let queued: Vec<crate::rpc::Work> = std::iter::from_fn(|| window.work.try_recv().ok()).collect();
    for work in queued {
        if let crate::rpc::Work::Call { ticket, method, params, .. } = work {
            let result = host.call(&method, params).map(RpcPayload::Json);
            window.replies.send(RpcReply { ticket, result }).expect("the window listens");
        }
    }
    let ctx = window.ctx.clone();
    window.app.process_replies(&ctx);
}

/// The documents the window's waiting export calls are recorded under.
fn export_records(window: &Window) -> Vec<String> {
    window.app.pending.values()
        .filter(|pending| pending.method == "artifact.export")
        .filter_map(|pending| pending.artifact_id.clone())
        .collect()
}

#[test]
fn an_export_write_sent_after_a_switch_is_recorded_under_its_own_document() {
    let root = scratch("export-record");
    let mut host = Host::start();
    let figure_a = host.create("figure", "A");
    let figure_b = host.create("figure", "B");

    // A shown: the export's dry run and then its write are A's.
    let mut shown = open(&mut host, &figure_a);
    submit_export(&mut shown, &root.join("shown.png"));
    assert_eq!(export_records(&shown), vec![figure_a.clone()], "the dry run is A's");
    answer_queued_once(&mut shown, &mut host);
    assert_eq!(export_records(&shown), vec![figure_a.clone()], "the write is A's");

    // The same export, but B is opened before A's dry run is answered: the write still counts for A, never for B.
    let mut window = open(&mut host, &figure_a);
    submit_export(&mut window, &root.join("switched.png"));
    let list = host.call("workspace.list", json!({})).expect("the work list");
    let summary = list["artifacts"].as_array().and_then(|all| all.iter().find(|item| item["id"] == figure_b)).cloned().expect("B listed");
    window.app.activate(serde_json::from_value(summary).expect("a summary"));
    for _ in 0..6 {
        if window.app.pending.values().any(|pending| pending.method == "artifact.export" && matches!(pending.action, super::PendingAction::Export)) {
            break;
        }
        answer_queued_once(&mut window, &mut host);
    }
    assert_eq!(window.app.active.as_ref().map(|active| active.id.as_str()), Some(figure_b.as_str()));
    let write = window.app.pending.values().find(|pending| matches!(pending.action, super::PendingAction::Export)).expect("A's write is in flight");
    assert_eq!(write.artifact_id.as_deref(), Some(figure_a.as_str()), "the write is recorded under A, as when A is shown");
    assert!(!export_records(&window).contains(&figure_b), "B has no export of its own in flight");
    window.settle(&mut host);
    assert!(root.join("switched.png").exists(), "A's export is written");
    let _ = std::fs::remove_dir_all(&root);
}
