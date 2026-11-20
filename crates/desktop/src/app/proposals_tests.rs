//! Tests of the proposal rows and the AI request's delivery choice (proposals.rs). A proposal is made the way an agent makes one
//! (`proposal.create`), the window learns of it from the host's events, and the row's command runs against the real host.
//! The parity tests send the same work by the MCP tool (BRIEF 17).

use std::{collections::BTreeSet, time::Instant};

use designllm_contracts::{new_id, ChangeSet, DeliverAs, DltError, ErrorCode, Intent, Job, Node, Operation, UiPermission, UiRun, UiRunStatus};
use eframe::egui::{self, Context, FullOutput};
use serde_json::{json, Value};

use super::{
    proposals::{APPLY_UNKNOWN, STALE},
    test_support::{canvas_with_page, change_set_arguments, content_of, drawn_text, screen, snapshot, Harness, Host, HostBackend, Mcp, Window},
    thumbs, DesktopApp,
};
use crate::{
    commands,
    rpc::{RpcPayload, RpcReply, Work},
    theme,
};

/// A canvas with one text box `a` reading `text`, on the page `page-1` that `canvas_with_page` makes.
fn canvas_with_text(host: &mut Host, title: &str, text: &str) -> String {
    let id = canvas_with_page(host, title);
    let node: Node = serde_json::from_value(json!({
        "id": "a", "kind": "text", "text": text,
        "bounds": { "x": 0, "y": 1_000_000, "width": 1_000_000, "height": 400_000 },
    }))
    .expect("a text node");
    let intents = [Intent::Content].into_iter().collect();
    host.apply(&id, vec![Operation::AddNode { page_id: "page-1".into(), parent_id: None, node }], intents).expect("the box");
    id
}

/// An agent's proposal on `artifact`: box `a` reads `text` once it is applied. It is made on the artifact's current revision.
fn propose(host: &mut Host, artifact: &str, text: &str) -> String {
    let revision = host.artifact(artifact).revision;
    let workspace = host.workspace_id.clone();
    let operations = vec![Operation::SetText { node_id: "a".into(), text: text.into() }];
    let change = change_set_arguments(&workspace, artifact, revision, &[Intent::Content], operations);
    let proposal = host.call("proposal.create", json!({ "change_set": change })).expect("the proposal is stored");
    proposal["id"].as_str().expect("the proposal has an id").to_owned()
}

/// Another change to box `a` by the desktop itself, as someone else's commit would make it.
fn change_box(host: &mut Host, artifact: &str, text: &str) {
    let operations = vec![Operation::SetText { node_id: "a".into(), text: text.into() }];
    host.apply(artifact, operations, [Intent::Content].into_iter().collect()).expect("the change");
}

/// The window takes the host's events as its poll would: everything the host kept, the ones it already has skipped.
fn feed_events(host: &mut Host, window: &mut Window) {
    let batch = host.call("events.poll", json!({ "after_seq": 0, "limit": 1000 })).expect("the events");
    window.app.handle_events(batch);
    window.settle(host);
}

/// A context with the app's fonts and theme, as the window installs them at start (text cannot be laid out without them).
fn themed() -> Context {
    let ctx = Context::default();
    theme::install(&ctx);
    ctx
}

/// One frame of the proposal rows, as the AI tab draws them.
fn draw_rows(app: &mut DesktopApp, ctx: &Context) -> FullOutput {
    ctx.run(screen(1.0, 1000.0, 800.0), |ctx| {
        egui::SidePanel::left("proposal-rows").exact_width(theme::SIDEBAR_WIDTH).show(ctx, |ui| {
            let palette = theme::palette(ctx);
            app.draw_proposals(ui, &palette);
        });
    })
}

/// The `proposal.preview` answer of proposal `id` made on `artifact` at `base`: a snapshot whose picture is a 2 by 2 PNG stand-in.
fn preview_of(id: &str, artifact: &str, base: u64) -> Value {
    let change = serde_json::to_value(ChangeSet {
        request_id: new_id(),
        actor: "mcp".into(),
        workspace_id: "w1".into(),
        artifact_id: artifact.into(),
        base_revision: base,
        allowed_intents: [Intent::Content].into_iter().collect(),
        job_ref: None,
        read_dependencies: Vec::new(),
        operations: Vec::new(),
    })
    .expect("a change set");
    let mut render = snapshot(artifact, base);
    render["preview"] = json!({ "sha256": "f".repeat(64), "mime": "image/png", "bytes": 16 });
    json!({ "id": id, "change_set": change, "status": "validated", "preview": render })
}

#[test]
fn the_list_has_the_proposals_of_the_open_document_and_no_other() {
    let mut host = Host::start();
    let first = canvas_with_text(&mut host, "첫째", "가");
    let second = canvas_with_text(&mut host, "둘째", "가");
    let before = content_of(&host.artifact(&first));
    let first_proposal = propose(&mut host, &first, "나");
    propose(&mut host, &second, "다");
    let mut window = Window::open(&mut host, &first);
    feed_events(&mut host, &mut window);
    assert_eq!(window.app.listed_proposal_ids(), [first_proposal], "only the open document's proposal is listed");
    assert_eq!(content_of(&host.artifact(&first)), before, "an unapplied proposal changes nothing");
}

#[test]
fn a_row_draws_its_preview_render_as_its_picture() {
    let mut h = Harness::new();
    theme::install(&h.ctx);
    let _ = h.open_document("canvas", 1);
    h.app.handle_events(json!({ "events": [{
        "event_seq": 1, "event": "proposal.created", "artifact_id": "a1", "revision": 1,
        "detail": { "proposal_id": "p1", "status": "validated", "job_id": null },
    }] }));
    let _ = h.sent();
    let _ = draw_rows(&mut h.app, &h.ctx);
    let requests = h.sent();
    h.answer(&requests, "proposal.preview", Ok(preview_of("p1", "a1", 1)));
    let ticket = std::iter::from_fn(|| h.work.try_recv().ok())
        .find_map(|work| match work {
            Work::ReadAsset { ticket, .. } => Some(ticket),
            _ => None,
        })
        .expect("the preview's picture is read");
    let picture = RpcPayload::Image { mime: "image/png".into(), image: egui::ColorImage::from_rgba_unmultiplied([2, 2], &[255; 16]) };
    h.replies.send(RpcReply { ticket, result: Ok(picture) }).expect("the window listens");
    h.app.process_replies(&h.ctx);
    assert!(thumbs::picture_of(&h.ctx, "proposal:p1").is_some(), "the row shows the preview render");
}

#[test]
fn a_row_that_can_be_applied_draws_its_apply_button() {
    let mut host = Host::start();
    let canvas = canvas_with_text(&mut host, "그림판", "가");
    propose(&mut host, &canvas, "나");
    let mut window = Window::open(&mut host, &canvas);
    feed_events(&mut host, &mut window);
    let drawn = drawn_text(&draw_rows(&mut window.app, &themed()));
    assert!(drawn.iter().any(|text| text == "적용"), "the apply button is drawn: {drawn:?}");
    assert!(drawn.iter().all(|text| text != STALE), "no reason is drawn for a row that can be applied");
}

#[test]
fn applying_a_row_commits_one_change_and_one_undo_takes_it_back() {
    let mut host = Host::start();
    let canvas = canvas_with_text(&mut host, "그림판", "가");
    let before = content_of(&host.artifact(&canvas));
    let proposal = propose(&mut host, &canvas, "나");
    let mut window = Window::open(&mut host, &canvas);
    feed_events(&mut host, &mut window);
    window.step(&mut host, "proposal.apply", json!({ "proposal_id": proposal })).expect("the row applies");
    let applied = content_of(&host.artifact(&canvas));
    assert_ne!(before, applied, "the proposal changed the document");
    let revision = host.artifact(&canvas).revision;
    assert_eq!(window.app.active.as_ref().map(|active| active.revision), Some(revision), "the window shows the new revision");
    let drawn = drawn_text(&draw_rows(&mut window.app, &themed()));
    assert!(drawn.iter().any(|text| text == "이미 적용했습니다"), "the row shows it is applied at once: {drawn:?}");
    window.step(&mut host, "edit.undo", json!({})).expect("one undo");
    assert_eq!(content_of(&host.artifact(&canvas)), before, "one undo takes the whole proposal back");
}

#[test]
fn a_row_on_another_revision_cannot_be_applied_and_the_reason_is_drawn() {
    let mut host = Host::start();
    let canvas = canvas_with_text(&mut host, "그림판", "가");
    let proposal = propose(&mut host, &canvas, "나");
    let mut window = Window::open(&mut host, &canvas);
    feed_events(&mut host, &mut window);
    // Someone else changes the document after the proposal was made, and the window learns of it.
    change_box(&mut host, &canvas, "외부");
    let moved = content_of(&host.artifact(&canvas));
    feed_events(&mut host, &mut window);
    assert_eq!(window.app.invoke("proposal.apply", json!({ "proposal_id": proposal })), Err(STALE.to_owned()));
    window.settle(&mut host);
    assert_eq!(content_of(&host.artifact(&canvas)), moved, "the refused row changes nothing");
    let drawn = drawn_text(&draw_rows(&mut window.app, &themed()));
    assert!(drawn.iter().any(|text| text == STALE), "the reason is drawn: {drawn:?}");
    assert!(drawn.iter().all(|text| text != "적용"), "the apply button is not drawn: {drawn:?}");
}

#[test]
fn a_refused_apply_shows_the_error_and_keeps_the_document() {
    let mut host = Host::start();
    let canvas = canvas_with_text(&mut host, "그림판", "가");
    let proposal = propose(&mut host, &canvas, "나");
    let mut window = Window::open(&mut host, &canvas);
    feed_events(&mut host, &mut window);
    // The window does not learn of this change, so it still takes the row for current; the host refuses the stale proposal.
    change_box(&mut host, &canvas, "외부");
    let moved = content_of(&host.artifact(&canvas));
    window.step(&mut host, "proposal.apply", json!({ "proposal_id": proposal })).expect("the request is sent");
    assert_eq!(content_of(&host.artifact(&canvas)), moved, "the refused apply changes nothing");
    let message = window.app.notices.current(Instant::now()).map(|notice| notice.text.clone());
    assert!(message.is_some(), "the refusal is shown to the user");
}

#[test]
fn discarding_a_row_removes_it_from_the_list_and_changes_no_document() {
    let mut host = Host::start();
    let canvas = canvas_with_text(&mut host, "그림판", "가");
    let before = content_of(&host.artifact(&canvas));
    let proposal = propose(&mut host, &canvas, "나");
    let mut window = Window::open(&mut host, &canvas);
    feed_events(&mut host, &mut window);
    window.step(&mut host, "proposal.discard", json!({ "proposal_id": proposal })).expect("the row is discarded");
    assert!(window.app.listed_proposal_ids().is_empty(), "the row left the list");
    assert_eq!(content_of(&host.artifact(&canvas)), before, "the document did not change");
    let stored = host.call("proposal.get", json!({ "proposal_id": proposal })).expect("the proposal is still stored");
    assert_eq!(stored["proposal"]["status"], "validated", "the store keeps the proposal");
}

#[test]
fn a_command_on_a_row_the_open_document_does_not_list_is_refused() {
    let mut host = Host::start();
    let canvas = canvas_with_text(&mut host, "그림판", "가");
    let mut window = Window::open(&mut host, &canvas);
    for id in ["proposal.select", "proposal.discard", "proposal.apply"] {
        assert!(window.app.invoke(id, json!({ "proposal_id": "missing" })).is_err(), "{id} refuses a row it does not hold");
    }
}

#[test]
fn the_ai_request_carries_the_delivery_the_user_chose() {
    let mut host = Host::start();
    let canvas = canvas_with_text(&mut host, "그림판", "가");
    let mut window = Window::open(&mut host, &canvas);
    window.app.ai_model = "mock-model".into();
    window.step(&mut host, "ai.deliver_as", json!({ "deliver_as": "proposal" })).expect("the choice is made");
    window.step(&mut host, "ai.request", json!({ "prompt": "첫 상자를 고쳐 주세요" })).expect("the request is sent");
    let job: Job = window.app.job.clone().expect("the job is shown");
    assert_eq!(job.deliver_as, Some(DeliverAs::Proposal), "the request asks for a proposal");
    drop(window);

    let plain = canvas_with_text(&mut host, "그림판", "가");
    let mut window = Window::open(&mut host, &plain);
    window.app.ai_model = "mock-model".into();
    window.step(&mut host, "ai.request", json!({ "prompt": "첫 상자를 고쳐 주세요" })).expect("the request is sent");
    let job: Job = window.app.job.clone().expect("the job is shown");
    assert_eq!(job.deliver_as, None, "a request without the choice applies as before");
}

/// The figure's AI request carries the delivery choice too (`send_job_start`), as the canvas's does (backlog desktop 38).
#[test]
fn a_figure_ai_request_carries_the_delivery_the_user_chose() {
    let mut host = Host::start();
    let figure = host.create_figure();
    let mut window = Window::open(&mut host, &figure);
    window.app.ai_model = "mock-model".into();
    window.step(&mut host, "ai.deliver_as", json!({ "deliver_as": "proposal" })).expect("the choice is made");
    window.step(&mut host, "ai.request", json!({ "prompt": "범례를 정리해 주세요" })).expect("the request is sent");
    let start = window.calls.iter().find(|(method, _, _)| method == "job.start").expect("the figure's request starts a job");
    assert_eq!(start.1["deliver_as"], "proposal", "the figure's request asks for a proposal");
    let job: Job = window.app.job.clone().expect("the job is shown");
    assert_eq!(job.deliver_as, Some(DeliverAs::Proposal));
}

/// An apply the host refuses because the change touches what the job may not change (CONTENT_LOCKED) says so in the window's
/// words for that refusal, not that the change may have landed, and the row stays to be applied or discarded (backlog desktop 38).
#[test]
fn a_content_locked_apply_says_why_and_keeps_the_row() {
    let mut h = Harness::new();
    let _ = h.open_document("canvas", 1);
    h.app.record_proposal(&created("p1", "a1", 1));
    h.app.invoke("proposal.apply", json!({ "proposal_id": "p1" })).expect("the apply is sent");
    let requests = h.sent();
    let locked = DltError::new(ErrorCode::ContentLocked, "the change set touches a locked target");
    h.answer(&requests, "proposal.apply", Err(locked.clone()));
    let shown = h.message().map(|(_, text)| text);
    assert_eq!(shown, Some(super::user_error(&locked)), "the content-locked words");
    assert_ne!(shown.as_deref(), Some(APPLY_UNKNOWN));
    assert_eq!(h.app.listed_proposal_ids(), vec!["p1".to_owned()], "the row stays");
}

#[test]
fn the_request_choice_gives_the_same_job_by_the_command_and_by_the_tool() {
    let mut host = Host::start();
    let by_command_canvas = canvas_with_text(&mut host, "그림판", "가");
    let by_tool_canvas = canvas_with_text(&mut host, "그림판", "가");
    let mut window = Window::open(&mut host, &by_command_canvas);
    window.app.ai_model = "mock-model".into();
    window.step(&mut host, "ai.deliver_as", json!({ "deliver_as": "proposal" })).expect("the choice is made");
    window.step(&mut host, "ai.request", json!({ "prompt": "첫 상자를 고쳐 주세요" })).expect("the request is sent");
    let by_command: Job = window.app.job.clone().expect("the job the request started is shown");
    drop(window);

    let backend = HostBackend::new(host);
    let mcp = Mcp::connect(backend.clone());
    let revision = backend.with(|host| host.artifact(&by_tool_canvas).revision);
    let started = mcp.call(
        "job_start",
        json!({
            "request_id": new_id(),
            "artifact_id": by_tool_canvas,
            "base_revision": revision,
            "allowed_intents": ["content", "design"],
            "runtime": "claude",
            "model": "mock-model",
            "effort": "high",
            "prompt": "첫 상자를 고쳐 주세요",
            "target_ids": [],
            "deliver_as": "proposal",
        }),
    );
    let by_tool: Job = serde_json::from_value(started).expect("the job the tool started");
    assert_eq!(by_command.deliver_as, Some(DeliverAs::Proposal));
    assert_eq!(by_tool.deliver_as, by_command.deliver_as, "the command and the tool name the same delivery");
    assert_eq!(by_tool.allowed_intents, by_command.allowed_intents);
}

#[test]
fn a_row_applied_by_the_command_and_by_the_tool_leaves_the_same_canvas() {
    let mut host = Host::start();
    let by_command_canvas = canvas_with_text(&mut host, "그림판", "가");
    let by_tool_canvas = canvas_with_text(&mut host, "그림판", "가");
    let before = content_of(&host.artifact(&by_command_canvas));
    let by_command_proposal = propose(&mut host, &by_command_canvas, "나");
    let by_tool_proposal = propose(&mut host, &by_tool_canvas, "나");
    let mut window = Window::open(&mut host, &by_command_canvas);
    feed_events(&mut host, &mut window);
    window.step(&mut host, "proposal.apply", json!({ "proposal_id": by_command_proposal })).expect("the row applies");
    drop(window);

    let backend = HostBackend::new(host);
    let mcp = Mcp::connect(backend.clone());
    let applied = mcp.call("proposal_apply", json!({ "proposal_id": by_tool_proposal }));
    assert!(applied["revision"].as_u64().is_some(), "the tool applies: {applied}");
    let by_command = backend.with(|host| content_of(&host.artifact(&by_command_canvas)));
    let by_tool = backend.with(|host| content_of(&host.artifact(&by_tool_canvas)));
    assert_ne!(before, by_command, "the command changed its document");
    assert_eq!(by_command, by_tool, "the command and the tool left different content");
}

#[test]
fn applying_a_proposal_names_every_intent_it_may_change() {
    let spec = commands::find("proposal.apply").expect("the command is registered");
    assert_eq!(commands::ui_permission(spec), UiPermission::Change);
    let every: BTreeSet<Intent> = [Intent::Content, Intent::Design, Intent::Data].into_iter().collect();
    assert_eq!(commands::ui_intents(spec), every);
}

/// Answers the preview request of proposal `id` that a draw sent, and the read of its picture, so the picture is in the cache.
fn picture_arrives(h: &mut Harness, id: &str) {
    let requests = h.sent();
    h.answer(&requests, "proposal.preview", Ok(preview_of(id, "a1", 1)));
    let ticket = std::iter::from_fn(|| h.work.try_recv().ok())
        .find_map(|work| match work {
            Work::ReadAsset { ticket, .. } => Some(ticket),
            _ => None,
        })
        .expect("the preview's picture is read");
    let picture = RpcPayload::Image { mime: "image/png".into(), image: egui::ColorImage::from_rgba_unmultiplied([2, 2], &[255; 16]) };
    h.replies.send(RpcReply { ticket, result: Ok(picture) }).expect("the window listens");
    h.app.process_replies(&h.ctx);
}

/// A queued agent run of `command` that the window checks against `permission` before it runs the command.
fn agent_run(command: &str, args: Value, permission: UiPermission) -> UiRun {
    UiRun {
        run_id: new_id(),
        view_id: new_id(),
        expected: None,
        command_id: command.into(),
        args,
        actor: "test".into(),
        permission,
        allowed_intents: BTreeSet::new(),
        status: UiRunStatus::Queued,
        timeout_ms: 1_000,
        result: None,
        error: None,
    }
}

/// A `proposal.created` event of `artifact` for proposal `id`, made on `revision`.
fn created(id: &str, artifact: &str, revision: u64) -> Value {
    json!({ "event": "proposal.created", "artifact_id": artifact, "revision": revision,
        "detail": { "proposal_id": id, "status": "validated", "job_id": null } })
}

#[test]
fn every_row_is_listed_and_only_the_rows_in_view_ask_for_a_preview() {
    let mut h = Harness::new();
    theme::install(&h.ctx);
    let _ = h.open_document("canvas", 1);
    for n in 1..=12 {
        h.app.record_proposal(&created(&format!("p{n}"), "a1", 1));
    }
    let _ = h.sent();
    let drawn = drawn_text(&draw_rows(&mut h.app, &h.ctx));
    let asked: Vec<String> = h
        .sent()
        .iter()
        .filter(|request| request.1 == "proposal.preview")
        .map(|request| request.2["proposal_id"].as_str().unwrap_or_default().to_owned())
        .collect();
    assert!(!asked.is_empty() && asked.len() < 12, "the rows in view ask, the rows below the scroll area do not: {asked:?}");
    assert_eq!(asked[0], "p12", "the newest row is at the top and asks first");
    assert!(!asked.iter().any(|id| id == "p1"), "the oldest row is out of view");
    assert_eq!(h.app.listed_proposal_ids().len(), 12, "every row is listed");
    assert_eq!(drawn.iter().filter(|text| text.as_str() == "적용").count(), 12, "every row has its apply button: {drawn:?}");
    // The oldest row, out of view, applies like the others: its apply is sent.
    assert_eq!(h.app.invoke("proposal.apply", json!({ "proposal_id": "p1" })), Ok(()));
    assert!(h.sent().iter().any(|request| request.1 == "proposal.apply"), "the apply of the oldest row is sent");
}

#[test]
fn a_row_scrolled_into_view_asks_for_its_preview() {
    let mut h = Harness::new();
    theme::install(&h.ctx);
    h.ctx.all_styles_mut(|style| style.scroll_animation = egui::style::ScrollAnimation::none());
    let _ = h.open_document("canvas", 1);
    for n in 1..=12 {
        h.app.record_proposal(&created(&format!("p{n}"), "a1", 1));
    }
    let _ = h.sent();
    let _ = draw_rows(&mut h.app, &h.ctx);
    let asked = |h: &mut Harness| -> Vec<String> {
        h.sent()
            .iter()
            .filter(|request| request.1 == "proposal.preview")
            .map(|request| request.2["proposal_id"].as_str().unwrap_or_default().to_owned())
            .collect()
    };
    assert!(!asked(&mut h).iter().any(|id| id == "p1"), "the oldest row starts out of view");

    // The wheel scrolls the rows to their end, over a few frames of the window's clock.
    let over_rows = egui::pos2(theme::SIDEBAR_WIDTH / 2.0, 120.0);
    let mut later = Vec::new();
    for frame in 0..20 {
        let mut input = screen(1.0, 1000.0, 800.0);
        input.time = Some(f64::from(frame) * 0.1);
        input.events.push(egui::Event::PointerMoved(over_rows));
        if frame == 0 {
            input.events.push(egui::Event::MouseWheel {
                unit: egui::MouseWheelUnit::Point,
                delta: egui::vec2(0.0, -5000.0),
                modifiers: egui::Modifiers::NONE,
            });
        }
        let ctx = h.ctx.clone();
        let _ = ctx.run(input, |ctx| {
            egui::SidePanel::left("proposal-rows").exact_width(theme::SIDEBAR_WIDTH).show(ctx, |ui| {
                let palette = theme::palette(ctx);
                h.app.draw_proposals(ui, &palette);
            });
        });
        later.extend(asked(&mut h));
    }
    assert!(later.iter().any(|id| id == "p1"), "the oldest row asks once it is scrolled into view: {later:?}");
    assert_eq!(later.iter().filter(|id| id.as_str() == "p1").count(), 1, "and asks once");
}

#[test]
fn selecting_a_row_picks_it_and_a_row_of_another_document_is_refused() {
    let mut h = Harness::new();
    let _ = h.open_document("canvas", 1);
    h.app.record_proposal(&created("p1", "a1", 1));
    h.app.record_proposal(&created("elsewhere", "other", 1));
    assert_eq!(h.app.invoke("proposal.select", json!({ "proposal_id": "p1" })), Ok(()));
    assert_eq!(h.app.selected_proposal(), Some("p1"), "the row is picked");
    assert!(h.app.invoke("proposal.select", json!({ "proposal_id": "elsewhere" })).is_err(), "a row of another document is not this list's");
    assert_eq!(h.app.selected_proposal(), Some("p1"), "the pick stays");
    assert!(!h.sent().iter().any(|request| request.1.starts_with("proposal.")), "selecting asks the host nothing");
}

#[test]
fn a_discarded_row_takes_its_picture_out_of_the_cache() {
    let mut h = Harness::new();
    theme::install(&h.ctx);
    let _ = h.open_document("canvas", 1);
    h.app.record_proposal(&created("p1", "a1", 1));
    let _ = h.sent();
    let _ = draw_rows(&mut h.app, &h.ctx);
    picture_arrives(&mut h, "p1");
    assert!(thumbs::picture_of(&h.ctx, "proposal:p1").is_some(), "the row shows its picture");
    h.app.invoke("proposal.discard", json!({ "proposal_id": "p1" })).expect("the row is discarded");
    let _ = draw_rows(&mut h.app, &h.ctx);
    assert!(thumbs::picture_of(&h.ctx, "proposal:p1").is_none(), "the discarded row's picture is dropped");
}

#[test]
fn a_read_grade_run_is_refused_the_commands_that_change_the_request_or_the_list() {
    let mut host = Host::start();
    let canvas = canvas_with_text(&mut host, "그림판", "가");
    let window = Window::open(&mut host, &canvas);
    let row = json!({ "proposal_id": "p1" });
    // The refusal names the gate that holds these commands back (`ui_run_supported`), not a dialog or several steps they do
    // not have (backlog desktop 38).
    let unconfirmed = |refusal: Option<DltError>| refusal.map(|error| (error.code, error.message));
    let gate = Some((ErrorCode::UnsupportedOperation, super::ui_channel::UI_RUN_UNCONFIRMED.to_owned()));
    for (command, args) in [("ai.deliver_as", json!({ "deliver_as": "proposal" })), ("proposal.discard", row.clone()), ("proposal.apply", row.clone())] {
        assert_eq!(unconfirmed(window.app.ui_run_refusal(&agent_run(command, args.clone(), UiPermission::Read))), gate, "a read run is refused {command}");
        assert_eq!(unconfirmed(window.app.ui_run_refusal(&agent_run(command, args, UiPermission::Change))), gate, "an agent run does not run {command}");
    }
    assert!(window.app.ui_run_refusal(&agent_run("proposal.select", row, UiPermission::Read)).is_none(), "a read run may select a row");
}

#[test]
fn a_lost_apply_says_it_may_have_landed_and_a_refusal_says_why() {
    let mut h = Harness::new();
    let _ = h.open_document("canvas", 1);
    h.app.record_proposal(&created("p1", "a1", 1));
    h.app.invoke("proposal.apply", json!({ "proposal_id": "p1" })).expect("the apply is sent");
    let requests = h.sent();
    h.answer(&requests, "proposal.apply", Err(DltError::new(ErrorCode::Internal, "the answer was lost")));
    assert_eq!(h.message().map(|(_, text)| text), Some(APPLY_UNKNOWN.to_owned()), "a lost answer says the change may have landed");
    h.app.invoke("proposal.apply", json!({ "proposal_id": "p1" })).expect("the apply is sent again");
    let requests = h.sent();
    h.answer(&requests, "proposal.apply", Err(DltError::new(ErrorCode::RevisionConflict, "the document moved")));
    let refusal = h.message().map(|(_, text)| text);
    assert!(refusal.is_some() && refusal.as_deref() != Some(APPLY_UNKNOWN), "a refusal says why: {refusal:?}");
}

/// Regression (backlog desktop 38): an apply on its way was not a revision change for the image replacement and video checks
/// (`artifact_revision_change_pending`), so they could start against the revision the apply was about to move.
#[test]
fn an_apply_on_its_way_holds_the_changes_that_wait_for_the_revision() {
    let mut h = Harness::new();
    let _ = h.open_document("canvas", 1);
    h.app.record_proposal(&created("p1", "a1", 1));
    assert!(!h.app.artifact_revision_change_pending("a1"), "nothing is on its way yet");
    h.app.invoke("proposal.apply", json!({ "proposal_id": "p1" })).expect("the apply is sent");
    assert!(h.app.artifact_revision_change_pending("a1"), "the apply moves the revision");
    let requests = h.sent();
    h.answer(&requests, "proposal.apply", Err(DltError::new(ErrorCode::RevisionConflict, "the document moved")));
    assert!(!h.app.artifact_revision_change_pending("a1"), "the answered apply holds nothing");
}

#[test]
fn a_document_keeps_its_own_rows_and_the_other_documents_rows_stay() {
    let mut h = Harness::new();
    h.app.record_proposal(&created("elsewhere", "other", 1));
    for n in 0..65 {
        h.app.record_proposal(&created(&format!("p{n}"), "a1", 1));
    }
    assert_eq!(h.app.proposal_rows(), 65, "the document's oldest row made room; the other document's row stayed");
}
