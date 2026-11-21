//! Figure AI requests with evidence files (`figure_ai_attach`): the list is the editor's own, every file is imported
//! before the job, and the job starts only while the Figure is still the artifact and revision the request was asked at.
//! A failed or cancelled request keeps the prompt and the list. No runtime runs here: the host is played by the test.

use std::{
    path::PathBuf,
    sync::mpsc::{Receiver, Sender},
    time::Instant,
};

use designllm_contracts::{new_id, ArtifactKind, ArtifactSummary, ContextSnapshot, DltError, ErrorCode};
use eframe::egui::Context;
use serde_json::{json, Value};

use super::{DesktopApp, Screen};
use crate::{
    app::{
        link_flow_tests::drawn_figure,
        test_support::{Host, Window},
    },
    rpc::{RpcPayload, RpcReply, RpcWorker, Work},
};

type Request = (u64, String, Value);

const FIGURE: &str = "figure-1";
const PROMPT: &str = "곡선을 추가하세요";

struct Harness {
    app: DesktopApp,
    replies: Sender<RpcReply>,
    work: Receiver<Work>,
    ctx: Context,
}

impl Harness {
    /// One open Figure at revision 3 with a model, and the prompt typed with spaces around it.
    fn new() -> Self {
        let (rpc, replies, work) = RpcWorker::detached();
        let mut app = DesktopApp::new(rpc, "그림 근거".into(), None, None);
        while work.try_recv().is_ok() {}
        app.screen = Screen::Editor;
        app.active = Some(ArtifactSummary {
            id: FIGURE.into(),
            kind: ArtifactKind::Figure,
            title: "그림".into(),
            revision: 3,
            engine_id: "figure".into(),
            dirty: false,
            modified_at_ms: None,
            active_jobs: Vec::new(),
        });
        app.context = Some(context(FIGURE, 3, &app.view_id));
        app.ai_prompt = format!("  {PROMPT}  ");
        app.ai_model = "mock-model".into();
        app.ai_runtime = "mock".into();
        app.ai_effort = "high".into();
        Self { app, replies, work, ctx: Context::default() }
    }

    /// The requests the window sent since the last call, oldest first.
    fn sent(&mut self) -> Vec<Request> {
        let mut batch = Vec::new();
        while let Ok(work) = self.work.try_recv() {
            if let Work::Call { ticket, method, params, .. } = work {
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

    fn message(&self) -> String {
        self.app.notices.current(Instant::now()).map(|notice| notice.text.clone()).unwrap_or_default()
    }

    /// Starts a request the way the AI tab does: the Ctrl+Enter command with the typed prompt.
    fn request(&mut self) {
        self.app.invoke_ui("ai.request", json!({ "prompt": format!("  {PROMPT}  ") }));
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

fn methods(requests: &[Request]) -> Vec<&str> {
    requests.iter().map(|request| request.1.as_str()).collect()
}

/// The asset the host returns for an import: a 64-digit hash made of `digit`.
fn asset(digit: char, mime: &str) -> Value {
    json!({ "sha256": digit.to_string().repeat(64), "mime": mime, "bytes": 13 })
}

fn started_job() -> Value {
    json!({
        "id": "job-1",
        "attempt_id": "attempt-1",
        "artifact_id": FIGURE,
        "base_revision": 3,
        "status": "queued",
        "allowed_intents": ["content"],
        "model": "mock-model",
        "effort": "high",
        "error": null,
    })
}

/// A file on disk for the list. Its folder is removed when the test ends.
struct Evidence {
    folder: PathBuf,
    path: PathBuf,
}

impl Evidence {
    fn new(name: &str, bytes: &[u8]) -> Self {
        let folder = std::env::temp_dir().join(format!("designllm-figure-ai-{}", new_id()));
        std::fs::create_dir_all(&folder).expect("a folder for the evidence");
        let path = folder.join(name);
        std::fs::write(&path, bytes).expect("the evidence file is written");
        Self { folder, path }
    }
}

impl Drop for Evidence {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.folder);
    }
}

#[test]
fn the_editor_list_is_its_own_and_the_home_list_is_left_alone() {
    let mut harness = Harness::new();
    harness.app.home.attachments = vec!["C:/home/ref.png".into()];
    let paper = Evidence::new("paper.pdf", b"%PDF-1.4 evidence");
    harness.app.add_figure_ai_attachment(paper.path.clone()).expect("a PDF joins the list");
    assert_eq!(harness.app.figure_ai_files().len(), 1);
    assert_eq!(harness.app.figure_ai_files()[0].name, "paper.pdf");
    assert_eq!(harness.app.home.attachments, ["C:/home/ref.png"], "the home list keeps its own entries");
    assert!(harness.sent().is_empty(), "attaching sends nothing to the host");
}

#[test]
fn only_pdf_csv_tsv_and_text_files_join_the_list_once_each() {
    let mut harness = Harness::new();
    let pdf = Evidence::new("paper.PDF", b"%PDF-1.4");
    let docx = Evidence::new("notes.docx", b"PK");
    assert!(harness.app.add_figure_ai_attachment(docx.path.clone()).is_err(), "a Word file is not evidence here");
    assert!(harness.app.add_figure_ai_attachment(PathBuf::from("C:/no-such-folder/none.csv")).is_err(), "a missing file is refused");
    harness.app.add_figure_ai_attachment(pdf.path.clone()).expect("the upper-case extension counts");
    assert!(harness.app.add_figure_ai_attachment(pdf.path.clone()).is_err(), "the same file joins once");
    let listed: Vec<Evidence> = ["measurements.csv", "table.tsv", "method.txt", "method.md"]
        .iter()
        .map(|name| Evidence::new(name, b"text"))
        .collect();
    for file in &listed {
        harness.app.add_figure_ai_attachment(file.path.clone()).expect("a listed format joins");
    }
    assert_eq!(harness.app.figure_ai_files().len(), 5);
    harness.app.remove_figure_ai_attachment(0).expect("an idle list can drop a file");
    assert_eq!(harness.app.figure_ai_files().len(), 4);
}

#[test]
fn the_list_holds_at_most_sixteen_files() {
    let mut harness = Harness::new();
    let files: Vec<Evidence> = (0..17).map(|index| Evidence::new(&format!("row-{index}.csv"), b"x\n1\n")).collect();
    for file in &files[..16] {
        harness.app.add_figure_ai_attachment(file.path.clone()).expect("up to sixteen files join");
    }
    assert!(harness.app.add_figure_ai_attachment(files[16].path.clone()).is_err(), "the seventeenth is refused");
    assert_eq!(harness.app.figure_ai_files().len(), 16);
}

#[test]
fn attaching_is_refused_outside_an_open_figure() {
    let mut harness = Harness::new();
    harness.app.active.as_mut().expect("a document is open").kind = ArtifactKind::Canvas;
    let file = Evidence::new("paper.pdf", b"%PDF-1.4");
    assert!(harness.app.add_figure_ai_attachment(file.path.clone()).is_err());
    assert!(harness.app.figure_ai_files().is_empty());
}

#[test]
fn a_request_imports_each_file_in_order_then_starts_one_job_on_their_assets() {
    let mut harness = Harness::new();
    let paper = Evidence::new("paper.pdf", b"%PDF-1.4 method");
    let table = Evidence::new("measurements.csv", b"dose,response\n1,2\n");
    harness.app.add_figure_ai_attachment(paper.path.clone()).expect("the PDF joins");
    harness.app.add_figure_ai_attachment(table.path.clone()).expect("the CSV joins");

    harness.request();
    let first = harness.sent();
    assert_eq!(methods(&first), ["asset.import"], "nothing else before the first import");
    assert_eq!(first[0].2["path"].as_str(), paper.path.to_str());
    harness.answer(&first, "asset.import", Ok(asset('a', "application/pdf")));

    let second = harness.sent();
    assert_eq!(methods(&second), ["asset.import"], "the second file is imported after the first");
    assert_eq!(second[0].2["path"].as_str(), table.path.to_str());
    harness.answer(&second, "asset.import", Ok(asset('b', "text/csv")));

    let job = harness.sent();
    assert_eq!(methods(&job), ["job.start"]);
    let params = &job[0].2;
    assert_eq!(params["artifact_id"], FIGURE);
    assert_eq!(params["base_revision"], 3, "the job is bound to the revision the request was asked at");
    assert_eq!(params["prompt"], PROMPT, "the prompt is sent trimmed");
    assert_eq!(params["source_refs"], json!([asset('a', "application/pdf"), asset('b', "text/csv")]));
    let mut intents: Vec<&str> = params["allowed_intents"].as_array().expect("intents").iter().filter_map(Value::as_str).collect();
    intents.sort_unstable();
    assert_eq!(intents, ["content", "design"]);
    assert_eq!(std::fs::read(&paper.path).expect("the PDF is still there"), b"%PDF-1.4 method", "the evidence is only read");
    harness.answer(&job, "job.start", Ok(started_job()));
    assert_eq!(harness.app.figure_ai_files().len(), 2, "the list stays for the next request");
    assert!(harness.app.figure_ai_request.is_none(), "the job start reply ends the request");
}

#[test]
fn a_request_without_files_is_the_plain_job_start_with_no_sources() {
    let mut harness = Harness::new();
    harness.request();
    let sent = harness.sent();
    assert_eq!(methods(&sent), ["job.start"]);
    assert!(sent[0].2["source_refs"].as_array().expect("sources").is_empty());
}

#[test]
fn a_failed_import_sends_no_job_and_keeps_the_prompt_and_the_files() {
    let mut harness = Harness::new();
    let paper = Evidence::new("paper.pdf", b"%PDF-1.4");
    let table = Evidence::new("measurements.csv", b"x\n1\n");
    harness.app.add_figure_ai_attachment(paper.path.clone()).expect("the PDF joins");
    harness.app.add_figure_ai_attachment(table.path.clone()).expect("the CSV joins");
    harness.request();
    let first = harness.sent();
    harness.answer(&first, "asset.import", Ok(asset('a', "application/pdf")));
    let second = harness.sent();
    harness.answer(&second, "asset.import", Err(DltError::new(ErrorCode::NotFound, "the file was moved")));

    assert!(harness.sent().is_empty(), "no job starts without every file");
    assert!(harness.app.figure_ai_request.is_none());
    assert_eq!(harness.app.figure_ai_files().len(), 2);
    assert_eq!(harness.app.ai_prompt, PROMPT);
    assert!(harness.message().contains("남겨 두었습니다"), "the message says what was kept: {}", harness.message());
}

#[test]
fn cancelling_during_the_imports_keeps_the_prompt_and_files_and_ignores_the_late_reply() {
    let mut harness = Harness::new();
    let paper = Evidence::new("paper.pdf", b"%PDF-1.4");
    harness.app.add_figure_ai_attachment(paper.path.clone()).expect("the PDF joins");
    harness.request();
    let first = harness.sent();
    assert_eq!(methods(&first), ["asset.import"]);

    harness.app.cancel_figure_ai_request().expect("the import can be cancelled");
    assert!(harness.app.figure_ai_request.is_none());
    harness.answer(&first, "asset.import", Ok(asset('a', "application/pdf")));

    assert!(harness.sent().is_empty(), "the late reply of a cancelled request starts nothing");
    assert_eq!(harness.app.figure_ai_files().len(), 1);
    assert_eq!(harness.app.ai_prompt, PROMPT);
}

#[test]
fn a_revision_change_before_the_job_starts_sends_no_job() {
    let mut harness = Harness::new();
    let paper = Evidence::new("paper.pdf", b"%PDF-1.4");
    let table = Evidence::new("measurements.csv", b"x\n1\n");
    harness.app.add_figure_ai_attachment(paper.path.clone()).expect("the PDF joins");
    harness.app.add_figure_ai_attachment(table.path.clone()).expect("the CSV joins");
    harness.request();
    let first = harness.sent();
    harness.answer(&first, "asset.import", Ok(asset('a', "application/pdf")));
    let second = harness.sent();

    // A commit moved the Figure on while the last file was importing.
    harness.app.active.as_mut().expect("a document is open").revision = 4;
    harness.app.context.as_mut().expect("a context").revision = 4;
    harness.answer(&second, "asset.import", Ok(asset('b', "text/csv")));

    assert!(harness.sent().is_empty(), "the job is not started on a revision the user did not ask about");
    assert!(harness.app.figure_ai_request.is_none());
    assert_eq!(harness.app.figure_ai_files().len(), 2);
    assert_eq!(harness.app.ai_prompt, PROMPT);
    assert!(harness.message().contains("바뀌어"), "the message names the change: {}", harness.message());
}

#[test]
fn switching_documents_drops_the_pending_import_and_sends_no_job() {
    let mut harness = Harness::new();
    let paper = Evidence::new("paper.pdf", b"%PDF-1.4");
    harness.app.add_figure_ai_attachment(paper.path.clone()).expect("the PDF joins");
    harness.request();
    let first = harness.sent();

    harness.app.active.as_mut().expect("a document is open").id = "figure-2".into();
    harness.answer(&first, "asset.import", Ok(asset('a', "application/pdf")));

    assert!(harness.sent().is_empty(), "the window now shows another document: nothing is sent for this request");
    assert!(harness.app.figure_ai_request.is_none());
    assert_eq!(harness.app.figure_ai_attachments.as_ref().map(|list| list.files.len()), Some(1), "the list stays with figure-1, as before");
    assert!(harness.app.figure_ai_files().is_empty(), "the list of the other document is not shown");
    assert_eq!(harness.app.ai_prompt, PROMPT);
}

#[test]
fn a_refused_job_start_keeps_the_prompt_and_the_files_for_a_retry() {
    let mut harness = Harness::new();
    let paper = Evidence::new("paper.pdf", b"%PDF-1.4");
    harness.app.add_figure_ai_attachment(paper.path.clone()).expect("the PDF joins");
    harness.request();
    let import = harness.sent();
    harness.answer(&import, "asset.import", Ok(asset('a', "application/pdf")));
    let job = harness.sent();
    assert_eq!(methods(&job), ["job.start"]);

    // A plain refusal: a revision conflict would also start a state refresh, which this test does not model.
    harness.answer(&job, "job.start", Err(DltError::invalid("the job was refused")));
    assert!(harness.app.figure_ai_request.is_none());
    assert!(harness.app.job.is_none(), "no job is shown for a refused start");
    assert_eq!(harness.app.figure_ai_files().len(), 1);
    assert_eq!(harness.app.ai_prompt, PROMPT);
    assert!(harness.message().contains("남겨 두었습니다"), "the message says what was kept: {}", harness.message());

    harness.app.invoke_ui("ai.retry", json!({}));
    assert_eq!(methods(&harness.sent()), ["asset.import"], "a retry imports the files again and starts the job after them");
}

/// A request with files whose job start was lost (its outcome unknown) is asked again with the same request id, so the host
/// answers it from its receipt and no second job is made (backlog desktop 19: `send_figure_ai_job` with files).
#[test]
fn a_lost_start_of_a_request_with_files_is_repeated_with_its_id() {
    let mut harness = Harness::new();
    let paper = Evidence::new("paper.pdf", b"%PDF-1.4");
    harness.app.add_figure_ai_attachment(paper.path.clone()).expect("the PDF joins");
    let start = |harness: &mut Harness| {
        harness.request();
        let import = harness.sent();
        assert_eq!(methods(&import), ["asset.import"], "the files are imported first");
        harness.answer(&import, "asset.import", Ok(asset('a', "application/pdf")));
        let job = harness.sent();
        assert_eq!(methods(&job), ["job.start"]);
        job
    };
    let first = start(&mut harness);
    harness.answer(&first, "job.start", Err(DltError::new(ErrorCode::Io, "the connection was lost")));
    assert!(harness.app.job.is_none(), "no job is known");

    let again = start(&mut harness);
    assert_eq!(again[0].2["request_id"], first[0].2["request_id"], "the repeat keeps the lost start's id");
    assert_eq!(again[0].2["source_refs"], first[0].2["source_refs"], "the same files");
}

#[test]
fn a_started_job_is_shown_and_the_list_is_held_until_then() {
    let mut harness = Harness::new();
    let paper = Evidence::new("paper.pdf", b"%PDF-1.4");
    harness.app.add_figure_ai_attachment(paper.path.clone()).expect("the PDF joins");
    harness.request();
    let import = harness.sent();
    harness.answer(&import, "asset.import", Ok(asset('a', "application/pdf")));
    let job = harness.sent();

    assert!(harness.app.cancel_figure_ai_request().is_err(), "the job start is not cancelled from here");
    assert!(harness.app.remove_figure_ai_attachment(0).is_err(), "the list is held while the job starts");

    harness.answer(&job, "job.start", Ok(started_job()));
    assert!(harness.app.figure_ai_request.is_none());
    assert_eq!(harness.app.job.as_ref().map(|job| job.id.as_str()), Some("job-1"));
    assert_eq!(harness.app.figure_ai_files().len(), 1);
}

#[test]
fn the_list_does_not_join_a_request_made_outside_a_figure() {
    let mut harness = Harness::new();
    let paper = Evidence::new("paper.pdf", b"%PDF-1.4");
    harness.app.add_figure_ai_attachment(paper.path.clone()).expect("the PDF joins");
    harness.app.active.as_mut().expect("a document is open").kind = ArtifactKind::Canvas;
    harness.request();
    let sent = harness.sent();
    assert_eq!(methods(&sent), ["job.start"]);
    assert!(sent[0].2["source_refs"].as_array().expect("sources").is_empty());
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

#[test]
fn a_request_never_carries_the_list_of_another_figure() {
    let mut harness = Harness::new();
    let paper = Evidence::new("paper.pdf", b"%PDF-1.4");
    harness.app.add_figure_ai_attachment(paper.path.clone()).expect("the PDF joins figure-1's list");

    // The window shows figure-2 while the list still belongs to figure-1.
    harness.app.active = Some(figure_summary("figure-2", 3));
    assert!(harness.app.figure_ai_files().is_empty(), "the list of figure-1 is not shown on figure-2");
    assert!(!harness.app.figure_ai_sources_apply());
    harness.request();
    let sent = harness.sent();
    assert_eq!(methods(&sent), ["job.start"], "no evidence of figure-1 is imported for figure-2");
    assert_eq!(sent[0].2["artifact_id"], "figure-2");
    assert!(sent[0].2["source_refs"].as_array().expect("sources").is_empty());
}

#[test]
fn attaching_on_another_figure_starts_that_figures_own_list() {
    let mut harness = Harness::new();
    let paper = Evidence::new("paper.pdf", b"%PDF-1.4");
    let table = Evidence::new("measurements.csv", b"x\n1\n");
    harness.app.add_figure_ai_attachment(paper.path.clone()).expect("the PDF joins figure-1's list");
    harness.app.active = Some(figure_summary("figure-2", 3));
    harness.app.add_figure_ai_attachment(table.path.clone()).expect("the CSV joins figure-2's list");
    assert_eq!(harness.app.figure_ai_files().len(), 1);
    assert_eq!(harness.app.figure_ai_files()[0].name, "measurements.csv", "figure-1's file is not carried over");
}

#[test]
fn switching_documents_asks_first_and_then_drops_the_list_for_the_new_document() {
    let mut harness = Harness::new();
    let paper = Evidence::new("paper.pdf", b"%PDF-1.4");
    harness.app.add_figure_ai_attachment(paper.path.clone()).expect("the PDF joins");

    harness.app.activate(figure_summary("figure-2", 1));
    assert_eq!(harness.app.active.as_ref().map(|active| active.id.as_str()), Some(FIGURE), "the switch waits for the answer");
    assert!(harness.app.pending_switch.is_some());
    assert_eq!(harness.app.figure_ai_files().len(), 1, "nothing is dropped before the answer");

    // The answer 전환 drops the files that were not sent and shows the other figure.
    harness.app.discard_figure_work();
    let next = harness.app.pending_switch.take().expect("a switch was asked");
    harness.app.show_artifact(next);
    assert_eq!(harness.app.active.as_ref().map(|active| active.id.as_str()), Some("figure-2"));
    assert!(harness.app.figure_ai_attachments.is_none(), "the list is not carried over");
    assert!(harness.app.figure_ai_files().is_empty());
    assert_eq!(std::fs::read(&paper.path).expect("the PDF is still there"), b"%PDF-1.4", "the evidence is only read");
}

#[test]
fn switching_without_unsent_files_is_not_asked_about() {
    let mut harness = Harness::new();
    harness.app.activate(figure_summary("figure-2", 1));
    assert_eq!(harness.app.active.as_ref().map(|active| active.id.as_str()), Some("figure-2"));
    assert!(harness.app.pending_switch.is_none());
}

#[test]
fn the_request_control_is_off_and_refuses_while_the_evidence_is_imported() {
    // Regression: the send control stayed on during the imports, and a click on it was refused without a word.
    let mut harness = Harness::new();
    let paper = Evidence::new("paper.pdf", b"%PDF-1.4");
    harness.app.add_figure_ai_attachment(paper.path.clone()).expect("the PDF joins");
    harness.request();
    let import = harness.sent();
    assert_eq!(methods(&import), ["asset.import"]);

    assert!(!harness.app.can_start_ai_job(), "the control is off while the import runs");
    let reason = "근거 자료를 준비하는 중입니다. 끝나거나 취소한 뒤 다시 요청하세요";
    assert_eq!(harness.app.ai_request_block(PROMPT), Some(reason));
    assert_eq!(harness.app.invoke("ai.request", json!({ "prompt": PROMPT })), Err(reason.into()));
    assert_eq!(harness.app.invoke("ai.retry", json!({})), Err(reason.into()));
    assert!(harness.sent().is_empty(), "no second import and no job start while the first import runs");
}

#[test]
fn a_request_with_evidence_works_on_the_selected_panel_of_the_figure() {
    // The evidence is imported first; the job then carries the selected panel as its target, and the imported file as its source.
    let mut host = Host::start();
    let figure = drawn_figure(&mut host, "그림", 1);
    let (document, _) = host.figure(&figure);
    let panel = document.panels[0].id.clone();
    let mut window = Window::open(&mut host, &figure);
    window.app.ai_model = "mock-model".into();
    window.step(&mut host, "select.node", json!({ "node_id": panel })).expect("the panel is selected");
    let evidence = Evidence::new("method.md", b"# method\n");
    window.app.add_figure_ai_attachment(evidence.path.clone()).expect("the file joins the list");

    window.step(&mut host, "ai.request", json!({ "prompt": PROMPT })).expect("the request is sent");
    let job = window.app.job.clone().expect("the job the request started is shown");
    assert_eq!(job.target_ids, [panel.as_str()], "the job works on the selected panel");
    assert_eq!(job.artifact_id, figure);
}
