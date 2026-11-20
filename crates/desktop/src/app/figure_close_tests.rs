//! Figure work a window close or a document switch would leave behind (figure_close.rs): what the close and the switch
//! say about it, that nothing is dropped before the user's answer, what the answer drops, and which job replies still
//! count. The window is played by the test with detached RPC, and egui runs headless (no window opens).

use std::{
    path::PathBuf,
    sync::mpsc::{Receiver, Sender},
    time::{Duration, Instant},
};

use designllm_contracts::{new_id, ArtifactKind, ArtifactSummary, ContextSnapshot, DltError, ErrorCode, Evidence, Job, JobStatus};
use eframe::egui::{Context, RawInput, ViewportCommand, ViewportEvent, ViewportId};
use serde_json::{json, Value};

use super::{
    drafts::{drafts_path, DraftBook},
    figure_close::{AWAITED_REPLY_LIMIT, LATE_JOB_UNREADABLE, LATE_START_UNKNOWN, LATE_START_WAIT},
    figure_flow_tests::Host,
    CsvImportDraft, DatasetCellEdit, DesktopApp, HeldPdfEvidence, PdfEvidenceTarget, PendingAction, Screen,
};
use crate::{
    notice::Severity,
    rpc::{RpcPayload, RpcReply, RpcWorker, Work},
};

const FIGURE: &str = "figure-1";
const OTHER: &str = "figure-2";
const PAGE: u32 = 11;

type Request = (u64, String, Value);

struct Harness {
    app: DesktopApp,
    replies: Sender<RpcReply>,
    work: Receiver<Work>,
    ctx: Context,
}

impl Harness {
    /// One open Figure at revision 3, with a model and a prompt so that a request can start. Both figures are known to the
    /// workspace, as they are in the window.
    fn new() -> Self {
        let (rpc, replies, work) = RpcWorker::detached();
        let mut app = DesktopApp::new(rpc, "그림 작업".into(), None, None);
        while work.try_recv().is_ok() {}
        app.screen = Screen::Editor;
        app.active = Some(figure_summary(FIGURE, 3));
        app.artifacts = vec![figure_summary(FIGURE, 3), figure_summary(OTHER, 1)];
        app.ai_prompt = "곡선을 추가하세요".into();
        app.ai_model = "mock-model".into();
        app.ai_runtime = "mock".into();
        app.ai_effort = "high".into();
        app.context = Some(context(FIGURE, 3, &app.view_id));
        Self { app, replies, work, ctx: Context::default() }
    }

    fn sent(&mut self) -> Vec<Request> {
        let mut batch = Vec::new();
        while let Ok(work) = self.work.try_recv() {
            if let Work::Call { ticket, method, params } = work {
                batch.push((ticket, method, params));
            }
        }
        batch
    }

    /// Answers the newest request for `method` in `requests` and lets the window handle the reply.
    fn answer(&mut self, requests: &[Request], method: &str, result: Result<Value, DltError>) {
        let ticket = requests
            .iter()
            .rev()
            .find(|request| request.1 == method)
            .map(|request| request.0)
            .unwrap_or_else(|| panic!("the window sent no {method} request"));
        self.replies.send(RpcReply { ticket, result: result.map(RpcPayload::Json) }).expect("the window still listens");
        self.app.process_replies(&self.ctx);
    }

    /// Answers the request with `ticket`, which need not be the newest one for its method.
    fn answer_ticket(&mut self, ticket: u64, result: Result<Value, DltError>) {
        self.replies.send(RpcReply { ticket, result: result.map(RpcPayload::Json) }).expect("the window still listens");
        self.app.process_replies(&self.ctx);
    }

    /// Adds one evidence file to the open Figure's list.
    fn attach(&mut self, file: &TempFile) {
        self.app.add_figure_ai_attachment(file.path.clone()).expect("the file joins the list");
    }

    /// Starts an AI request on the open Figure with the typed prompt, as the AI tab does.
    fn request(&mut self) {
        self.app.invoke_ui("ai.request", json!({ "prompt": "곡선을 추가하세요" }));
    }

    /// The notice the window shows now, with its severity.
    fn notice(&self) -> Option<(Severity, String)> {
        self.app.notices.current(Instant::now()).map(|notice| (notice.severity, notice.text.clone()))
    }

    /// Starts an evidence request, answers its file import, and returns the unanswered job start.
    fn start_request_with_a_file(&mut self) -> Vec<Request> {
        let paper = TempFile::new("paper.pdf");
        self.attach(&paper);
        self.request();
        let import = self.sent();
        self.answer(&import, "asset.import", Ok(asset_json()));
        let start = self.sent();
        assert_eq!(methods(&start), ["job.start"], "every file is imported, so the job start goes out");
        start
    }
}

/// A file on disk for the list. Its folder is removed when the test ends.
struct TempFile {
    folder: PathBuf,
    path: PathBuf,
}

impl TempFile {
    fn new(name: &str) -> Self {
        let folder = std::env::temp_dir().join(format!("designllm-figure-close-{}", new_id()));
        std::fs::create_dir_all(&folder).expect("a folder for the evidence");
        let path = folder.join(name);
        std::fs::write(&path, b"%PDF-1.4").expect("the evidence file is written");
        Self { folder, path }
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.folder);
    }
}

fn figure_summary(id: &str, revision: u64) -> ArtifactSummary {
    ArtifactSummary {
        id: id.into(),
        kind: ArtifactKind::Figure,
        title: "그림".into(),
        revision,
        engine_id: "figure".into(),
        dirty: false,
        modified_at_ms: None,
        active_jobs: Vec::new(),
    }
}

fn record() -> Evidence {
    Evidence {
        id: "pdf-p11-l000".into(),
        source_hash: "a".repeat(64),
        origin: "paper_evidence".into(),
        physical_page_index: Some(PAGE),
        region: None,
        pdf_geometry: None,
        text: "A activates B".into(),
        review_state: "unreviewed".into(),
    }
}

/// A held set of one record. `ticket` is set while its commit is on its way.
fn held(ticket: Option<u64>) -> HeldPdfEvidence {
    HeldPdfEvidence {
        id: new_id(),
        target: PdfEvidenceTarget {
            artifact_id: FIGURE.into(),
            revision: 3,
            page_index: PAGE,
            file_name: "paper.pdf".into(),
            asset_sha256: Some("a".repeat(64)),
            awaiting: None,
        },
        records: vec![record()],
        message: String::new(),
        ticket,
    }
}

fn context(artifact_id: &str, revision: u64, view_id: &str) -> ContextSnapshot {
    serde_json::from_value(json!({
        "workspace_id": "w1",
        "artifact_id": artifact_id,
        "revision": revision,
        "view_id": view_id,
        "selection": { "node_ids": [], "text_range": null },
        "visible_page": 0,
        "selected_properties": { "nodes": null, "stale_selection": null, "render_unavailable": null },
        "capabilities": {},
        "render_snapshot": null,
        "view_transform": null,
    }))
    .expect("a context snapshot")
}

/// A job as the host reports it, for the document `artifact_id`.
fn job_json(id: &str, artifact_id: &str, status: &str) -> Value {
    json!({
        "id": id,
        "attempt_id": "attempt-1",
        "artifact_id": artifact_id,
        "base_revision": 3,
        "status": status,
        "allowed_intents": ["content"],
        "model": "mock-model",
        "effort": "high",
        "error": null,
    })
}

fn running_job_of(artifact_id: &str) -> Job {
    serde_json::from_value(job_json("job-1", artifact_id, "running")).expect("a running job")
}

fn asset_json() -> Value {
    json!({ "sha256": "a".repeat(64), "mime": "application/pdf", "bytes": 8 })
}

fn methods(requests: &[Request]) -> Vec<&str> {
    requests.iter().map(|request| request.1.as_str()).collect()
}

fn close_input() -> RawInput {
    let mut input = RawInput::default();
    input.viewports.get_mut(&ViewportId::ROOT).expect("root viewport").events.push(ViewportEvent::Close);
    input
}

fn has_command(output: &eframe::egui::FullOutput, expected: ViewportCommand) -> bool {
    output.viewport_output.get(&ViewportId::ROOT).is_some_and(|viewport| viewport.commands.contains(&expected))
}

fn active_id(harness: &Harness) -> Option<&str> {
    harness.app.active.as_ref().map(|active| active.id.as_str())
}

/// A time past the window's wait for an answer: a start sent then has waited longer than the window waits for it.
fn overdue() -> Instant {
    Instant::now().checked_sub(AWAITED_REPLY_LIMIT + Duration::from_secs(1)).expect("the clock is more than two and a half minutes old")
}

/// The request id and the request of the newest `job.start` in `requests`.
fn start_of(requests: &[Request]) -> (&Value, &str) {
    let start = requests.iter().rev().find(|request| request.1 == "job.start").expect("the window sent a job start");
    (&start.2, start.2["request_id"].as_str().expect("the start has a request id"))
}

#[test]
fn a_window_with_no_figure_work_closes_without_a_question() {
    let mut harness = Harness::new();
    assert!(harness.app.figure_close_warning().is_none());
    let ctx = harness.ctx.clone();
    let close = ctx.run(close_input(), |ctx| harness.app.guard_held_edit_close(ctx));
    assert!(!has_command(&close, ViewportCommand::CancelClose), "nothing holds the close");
    assert!(harness.app.held_edit_close_warning().is_none());
}

#[test]
fn closing_asks_before_it_drops_held_pdf_records() {
    let mut harness = Harness::new();
    harness.app.held_pdf_evidence.push(held(None));
    let ctx = harness.ctx.clone();
    let close = ctx.run(close_input(), |ctx| harness.app.guard_held_edit_close(ctx));
    assert!(has_command(&close, ViewportCommand::CancelClose), "the close waits for the answer");
    assert!(harness.app.close_confirmation_open);
    assert_eq!(harness.app.held_pdf_evidence.len(), 1, "nothing is dropped before the answer");
    let warning = harness.app.held_edit_close_warning().expect("the close warning");
    assert!(warning.contains("보류한 PDF 근거 1개"), "the warning names the held records: {warning}");
    assert!(warning.contains("창을 닫으면 보류한 PDF 근거와 보내지 않은 근거 자료는 버려집니다."), "the warning says what closing drops: {warning}");
}

#[test]
fn closing_names_unsent_files_and_a_running_job_and_says_the_job_is_not_cancelled() {
    let mut harness = Harness::new();
    let paper = TempFile::new("paper.pdf");
    harness.attach(&paper);
    harness.app.job = Some(running_job_of(FIGURE));
    assert_eq!(harness.app.figure_work(true).jobs_running, 1, "the job of the open figure runs");
    let warning = harness.app.figure_close_warning().expect("figure work is held");
    assert!(warning.contains("보내지 않은 근거 자료 1개"), "{warning}");
    assert!(warning.contains("실행 중인 그림 AI 작업 1개"), "{warning}");
    assert!(warning.contains("실행 중인 AI 작업은 취소하지 않습니다."), "{warning}");
    assert!(!harness.app.figure_only_job_remains(), "files are waiting too");
    assert_eq!(harness.app.figure_ai_files().len(), 1, "the file stays until the answer");
}

#[test]
fn a_running_figure_job_alone_closes_with_the_plain_close_button() {
    let mut harness = Harness::new();
    harness.app.job = Some(running_job_of(FIGURE));
    let warning = harness.app.figure_close_warning().expect("the job is running");
    assert!(!warning.contains("버려집니다"), "a job is not discarded by closing: {warning}");
    assert!(harness.app.figure_only_job_remains(), "the close button then only closes");
}

#[test]
fn a_running_figure_job_alone_closes_once_the_user_chooses_to_close() {
    // Regression: the accepted close was cancelled again on the next frame while the job still ran.
    let mut harness = Harness::new();
    harness.app.job = Some(running_job_of(FIGURE));
    let ctx = harness.ctx.clone();
    let asked = ctx.run(close_input(), |ctx| harness.app.guard_held_edit_close(ctx));
    assert!(has_command(&asked, ViewportCommand::CancelClose), "the close is asked about first");
    assert!(harness.app.close_confirmation_open);

    // 닫기 is chosen: the window closes, and the job is not cancelled.
    let closed = ctx.run(RawInput::default(), |ctx| harness.app.discard_held_edits_and_close(ctx));
    assert!(has_command(&closed, ViewportCommand::Close), "the answer closes the window");

    // The host reports the close request again on the next frame: the accepted close goes through.
    let again = ctx.run(close_input(), |ctx| harness.app.guard_held_edit_close(ctx));
    assert!(!has_command(&again, ViewportCommand::CancelClose), "the job does not hold the window open again");
    assert!(!harness.app.close_confirmation_open);
    assert!(harness.app.job.is_some(), "the job stays known to the window");
    assert!(harness.sent().iter().all(|request| request.1 != "job.cancel"), "closing cancels no job");
}

#[test]
fn discarding_on_close_drops_held_records_and_files_and_closes() {
    let mut harness = Harness::new();
    let paper = TempFile::new("paper.pdf");
    harness.attach(&paper);
    harness.app.held_pdf_evidence.push(held(None));
    harness.app.held_pdf_evidence.push(held(Some(7)));
    let ctx = harness.ctx.clone();
    let close = ctx.run(close_input(), |ctx| harness.app.guard_held_edit_close(ctx));
    assert!(has_command(&close, ViewportCommand::CancelClose));
    let closed = ctx.run(RawInput::default(), |ctx| harness.app.discard_held_edits_and_close(ctx));
    assert!(has_command(&closed, ViewportCommand::Close), "the answer closes the window");
    assert!(harness.app.held_pdf_evidence.is_empty());
    assert!(harness.app.figure_ai_attachments.is_none(), "the files not sent are dropped with the answer");
    assert!(harness.app.close_outcome_uncertain, "a commit on its way may already have landed");
    assert!(harness.sent().iter().all(|request| request.1 != "changes.apply"), "closing sends no change");
}

/// A data value sent and not answered when the window closes may already be in the figure: the close is asked about, and the
/// answer to close says that a change sent before may have landed (`close_outcome_uncertain`, backlog desktop 19).
#[test]
fn closing_while_a_data_value_is_on_its_way_says_it_may_have_landed() {
    let mut harness = Harness::new();
    harness.app.call("figure.dataset.edit", json!({}), PendingAction::DatasetEdit).expect("the value is sent");
    let ctx = harness.ctx.clone();
    let close = ctx.run(close_input(), |ctx| harness.app.guard_held_edit_close(ctx));
    assert!(has_command(&close, ViewportCommand::CancelClose), "the close is asked about first");
    assert!(!harness.app.close_outcome_uncertain, "nothing is judged before the answer");
    let closed = ctx.run(RawInput::default(), |ctx| harness.app.discard_held_edits_and_close(ctx));
    assert!(has_command(&closed, ViewportCommand::Close), "the answer closes the window");
    assert!(harness.app.close_outcome_uncertain, "the value on its way may already be in the figure");
}

#[test]
fn switching_documents_waits_and_its_answer_keeps_a_commit_on_its_way() {
    let mut harness = Harness::new();
    let paper = TempFile::new("paper.pdf");
    harness.attach(&paper);
    harness.app.held_pdf_evidence.push(held(None));
    harness.app.held_pdf_evidence.push(held(Some(7)));

    harness.app.activate(figure_summary(OTHER, 1));
    assert_eq!(active_id(&harness), Some(FIGURE), "the switch waits for the answer");
    assert_eq!(harness.app.held_pdf_evidence.len(), 2, "nothing is dropped before the answer");
    let warning = harness.app.figure_switch_warning().expect("figure work is held");
    assert!(warning.contains("작업물을 바꾸면 보류한 PDF 근거와 보내지 않은 근거 자료는 버려집니다."), "{warning}");
    assert!(warning.contains("전송 중인 PDF 근거 1개"), "{warning}");
    assert!(warning.contains("이미 작업물에 반영됐을 수 있으며"), "{warning}");

    // The answer 전환: the records not on their way and the files go; the commit on its way stays until the host answers.
    harness.app.answer_pending_switch(true);
    assert_eq!(active_id(&harness), Some(OTHER));
    assert_eq!(harness.app.held_pdf_evidence.len(), 1);
    assert_eq!(harness.app.held_pdf_evidence[0].ticket, Some(7));
    assert!(harness.app.figure_ai_attachments.is_none());
}

#[test]
fn keeping_working_keeps_the_document_and_everything_held() {
    let mut harness = Harness::new();
    harness.app.held_pdf_evidence.push(held(None));
    harness.app.activate(figure_summary(OTHER, 1));
    assert!(harness.app.pending_switch.is_some());
    harness.app.answer_pending_switch(false);
    assert!(harness.app.pending_switch.is_none());
    assert_eq!(active_id(&harness), Some(FIGURE));
    assert_eq!(harness.app.held_pdf_evidence.len(), 1, "nothing is dropped by 계속 작업");
}

#[test]
fn a_switch_whose_question_is_gone_is_not_made() {
    let mut harness = Harness::new();
    harness.app.pending_switch = Some(figure_summary(OTHER, 1));
    let ctx = harness.ctx.clone();
    let _ = ctx.run(RawInput::default(), |ctx| harness.app.draw_figure_switch_dialog(ctx));
    assert!(harness.app.pending_switch.is_none(), "nothing held any more, so the question is dropped");
    assert_eq!(active_id(&harness), Some(FIGURE));
}

#[test]
fn the_unknown_result_warning_survives_the_switch_to_another_document() {
    // Regression: `show_artifact` cleared the notice that the switch had just set.
    let mut harness = Harness::new();
    let _start = harness.start_request_with_a_file();

    harness.app.activate(figure_summary(OTHER, 1));
    assert!(harness.app.pending_switch.is_some(), "the job start that is unanswered is named before the switch");
    harness.app.answer_pending_switch(true);
    assert_eq!(active_id(&harness), Some(OTHER));
    let (severity, text) = harness.notice().expect("the window says the job may have started");
    assert_eq!(severity, Severity::Warning);
    assert!(text.contains("시작됐을 수 있으니"), "the notice says the job may have started: {text}");
    assert!(harness.app.figure_ai_request.is_none());
}

#[test]
fn a_job_start_answered_after_the_window_came_back_is_recorded_as_the_job_of_the_figure() {
    // Regression: the job of a request the window had left was dropped, so a job running for figure-1 stayed hidden.
    let mut harness = Harness::new();
    let start = harness.start_request_with_a_file();
    harness.app.activate(figure_summary(OTHER, 1));
    harness.app.answer_pending_switch(true);
    harness.app.activate(figure_summary(FIGURE, 3));
    assert_eq!(active_id(&harness), Some(FIGURE), "figure-1 is shown again");

    harness.answer(&start, "job.start", Ok(job_json("job-1", FIGURE, "queued")));
    assert_eq!(harness.app.job.as_ref().map(|job| job.id.as_str()), Some("job-1"), "the job runs for figure-1, so it is shown");
    assert!(harness.app.parked_jobs.is_empty());
    assert!(harness.app.figure_ai_request.is_none());
    let (severity, text) = harness.notice().expect("the window says a job started");
    assert_eq!(severity, Severity::Warning);
    assert!(text.contains("실행 상태를 확인"), "{text}");
}

#[test]
fn an_unreadable_answer_of_a_start_the_window_left_warns_that_the_job_may_run() {
    let mut harness = Harness::new();
    let start = harness.start_request_with_a_file();
    harness.app.activate(figure_summary(OTHER, 1));
    harness.app.answer_pending_switch(true);
    harness.app.activate(figure_summary(FIGURE, 3));

    harness.answer(&start, "job.start", Ok(json!({ "id": 7 })));
    assert!(harness.app.job.is_none(), "nothing readable is recorded as a job");
    assert!(harness.app.parked_jobs.is_empty());
    assert_eq!(harness.notice(), Some((Severity::Warning, LATE_JOB_UNREADABLE.to_owned())), "the user is told to check the job list");
}

#[test]
fn a_late_status_of_the_same_attempt_does_not_take_back_a_newer_one() {
    let mut harness = Harness::new();
    harness.app.record_started_job(job_of("job-1", FIGURE, "running"));
    harness.app.update_job(job_of("job-1", FIGURE, "completed"));
    harness.app.update_job(job_of("job-1", FIGURE, "running"));
    assert_eq!(harness.app.job.as_ref().map(|job| job.status), Some(JobStatus::Completed), "a late running answer leaves the end in place");
    harness.app.update_job(job_of("job-1", FIGURE, "queued"));
    assert_eq!(harness.app.job.as_ref().map(|job| job.status), Some(JobStatus::Completed));

    // A resumed job runs a new attempt, so its status is taken.
    let mut resumed = job_of("job-1", FIGURE, "queued");
    resumed.attempt_id = "attempt-2".into();
    harness.app.update_job(resumed);
    assert_eq!(harness.app.job.as_ref().map(|job| job.status), Some(JobStatus::Queued));

    // A job kept aside keeps a running status over a late queued one.
    let mut harness = Harness::new();
    harness.app.record_started_job(job_of("job-1", FIGURE, "running"));
    harness.app.record_started_job(job_of("job-2", FIGURE, "running"));
    assert_eq!(harness.app.parked_jobs.len(), 1, "the older job is kept aside");
    harness.app.update_job(job_of("job-1", FIGURE, "queued"));
    assert_eq!(harness.app.parked_jobs[0].status, JobStatus::Running);
}

/// The job `job-1` of the figure in its attempt `number` (the attempt's id and its number, as the host reports both).
fn attempt_of(status: &str, number: u64) -> Job {
    let mut value = job_json("job-1", FIGURE, status);
    value["attempt_id"] = json!(format!("attempt-{number}"));
    value["attempt_seq"] = json!(number);
    serde_json::from_value(value).expect("a job")
}

/// Regression (48b, SPEC.md:72): after `job.resume` a late answer of the attempt the resume replaced took the place of the new
/// attempt, since two attempt ids say nothing of their order. The attempt number does, for the shown job, for a job kept aside
/// and for a late start answered from its receipt.
#[test]
fn a_late_answer_of_an_attempt_the_resume_replaced_does_not_take_the_place_of_the_new_attempt() {
    let shown = |harness: &Harness| harness.app.job.as_ref().map(|job| (job.attempt_id.clone(), job.status));
    let mut harness = Harness::new();
    harness.app.record_started_job(attempt_of("running", 1));
    harness.app.update_job(attempt_of("queued", 2));
    harness.app.update_job(attempt_of("failed", 1));
    assert_eq!(shown(&harness), Some(("attempt-2".to_owned(), JobStatus::Queued)), "a late status of the replaced attempt is ignored");
    harness.app.record_started_job(attempt_of("queued", 1));
    assert_eq!(shown(&harness), Some(("attempt-2".to_owned(), JobStatus::Queued)), "a late start of the replaced attempt is ignored");
    harness.app.update_job(attempt_of("running", 2));
    assert_eq!(shown(&harness), Some(("attempt-2".to_owned(), JobStatus::Running)), "the new attempt's own status is taken");

    // A job kept aside keeps its new attempt too, and goes on being polled.
    let mut harness = Harness::new();
    harness.app.record_started_job(attempt_of("running", 1));
    harness.app.record_started_job(job_of("job-2", FIGURE, "running"));
    harness.app.update_job(attempt_of("running", 2));
    harness.app.update_job(attempt_of("failed", 1));
    let parked: Vec<(&str, JobStatus)> = harness.app.parked_jobs.iter().map(|job| (job.attempt_id.as_str(), job.status)).collect();
    assert_eq!(parked, [("attempt-2", JobStatus::Running)], "the job kept aside is not dropped by the replaced attempt's end");
}

/// 48b review (MAJOR 4): a `job.start` receipt a build before `attempt_seq` stored has no number; it is the first attempt, so
/// it does not take the place of a later one either.
#[test]
fn a_start_receipt_without_an_attempt_number_counts_as_the_first_attempt() {
    let shown = |harness: &Harness| harness.app.job.as_ref().map(|job| (job.attempt_id.clone(), job.status));
    let mut harness = Harness::new();
    harness.app.record_started_job(attempt_of("queued", 2));
    harness.app.record_started_job(job_of("job-1", FIGURE, "queued"));
    assert_eq!(shown(&harness), Some(("attempt-2".to_owned(), JobStatus::Queued)), "a start answered from an old receipt is ignored");
    harness.app.update_job(job_of("job-1", FIGURE, "failed"));
    assert_eq!(shown(&harness), Some(("attempt-2".to_owned(), JobStatus::Queued)), "so is a status without a number");
    // Two attempts with one number cannot both be right; the one the window holds stays.
    let mut other = attempt_of("failed", 2);
    other.attempt_id = "attempt-x".into();
    harness.app.update_job(other);
    assert_eq!(shown(&harness), Some(("attempt-2".to_owned(), JobStatus::Queued)), "another attempt with the same number is ignored");
}

/// 48b review (MAJOR 5): a late `job.start` answer of the attempt the window holds does not take its status back to queued.
#[test]
fn a_late_start_answer_of_the_same_attempt_does_not_take_back_its_status() {
    let mut harness = Harness::new();
    harness.app.record_started_job(attempt_of("running", 1));
    harness.app.update_job(attempt_of("completed", 1));
    harness.app.record_started_job(attempt_of("queued", 1));
    assert_eq!(harness.app.job.as_ref().map(|job| job.status), Some(JobStatus::Completed), "the start's queued status is older");
}

#[test]
fn a_job_start_answered_while_another_figure_is_open_is_kept_aside_and_shown_on_return() {
    // Regression: the answer of a start that was answered while the window showed another document was dropped unseen.
    let mut harness = Harness::new();
    let ticket = harness.app.call("job.start", json!({ "artifact_id": FIGURE }), PendingAction::StartJob).expect("the start is sent");
    harness.app.active = Some(figure_summary(OTHER, 1));
    harness.answer_ticket(ticket, Ok(job_json("job-1", FIGURE, "queued")));
    assert!(harness.app.job.is_none(), "figure-2 shows no job of figure-1");
    assert_eq!(harness.app.parked_jobs.len(), 1, "the job of figure-1 is kept aside");
    assert_eq!(harness.app.figure_work(true).jobs_running, 1, "a close counts it");

    harness.app.activate(figure_summary(FIGURE, 3));
    assert_eq!(harness.app.job.as_ref().map(|job| job.id.as_str()), Some("job-1"), "figure-1's job is shown when figure-1 is opened");
    assert!(harness.app.parked_jobs.is_empty());
}

#[test]
fn a_refused_job_start_answered_after_the_window_came_back_shows_no_error() {
    let mut harness = Harness::new();
    let start = harness.start_request_with_a_file();
    harness.app.activate(figure_summary(OTHER, 1));
    harness.app.answer_pending_switch(true);
    harness.app.activate(figure_summary(FIGURE, 3));

    harness.answer(&start, "job.start", Err(DltError::invalid("the job was refused")));
    assert!(harness.app.job.is_none());
    assert!(harness.app.figure_ai_request.is_none());
    assert!(harness.notice().is_none_or(|(severity, _)| severity != Severity::Error), "the refusal of an earlier request is not reported as an error");
}

#[test]
fn a_start_left_unanswered_on_another_document_holds_the_figures_next_request_until_its_answer() {
    // The window stops waiting for a start when it leaves the figure (`release_waits_of_left_documents`), but its answer is still
    // due: the figure's next request waits for it, the warning says so again on return, and the job the answer records holds on.
    let mut harness = Harness::new();
    let stale = harness.start_request_with_a_file();
    harness.app.activate(figure_summary(OTHER, 1));
    harness.app.answer_pending_switch(true);
    harness.app.activate(figure_summary(FIGURE, 3));
    // Showing a document clears the context until its read comes back; the read answer is supplied here.
    harness.app.context = Some(context(FIGURE, 3, &harness.app.view_id));
    let (severity, text) = harness.notice().expect("the window says the answer is still due");
    assert_eq!((severity, text.as_str()), (Severity::Warning, LATE_START_WAIT));
    assert_eq!(harness.app.figure_job_wait(), Some(LATE_START_WAIT), "the answer is still due, so the figure waits");
    assert!(
        harness.app.figure_close_warning().is_some_and(|warning| warning.contains("시작 응답을 기다리는 그림 AI 작업 1개")),
        "a close counts the unanswered start"
    );
    let _ = harness.sent();
    assert_eq!(harness.app.invoke("ai.request", json!({ "prompt": "곡선을 추가하세요" })), Err(LATE_START_WAIT.into()));
    assert!(harness.sent().is_empty(), "no import and no job start while the answer is due");

    harness.answer(&stale, "job.start", Ok(job_json("job-1", FIGURE, "queued")));
    assert_eq!(harness.app.job.as_ref().map(|job| job.id.as_str()), Some("job-1"), "the job of the figure is shown");
    let reason = harness.app.figure_job_wait().expect("the running job holds the figure");
    assert!(!harness.app.can_start_ai_job(), "the send control is off while the job runs");
    assert_eq!(harness.app.invoke("ai.request", json!({ "prompt": "곡선을 추가하세요" })), Err(reason.into()));
    assert!(harness.sent().is_empty(), "no import and no job start while job-1 runs");
}

#[test]
fn a_start_not_answered_in_time_holds_the_figure_until_its_answer_and_its_late_answer_is_recorded() {
    // Regression: a start whose answer never came (a lost answer, a stopped worker) held the figure's requests back for good.
    // The window stops waiting for it, but the host may still run its job: the figure waits for the answer and a close counts it.
    let mut harness = Harness::new();
    let ticket = harness.app.call("job.start", json!({ "artifact_id": FIGURE }), PendingAction::StartJob).expect("the start is sent");
    assert!(harness.app.figure_job_wait().is_some(), "the start is waited for");

    harness.app.awaited_starts.insert(ticket, overdue());
    harness.app.expire_awaited_starts();
    assert_eq!(harness.app.figure_job_wait(), Some(LATE_START_WAIT), "the answer is still due, so the figure waits");
    assert!(
        harness.app.figure_close_warning().is_some_and(|warning| warning.contains("시작 응답을 기다리는 그림 AI 작업 1개")),
        "a close counts it"
    );
    let (severity, text) = harness.notice().expect("the window says the start timed out");
    assert_eq!(severity, Severity::Warning);
    assert!(text.contains("작업 목록을 확인"), "{text}");

    harness.answer_ticket(ticket, Ok(job_json("job-1", FIGURE, "queued")));
    assert_eq!(harness.app.job.as_ref().map(|job| job.id.as_str()), Some("job-1"), "the late answer is still recorded");
    assert!(harness.app.figure_job_wait().is_some(), "the job it records runs");
}

#[test]
fn a_cancelled_evidence_import_that_is_not_answered_in_time_shows_no_error() {
    // Regression: a cancelled import that was not answered in time showed "not sent" as an error after the cancel.
    let mut harness = Harness::new();
    let paper = TempFile::new("paper.pdf");
    harness.attach(&paper);
    harness.request();
    let import = harness.sent();
    let ticket = import.iter().find(|request| request.1 == "asset.import").expect("the import is on its way").0;
    harness.app.cancel_figure_ai_request().expect("the import can be cancelled");

    harness.app.awaited_starts.insert(ticket, overdue());
    harness.app.expire_awaited_starts();
    assert!(harness.notice().is_none_or(|(severity, _)| severity != Severity::Error), "the cancelled request shows no error");
}

#[test]
fn a_repeat_of_a_start_whose_answer_was_lost_reuses_its_request_id() {
    // The connection was lost after the start was written: the host may have made the job. The repeat carries the same id, and
    // the host answers it from its receipt, so one job results.
    let mut harness = Harness::new();
    harness.request();
    let first = harness.sent();
    let (_, first_id) = start_of(&first);
    let first_id = first_id.to_owned();
    harness.answer(&first, "job.start", Err(DltError::new(ErrorCode::HostNotRunning, "the connection was lost")));

    harness.request();
    let second = harness.sent();
    assert_eq!(start_of(&second).1, first_id, "the repeat carries the same request id");
    harness.answer(&second, "job.start", Ok(job_json("job-1", FIGURE, "queued")));
    assert_eq!(harness.app.job.as_ref().map(|job| job.id.as_str()), Some("job-1"));
}

#[test]
fn an_exact_figure_repeat_after_a_crash_uses_the_imported_asset_even_if_the_source_is_gone() {
    let workspace = TempFile::new("workspace.txt");
    let path = drafts_path(&workspace.folder);
    let mut first = Harness::new();
    first.app.drafts = DraftBook::open(path.clone());
    // The helper drops its temporary attachment after the original import and start.
    let original = first.start_request_with_a_file();
    first.app.drafts.end_session_as_crash();
    first.app.drafts = DraftBook::default();

    let mut next = Harness::new();
    next.app.ai_prompt.clear();
    next.app.drafts = DraftBook::open(path);
    let draft = next.app.drafts.recovered().pop().expect("the marked figure request");
    next.app.restore_draft(&draft.key);
    assert_eq!(next.app.figure_ai_files().len(), 1);
    assert!(!next.app.figure_ai_files()[0].path.exists());
    next.request();
    let repeat = next.sent();
    assert_eq!(methods(&repeat), ["job.start"], "the imported asset does not need another asset.import");
    assert_eq!(start_of(&repeat), start_of(&original), "the id and every parameter match");
}

#[test]
fn a_start_refused_by_the_host_or_repeated_with_other_content_gets_a_new_request_id() {
    // A refusal is the host's receipt for its id, so a repeat of it would be refused again. Another prompt is another request.
    let mut harness = Harness::new();
    harness.request();
    let first = harness.sent();
    let first_id = start_of(&first).1.to_owned();
    harness.answer(&first, "job.start", Err(DltError::new(ErrorCode::HostNotRunning, "the connection was lost")));

    harness.app.ai_prompt = "범례를 고치세요".into();
    harness.app.invoke_ui("ai.request", json!({ "prompt": "범례를 고치세요" }));
    let second = harness.sent();
    let second_id = start_of(&second).1.to_owned();
    assert_ne!(second_id, first_id, "other content is a new request");
    harness.answer(&second, "job.start", Err(DltError::invalid("the job was refused")));

    harness.request();
    let third = harness.sent();
    assert_ne!(start_of(&third).1, second_id, "a refused start is not repeated with its id");
}

#[test]
fn an_answer_to_a_request_the_window_no_longer_holds_keeps_the_notice_of_its_method() {
    // Regression: the answer of a discarded import cleared an error of the same method on screen before it was checked as stale.
    let mut harness = Harness::new();
    let paper = TempFile::new("paper.pdf");
    harness.attach(&paper);
    harness.request();
    let import = harness.sent();
    harness.app.discard_figure_work();
    harness.app.notices.error("실패", Some("asset.import"), Instant::now());
    harness.answer(&import, "asset.import", Ok(asset_json()));
    assert!(
        harness.notice().is_some_and(|(severity, text)| severity == Severity::Error && text == "실패"),
        "the answer to a discarded request leaves the error on screen"
    );
    assert!(harness.sent().iter().all(|request| request.1 != "job.start"), "the discarded import starts no job");
}

#[test]
fn a_value_sent_and_not_answered_is_named_as_uncertain_by_the_switch() {
    // Regression: a value sent to the host and not answered when the window left the figure was not named as uncertain.
    let mut harness = Harness::new();
    let _ = harness.app.call("figure.dataset.edit", json!({ "artifact_id": FIGURE }), PendingAction::DatasetEdit);
    let warning = harness.app.figure_close_warning().expect("the sent value is named");
    assert!(warning.contains("전송 중인 자료 값 변경 1개"), "{warning}");
    assert!(warning.contains("이미 작업물에 반영됐을 수 있으며"), "{warning}");

    harness.app.activate(figure_summary(OTHER, 1));
    assert_eq!(active_id(&harness), Some(FIGURE), "the switch waits for the answer");
    harness.app.answer_pending_switch(true);
    let (severity, text) = harness.notice().expect("the window says the value may be in the figure");
    assert_eq!(severity, Severity::Warning);
    assert!(text.contains("반영됐을 수 있습니다"), "{text}");
}

#[test]
fn a_start_that_the_host_refuses_after_its_wait_ended_shows_no_error() {
    let mut harness = Harness::new();
    let ticket = harness.app.call("job.start", json!({ "artifact_id": FIGURE }), PendingAction::StartJob).expect("the start is sent");
    harness.app.awaited_starts.insert(ticket, overdue());
    harness.app.expire_awaited_starts();
    harness.answer_ticket(ticket, Err(DltError::invalid("the job was refused")));
    assert!(harness.app.job.is_none());
    assert!(harness.notice().is_none_or(|(severity, _)| severity != Severity::Error), "a late refusal is not reported as an error");
}

#[test]
fn a_start_answered_after_the_switch_is_judged_unknown_as_its_draft_and_its_repeat_are() {
    // The late answer of a start uses the judgement of the request's draft and of the record kept for its repeat: an answer that
    // can come after the host received the start says the job may have started; a refusal that says nothing was done does not.
    let answer = |code: ErrorCode, retryable: bool| {
        let mut error = DltError::new(code, "the answer");
        error.retryable = retryable;
        error
    };
    let cases = [
        (answer(ErrorCode::Io, false), true),
        (answer(ErrorCode::Internal, false), true),
        (answer(ErrorCode::StorageFailure, false), true),
        (answer(ErrorCode::HostNotRunning, true), true),
        (answer(ErrorCode::TargetBusy, true), false),
        (answer(ErrorCode::InvalidArgument, false), false),
    ];
    for (error, unknown) in cases {
        let label = format!("{:?} retryable={}", error.code, error.retryable);
        let mut harness = Harness::new();
        let start = harness.start_request_with_a_file();
        harness.app.activate(figure_summary(OTHER, 1));
        harness.app.answer_pending_switch(true);
        assert_eq!(active_id(&harness), Some(OTHER), "{label}: the switch is made");
        harness.answer(&start, "job.start", Err(error));
        let late_unknown = harness.notice().is_some_and(|(_, text)| text == LATE_START_UNKNOWN);
        assert_eq!(late_unknown, unknown, "{label}: the late answer says the job may have started only when the outcome is unknown");
        assert_eq!(harness.app.unknown_starts.contains_key(FIGURE), unknown, "{label}: the start is kept for a repeat just as the warning says");
    }
}

#[test]
fn a_pdf_import_in_progress_is_named_and_its_late_answer_is_not_held() {
    // Regression: a page read in progress was neither named by the warning nor kept out of the window.
    let mut harness = Harness::new();
    let ticket = harness.app.call("source.extract_pdf", json!({ "sha256": "a".repeat(64), "pages": [PAGE] }), PendingAction::PdfEvidenceImport);
    harness.app.pdf_evidence_target = Some(PdfEvidenceTarget {
        artifact_id: FIGURE.into(),
        revision: 3,
        page_index: PAGE,
        file_name: "paper.pdf".into(),
        asset_sha256: Some("a".repeat(64)),
        awaiting: ticket,
    });
    let read = harness.sent();
    let warning = harness.app.figure_switch_warning().expect("the import is named");
    assert!(warning.contains("진행 중인 PDF 근거 가져오기 1개"), "{warning}");
    assert!(harness.app.ask_before_switch(&figure_summary(OTHER, 1)), "leaving asks first");
    harness.app.answer_pending_switch(true);
    assert!(harness.app.pdf_evidence_target.is_none(), "the import is no longer the window's");

    harness.answer(&read, "source.extract_pdf", Ok(json!({ "evidence": [record()] })));
    assert!(harness.app.held_pdf_evidence.is_empty(), "the late records are not held for the figure left");
}

#[test]
fn a_job_start_still_unanswered_is_named_and_asked_about_before_a_switch() {
    // Regression: a job start in flight had no job yet, so neither warning named it.
    let mut harness = Harness::new();
    harness.request();
    let start = harness.sent();
    assert_eq!(methods(&start), ["job.start"]);
    let warning = harness.app.figure_close_warning().expect("the start is unanswered");
    assert!(warning.contains("시작 응답을 기다리는 그림 AI 작업 1개"), "{warning}");
    assert!(harness.app.ask_before_switch(&figure_summary(OTHER, 1)), "leaving asks first");
}

#[test]
fn a_job_of_the_document_left_is_kept_aside_and_neither_shown_nor_cancelled() {
    // Regression: the job of figure-1 showed in figure-2's AI tab and its cancel button acted on it.
    let mut harness = Harness::new();
    harness.app.job = Some(running_job_of(FIGURE));
    harness.app.activate(figure_summary(OTHER, 1));
    assert!(harness.app.pending_switch.is_some(), "a running job of the open figure is asked about");
    harness.app.answer_pending_switch(true);

    assert_eq!(active_id(&harness), Some(OTHER));
    assert!(harness.app.job.is_none(), "figure-2 shows no job of figure-1");
    assert_eq!(harness.app.parked_jobs.len(), 1, "the job is kept aside");
    harness.app.cancel_job();
    assert!(harness.sent().iter().all(|request| request.1 != "job.cancel"), "figure-1's job is not cancelled from figure-2");

    assert_eq!(harness.app.figure_work(true).jobs_running, 1, "a close still counts it");
    assert_eq!(harness.app.figure_work(false).jobs_running, 0, "leaving figure-2 does not ask about figure-1's job");
    assert!(!harness.app.ask_before_switch(&figure_summary(FIGURE, 3)), "the question names the open document only");
}

#[test]
fn the_job_comes_back_with_its_document() {
    let mut harness = Harness::new();
    harness.app.job = Some(running_job_of(FIGURE));
    harness.app.activate(figure_summary(OTHER, 1));
    harness.app.answer_pending_switch(true);
    harness.app.activate(figure_summary(FIGURE, 3));
    assert_eq!(harness.app.job.as_ref().map(|job| job.id.as_str()), Some("job-1"), "figure-1's job is shown again");
    assert!(harness.app.parked_jobs.is_empty());
}

#[test]
fn a_finished_job_kept_aside_leaves_the_warnings_when_its_answer_comes() {
    let mut harness = Harness::new();
    harness.app.job = Some(running_job_of(FIGURE));
    harness.app.activate(figure_summary(OTHER, 1));
    harness.app.answer_pending_switch(true);

    harness.app.request_parked_job_status();
    let polls = harness.sent();
    assert_eq!(polls.iter().filter(|request| request.1 == "job.get").count(), 1, "the job kept aside is polled once");
    harness.answer(&polls, "job.get", Ok(job_json("job-1", FIGURE, "completed")));
    assert!(harness.app.parked_jobs.is_empty(), "a finished job kept aside is dropped");
    assert_eq!(harness.app.figure_work(true).jobs_running, 0);
    assert!(harness.app.figure_close_warning().is_none());
}

#[test]
fn a_file_import_answered_after_the_window_came_back_starts_no_job() {
    // Regression: a stale file import answer started a job for the request the window no longer held.
    let mut harness = Harness::new();
    let paper = TempFile::new("paper.pdf");
    harness.attach(&paper);
    harness.request();
    let import = harness.sent();
    assert!(import.iter().any(|request| request.1 == "asset.import"), "the file import is on its way");
    harness.app.activate(figure_summary(OTHER, 1));
    harness.app.answer_pending_switch(true);
    harness.app.activate(figure_summary(FIGURE, 3));

    harness.answer(&import, "asset.import", Ok(asset_json()));
    assert!(harness.sent().iter().all(|request| request.1 != "job.start"), "the stale import starts no job");
    assert!(harness.app.figure_ai_request.is_none());
}

#[test]
fn a_refused_stale_file_import_shows_no_error() {
    let mut harness = Harness::new();
    let paper = TempFile::new("paper.pdf");
    harness.attach(&paper);
    harness.request();
    let import = harness.sent();
    harness.app.activate(figure_summary(OTHER, 1));
    harness.app.answer_pending_switch(true);
    harness.app.activate(figure_summary(FIGURE, 3));

    harness.answer(&import, "asset.import", Err(DltError::new(ErrorCode::NotFound, "the file was moved")));
    assert!(harness.app.figure_ai_request.is_none());
    assert!(harness.notice().is_none_or(|(severity, _)| severity != Severity::Error), "a stale refusal shows no error");
}

#[test]
fn a_failed_poll_of_a_job_kept_aside_is_silent_and_keeps_the_notice() {
    // Regression: a background poll cleared the transient notice, and a failed poll was reported as an error.
    let mut harness = Harness::new();
    harness.app.job = Some(running_job_of(FIGURE));
    harness.app.activate(figure_summary(OTHER, 1));
    harness.app.answer_pending_switch(true);
    harness.app.notices.warning("probe", Instant::now());

    harness.app.request_parked_job_status();
    let polls = harness.sent();
    harness.answer(&polls, "job.get", Err(DltError::new(ErrorCode::Unavailable, "the host is busy")));
    assert!(harness.notice().is_some_and(|(_, text)| text == "probe"), "a background poll keeps the notice on screen");
    assert_eq!(harness.app.parked_jobs.len(), 1, "a failed poll keeps the job kept aside: only not-found drops it");
}

#[test]
fn a_poll_that_finds_no_job_drops_the_job_and_stops_its_polls() {
    // Regression: a job the host did not know was polled every two seconds for good.
    let mut harness = Harness::new();
    harness.app.job = Some(running_job_of(FIGURE));
    harness.app.activate(figure_summary(OTHER, 1));
    harness.app.answer_pending_switch(true);

    harness.app.request_parked_job_status();
    let polls = harness.sent();
    harness.answer(&polls, "job.get", Err(DltError::new(ErrorCode::NotFound, "job job-1 not found")));
    assert!(harness.app.parked_jobs.is_empty(), "the host does not know the job, so it is dropped");
    assert_eq!(harness.app.figure_work(true).jobs_running, 0, "nothing of it is counted any more");
    harness.app.request_parked_job_status();
    assert!(harness.sent().iter().all(|request| request.1 != "job.get"), "its status is not polled again");
}

#[test]
fn a_poll_of_a_job_kept_aside_is_not_sent_again_while_the_last_one_is_unanswered() {
    let mut harness = Harness::new();
    harness.app.job = Some(running_job_of(FIGURE));
    harness.app.activate(figure_summary(OTHER, 1));
    harness.app.answer_pending_switch(true);

    harness.app.request_parked_job_status();
    harness.app.request_parked_job_status();
    let polls = harness.sent();
    assert_eq!(polls.iter().filter(|request| request.1 == "job.get").count(), 1, "one poll is on its way at a time");
}

#[test]
fn a_failure_of_a_replaced_or_discarded_pdf_import_shows_no_error() {
    // Regression: the refusal of an import the window had replaced was raised as an error of the import in progress.
    let mut harness = Harness::new();
    let replaced = harness.app.call("asset.import", json!({ "path": "old.pdf" }), PendingAction::PdfAssetImport { page_index: PAGE });
    let current = harness.app.call("asset.import", json!({ "path": "new.pdf" }), PendingAction::PdfAssetImport { page_index: PAGE });
    harness.app.pdf_evidence_target = Some(PdfEvidenceTarget {
        artifact_id: FIGURE.into(),
        revision: 3,
        page_index: PAGE,
        file_name: "new.pdf".into(),
        asset_sha256: None,
        awaiting: current,
    });
    harness.answer_ticket(replaced.expect("the import is sent"), Err(DltError::new(ErrorCode::NotFound, "the file was moved")));
    assert!(harness.notice().is_none_or(|(severity, _)| severity != Severity::Error), "the replaced import's refusal shows no error");
    assert!(harness.app.pdf_evidence_target.is_some(), "the import in progress is still the window's");

    // Once the import is discarded, its late refusal shows nothing either.
    harness.app.pdf_evidence_target = None;
    harness.answer_ticket(current.expect("the import is sent"), Err(DltError::new(ErrorCode::NotFound, "the file was moved")));
    assert!(harness.notice().is_none_or(|(severity, _)| severity != Severity::Error), "a discarded import's refusal shows no error");
}

/// A job of `artifact_id` with this id and status, as the host reports it.
fn job_of(id: &str, artifact_id: &str, status: &str) -> Job {
    serde_json::from_value(job_json(id, artifact_id, status)).expect("a job")
}

/// A workspace listing with `artifacts`.
fn listing_of(artifacts: &[ArtifactSummary]) -> Value {
    json!({ "workspace_id": "w1", "artifacts": artifacts })
}

#[test]
fn a_running_job_the_newer_start_of_its_figure_replaced_is_still_counted_and_polled() {
    // Regression: the older running job of the open figure was kept aside, and the switch warning did not count it.
    let mut harness = Harness::new();
    harness.app.record_started_job(job_of("job-1", FIGURE, "running"));
    harness.app.record_started_job(job_of("job-2", FIGURE, "running"));
    assert_eq!(harness.app.job.as_ref().map(|job| job.id.as_str()), Some("job-2"), "the newest start is shown");
    assert_eq!(harness.app.figure_work(false).jobs_running, 2, "leaving the figure counts both running jobs");
    assert!(harness.app.figure_switch_warning().is_some_and(|warning| warning.contains("실행 중인 그림 AI 작업 2개")));

    harness.app.request_parked_job_status();
    let polls = harness.sent();
    assert_eq!(polls.iter().filter(|request| request.1 == "job.get").count(), 1, "the job pushed aside is polled");
    harness.answer(&polls, "job.get", Ok(job_json("job-1", FIGURE, "completed")));
    assert!(harness.app.parked_jobs.is_empty(), "a finished job pushed aside is dropped");
    assert_eq!(harness.app.figure_work(false).jobs_running, 1, "the shown job still runs");
}

#[test]
fn a_late_answer_for_a_job_already_dropped_does_not_take_the_shown_place() {
    // Regression: a late answer for a finished job was placed as the newest one, so it took the place of the job now shown.
    let mut harness = Harness::new();
    harness.app.record_started_job(job_of("job-1", FIGURE, "running"));
    let poll = PendingAction::GetJob { job_id: "job-1".into() };
    let stale = harness.app.call("job.get", json!({ "job_id": "job-1", "attempt_id": "attempt-1" }), poll.clone());
    let fresh = harness.app.call("job.get", json!({ "job_id": "job-1", "attempt_id": "attempt-1" }), poll);
    harness.answer_ticket(fresh.expect("the poll is sent"), Ok(job_json("job-1", FIGURE, "completed")));
    harness.app.record_started_job(job_of("job-2", FIGURE, "running"));
    harness.answer_ticket(stale.expect("the poll is sent"), Ok(job_json("job-1", FIGURE, "running")));

    assert_eq!(harness.app.job.as_ref().map(|job| job.id.as_str()), Some("job-2"), "the late answer does not take the shown place");
    assert!(harness.app.parked_jobs.is_empty(), "the job the window dropped does not come back");
}

#[test]
fn a_workspace_listing_without_an_artifact_drops_its_jobs_and_stops_their_polls() {
    // Regression: the job of a figure the workspace no longer listed was polled every two seconds.
    let mut harness = Harness::new();
    harness.app.record_started_job(job_of("job-1", FIGURE, "running"));
    harness.app.activate(figure_summary(OTHER, 1));
    harness.app.answer_pending_switch(true);
    assert_eq!(harness.app.parked_jobs.len(), 1, "the job is kept aside");

    harness.app.call("workspace.list", json!({}), PendingAction::WorkspaceList).expect("the list is sent");
    let list = harness.sent();
    harness.answer(&list, "workspace.list", Ok(listing_of(&[figure_summary(OTHER, 1)])));
    assert!(harness.app.parked_jobs.is_empty(), "the job of the figure no longer listed is dropped");
    harness.app.request_parked_job_status();
    assert!(harness.sent().iter().all(|request| request.1 != "job.get"), "its status is not polled");
}

#[test]
fn a_workspace_listing_without_the_open_artifact_drops_the_job_shown_for_it() {
    let mut harness = Harness::new();
    harness.app.record_started_job(job_of("job-1", FIGURE, "running"));
    harness.app.call("workspace.list", json!({}), PendingAction::WorkspaceList).expect("the list is sent");
    let list = harness.sent();
    harness.answer(&list, "workspace.list", Ok(listing_of(&[figure_summary(OTHER, 1)])));
    assert!(harness.app.job.is_none(), "no window shows the job of a figure no longer listed");
}

#[test]
fn a_running_job_is_shown_before_a_finished_one_and_the_kept_aside_one_takes_the_place_when_the_shown_one_ends() {
    // Regression: the newest job was shown even when it had ended, and a running job was hidden behind it.
    let mut harness = Harness::new();
    harness.app.job = Some(job_of("job-2", FIGURE, "completed"));
    harness.app.parked_jobs = vec![job_of("job-1", FIGURE, "running")];
    harness.app.place_jobs(None);
    assert_eq!(harness.app.job.as_ref().map(|job| job.id.as_str()), Some("job-1"), "the running job is shown");
    assert!(harness.app.parked_jobs.is_empty(), "the job that ended is dropped");

    // Two running jobs: the newest is shown; once it ends, the other one takes its place, where it can be cancelled.
    let mut harness = Harness::new();
    harness.app.record_started_job(job_of("job-1", FIGURE, "running"));
    harness.app.record_started_job(job_of("job-2", FIGURE, "running"));
    harness.app.update_job(job_of("job-2", FIGURE, "completed"));
    assert_eq!(harness.app.job.as_ref().map(|job| job.id.as_str()), Some("job-1"), "the job still running is shown");
    assert!(harness.app.parked_jobs.is_empty());
    assert_eq!(harness.app.figure_work(false).jobs_running, 1);
}

#[test]
fn a_document_that_is_not_a_figure_has_no_figure_work() {
    // Regression: the AI job of an open canvas counted as figure work in the switch question.
    let mut harness = Harness::new();
    harness.app.active = Some(ArtifactSummary { kind: ArtifactKind::Canvas, ..figure_summary("canvas-1", 1) });
    harness.app.job = Some(running_job_of("canvas-1"));
    assert_eq!(harness.app.figure_work(false).jobs_running, 0, "a canvas job is not figure work");
    assert_eq!(harness.app.figure_work(true).jobs_running, 0, "a close does not count it as figure work either");
    assert!(harness.app.figure_switch_warning().is_none());
}

#[test]
fn leaving_a_canvas_with_an_unanswered_job_start_says_the_job_may_have_started() {
    // Regression: no question and no note followed a job start of a document that is not a figure.
    let mut harness = Harness::new();
    harness.app.active = Some(ArtifactSummary { kind: ArtifactKind::Canvas, ..figure_summary("canvas-1", 1) });
    let _ = harness.app.call("job.start", json!({ "artifact_id": "canvas-1" }), PendingAction::StartJob);
    harness.app.activate(figure_summary(OTHER, 1));
    assert_eq!(active_id(&harness), Some(OTHER), "a canvas asks no question, so the switch is made");
    let (severity, text) = harness.notice().expect("the note is shown after the switch");
    assert_eq!(severity, Severity::Warning);
    assert!(text.contains("작업 상태를 확인"), "{text}");
}

#[test]
fn a_changed_value_under_edit_is_named_and_dropped_only_by_the_answer() {
    // Regression: leaving the figure dropped a value under edit without a word.
    let mut harness = Harness::new();
    harness.app.dataset_edit = Some(DatasetCellEdit {
        dataset_id: "dataset-1".into(),
        version_hash: "v1".into(),
        row_id: "row-1".into(),
        column_id: "column-1".into(),
        original: "0.10".into(),
        draft: "0.25".into(),
    });
    let warning = harness.app.figure_close_warning().expect("the changed value is named");
    assert!(warning.contains("편집 중인 자료 값 1개"), "{warning}");
    assert!(warning.contains("창을 닫으면 편집 중인 자료 값은 버려집니다."), "{warning}");

    harness.app.activate(figure_summary(OTHER, 1));
    assert_eq!(active_id(&harness), Some(FIGURE), "the switch waits for the answer");
    assert!(harness.app.dataset_edit.is_some(), "nothing is dropped before the answer");
    harness.app.answer_pending_switch(true);
    assert_eq!(active_id(&harness), Some(OTHER));
    assert!(harness.app.dataset_edit.is_none());

    // A value that was not changed loses nothing and asks nothing.
    harness.app.dataset_edit = Some(DatasetCellEdit {
        dataset_id: "dataset-1".into(),
        version_hash: "v1".into(),
        row_id: "row-1".into(),
        column_id: "column-1".into(),
        original: "0.10".into(),
        draft: "0.10".into(),
    });
    assert!(harness.app.figure_close_warning().is_none());
}

#[test]
fn a_held_set_of_another_figure_is_not_the_work_of_the_figure_on_screen() {
    // Regression: a set held for another figure was counted, and discarded, as the work of the figure on screen.
    let mut harness = Harness::new();
    let mut other_held = held(None);
    other_held.target.artifact_id = OTHER.into();
    harness.app.held_pdf_evidence.push(other_held);
    assert!(harness.app.figure_switch_warning().is_none(), "leaving this figure loses nothing of the other figure's");
    assert!(
        harness.app.figure_close_warning().is_some_and(|warning| warning.contains("보류한 PDF 근거 1개")),
        "closing counts every held set"
    );
    harness.app.activate(figure_summary(OTHER, 1));
    assert_eq!(harness.app.held_pdf_evidence.len(), 1, "the set stays with its figure");
}

#[test]
fn a_shown_job_the_host_does_not_know_is_dropped_and_not_polled_again() {
    // Regression: the shown job stayed on, and its poll failed with "not found" every two seconds.
    let mut harness = Harness::new();
    harness.app.record_started_job(job_of("job-1", FIGURE, "running"));
    harness.app.request_job_status();
    let polls = harness.sent();
    assert_eq!(polls.iter().filter(|request| request.1 == "job.get").count(), 1, "the shown job is polled");
    harness.answer(&polls, "job.get", Err(DltError::new(ErrorCode::NotFound, "job job-1 not found")));
    assert!(harness.app.job.is_none(), "the job the host does not know is dropped");
    assert_eq!(harness.app.figure_work(true).jobs_running, 0);
    assert!(harness.notice().is_none_or(|(severity, _)| severity != Severity::Error), "the poll raises no error");
    harness.app.request_job_status();
    assert!(harness.sent().iter().all(|request| request.1 != "job.get"), "its status is not polled again");
}

#[test]
fn a_kept_aside_job_not_found_after_it_was_shown_again_is_dropped_from_the_shown_place() {
    // Regression: the job was shown again while its poll was on its way, and the not-found answer left it shown.
    let mut harness = Harness::new();
    harness.app.job = Some(running_job_of(FIGURE));
    harness.app.activate(figure_summary(OTHER, 1));
    harness.app.answer_pending_switch(true);
    harness.app.request_parked_job_status();
    let polls = harness.sent();

    harness.app.activate(figure_summary(FIGURE, 3));
    assert!(harness.app.job.is_some(), "the job is shown again while the poll is on its way");
    harness.answer(&polls, "job.get", Err(DltError::new(ErrorCode::NotFound, "job job-1 not found")));
    assert!(harness.app.job.is_none(), "the host does not know the job, so it is not shown");
    assert!(harness.app.parked_jobs.is_empty());
    harness.app.request_job_status();
    assert!(harness.sent().iter().all(|request| request.1 != "job.get"), "its status is not polled again");
}

#[test]
fn the_close_warning_counts_every_unanswered_job_start() {
    // Regression: the count of unanswered starts was fixed at one.
    let mut harness = Harness::new();
    let _ = harness.app.call("job.start", json!({ "artifact_id": FIGURE }), PendingAction::StartJob);
    let _ = harness.app.call("job.start", json!({ "artifact_id": FIGURE }), PendingAction::StartJob);
    let warning = harness.app.figure_close_warning().expect("both starts are unanswered");
    assert!(warning.contains("시작 응답을 기다리는 그림 AI 작업 2개"), "{warning}");
}

#[test]
fn an_evidence_import_not_answered_in_time_ends_the_request_and_its_late_answer_starts_no_job() {
    // Regression: a lost import answer kept the request (and the list) locked for good.
    let mut harness = Harness::new();
    let paper = TempFile::new("paper.pdf");
    harness.attach(&paper);
    harness.request();
    let import = harness.sent();
    let ticket = import.iter().find(|request| request.1 == "asset.import").expect("the import is on its way").0;

    harness.app.awaited_starts.insert(ticket, overdue());
    harness.app.expire_awaited_starts();
    assert!(harness.app.figure_ai_request.is_none(), "the request ends");
    assert!(harness.notice().is_some_and(|(severity, _)| severity == Severity::Error), "the window says the import timed out");
    assert_eq!(harness.app.figure_ai_files().len(), 1, "the list stays for the next request");

    harness.answer_ticket(ticket, Ok(asset_json()));
    assert!(harness.sent().iter().all(|request| request.1 != "job.start"), "the late import starts no job");
}

#[test]
fn opening_the_same_figure_again_asks_before_a_value_under_edit_is_dropped() {
    // Regression: opening the figure that is open again dropped the value under edit without a question.
    let mut harness = Harness::new();
    harness.app.dataset_edit = Some(DatasetCellEdit {
        dataset_id: "dataset-1".into(),
        version_hash: "v1".into(),
        row_id: "row-1".into(),
        column_id: "column-1".into(),
        original: "0.10".into(),
        draft: "0.25".into(),
    });
    harness.app.activate(figure_summary(FIGURE, 3));
    assert!(harness.app.pending_switch.is_some(), "the question is asked first");
    assert!(harness.app.dataset_edit.is_some(), "nothing is dropped before the answer");
    let warning = harness.app.switch_loss(&figure_summary(FIGURE, 3)).expect("the value is named");
    assert!(warning.contains("편집 중인 자료 값 1개"), "{warning}");
    assert!(warning.contains("다시 열면 편집 중인 자료 값은 버려집니다."), "{warning}");

    harness.app.answer_pending_switch(false);
    assert!(harness.app.dataset_edit.is_some(), "계속 작업 keeps the value");
    harness.app.activate(figure_summary(FIGURE, 3));
    harness.app.answer_pending_switch(true);
    assert!(harness.app.dataset_edit.is_none(), "the answer drops it");
    assert_eq!(active_id(&harness), Some(FIGURE));
}

#[test]
fn a_csv_import_under_way_is_named_and_dropped_when_the_figure_is_opened_again() {
    // Regression: the CSV draft of the figure was dropped by opening the figure again, without a word.
    let mut harness = Harness::new();
    let mut host = Host::start();
    harness.app.csv_draft = Some(CsvImportDraft::to_declare(host.import_table("group,value\nA,1\nB,2\n")));
    let warning = harness.app.switch_loss(&figure_summary(FIGURE, 3)).expect("the draft is named");
    assert!(warning.contains("편집 중인 CSV 가져오기 1개"), "{warning}");
    harness.app.activate(figure_summary(FIGURE, 3));
    assert!(harness.app.pending_switch.is_some(), "the question is asked first");
    harness.app.answer_pending_switch(true);
    assert!(harness.app.csv_draft.is_none(), "the answer drops the draft");
}

#[test]
fn a_commit_on_its_way_for_another_figure_holds_nothing_back_here() {
    // Regression: a commit of figure-1 held the figure-2 window's PDF, review and plot controls back.
    let mut harness = Harness::new();
    let _ = harness.app.call("changes.apply", json!({ "artifact_id": FIGURE }), PendingAction::FigureEvidence);
    assert_eq!(harness.app.requests_on_their_way().count(), 1, "the commit of the open figure is on its way");
    harness.app.active = Some(figure_summary(OTHER, 1));
    assert_eq!(harness.app.requests_on_their_way().count(), 0, "figure-1's commit holds nothing back on figure-2");
}

#[test]
fn held_pdf_evidence_of_a_figure_the_listing_no_longer_has_is_dropped() {
    // Regression: the records held for a deleted figure stayed in the inspector and in every close warning.
    let mut harness = Harness::new();
    harness.app.held_pdf_evidence.push(held(None));
    harness.app.call("workspace.list", json!({}), PendingAction::WorkspaceList).expect("the list is sent");
    let list = harness.sent();
    harness.answer(&list, "workspace.list", Ok(listing_of(&[figure_summary(OTHER, 1)])));
    assert!(harness.app.held_pdf_evidence.is_empty(), "the figure is no longer listed, so its records go");
    assert!(harness.app.figure_close_warning().is_none());
}

#[test]
fn a_late_answer_of_a_replaced_import_keeps_the_notice_on_screen() {
    // Regression: the answer of an import that was replaced cleared the notice on screen.
    let mut harness = Harness::new();
    // The request itself clears the notice when it is sent, so the notice is raised after that.
    let replaced = harness.app.call("asset.import", json!({ "path": "old.pdf" }), PendingAction::PdfAssetImport { page_index: PAGE });
    harness.app.notices.warning("probe", Instant::now());
    harness.answer_ticket(replaced.expect("the import is sent"), Ok(asset_json()));
    assert!(harness.notice().is_some_and(|(_, text)| text == "probe"), "the answer of a discarded import clears nothing");
}
