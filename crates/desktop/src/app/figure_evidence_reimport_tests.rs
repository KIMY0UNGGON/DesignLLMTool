//! A PDF page read added to a figure that may hold some of its records already (`evidence_import` and
//! `try_put_held_pdf_evidence` in `figure_tools`). A record the figure holds word for word is not sent again, so its review
//! state stays. A record with the ID of a held one but other content holds the whole read, and the figure keeps what it
//! holds. Records that are not in their figure yet are held until they are retried, copied or discarded: a refused commit,
//! a change on its way, a figure that is not shown at the revision they were read at, and a reply that comes back after
//! the window left the figure. A reply that does not show its page as read safely adds nothing. The window's handler runs
//! against a host the test plays. One test reads a real page twice through the real reader and the real figure engine.

use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    sync::mpsc::{Receiver, Sender},
    time::Instant,
};

use designllm_contracts::{
    Artifact, ArtifactBody, ArtifactKind, ArtifactSummary, ContextSnapshot, Diagnostic, DltError, ErrorCode, Evidence,
    FigureDocument, Intent, Operation, PdfBox, PdfPageGeometry, Rect, Size,
};
use eframe::egui::Context;
use serde_json::{json, Value};

use super::super::{figure_flow_tests::Host, DatasetCellEdit, DesktopApp, HeldPdfEvidence, PdfEvidenceTarget, PendingAction};
use super::{
    conflict_message, evidence_additions, evidence_import, held_pdf_copy_text, page_outcome, review_change, review_target,
    PageOutcome, PDF_HELD_DATASET_EDIT, PDF_HELD_MOVED_ON, PDF_IMPORT_MOVED_ON, PDF_SOURCE_MISMATCH,
};
use crate::{
    notice::Severity,
    rpc::{RpcPayload, RpcReply, RpcWorker, Work},
};

type Request = (u64, String, Value);

const FIGURE: &str = "figure-1";
const REVISION: u64 = 7;
/// The physical page the window asks for when the user types page 12 (physical pages count from 0).
const PAGE: u32 = 11;
/// The import target of the page: the figure at its revision, the imported file (whose hash `read_record` gives its records),
/// and the page. Nothing is awaited yet.
fn pdf_target() -> PdfEvidenceTarget {
    PdfEvidenceTarget {
        artifact_id: FIGURE.into(),
        revision: REVISION,
        page_index: PAGE,
        file_name: "paper.pdf".into(),
        asset_sha256: Some("a".repeat(64)),
        awaiting: None,
    }
}

/// The same import target, waiting for the reply of the request `ticket`.
fn pdf_target_awaiting(ticket: u64) -> PdfEvidenceTarget {
    PdfEvidenceTarget { awaiting: Some(ticket), ..pdf_target() }
}

/// The ticket the import waits for in the tests that hand it replies directly, and the ticket of an import that was replaced.
const AWAITED: u64 = 90;
const REPLACED: u64 = 89;

fn content() -> BTreeSet<Intent> {
    [Intent::Content].into_iter().collect()
}

/// The repository root as a plain absolute path (the host refuses `..` components in import paths).
fn repository_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("crates/desktop sits two levels below the repository root")
        .to_path_buf()
}

fn geometry() -> PdfPageGeometry {
    let page = PdfBox { x_min: 0, y_min: 0, x_max: 7_772_400, y_max: 10_058_400 };
    PdfPageGeometry {
        media_box: page.clone(),
        crop_box: page,
        user_unit: "1".into(),
        rotation_degrees: 0,
        printed_page_label: Some("12".into()),
    }
}

/// One line of physical page `page` as the PDF reader returns it, with the given review state.
fn read_record(id: &str, page: u32, text: &str, review_state: &str) -> Evidence {
    Evidence {
        id: id.into(),
        source_hash: "a".repeat(64),
        origin: "paper_evidence".into(),
        physical_page_index: Some(page),
        region: Some(Rect { x: 914_400, y: 1_828_800, width: 4_572_000, height: 182_880 }),
        pdf_geometry: Some(geometry()),
        text: text.into(),
        review_state: review_state.into(),
    }
}

fn figure_holding(evidence: Vec<Evidence>) -> FigureDocument {
    FigureDocument { evidence, ..FigureDocument::new(Size::a4()) }
}

/// `record` with one field changed, so that a case differs in exactly that field.
fn changed(record: &Evidence, change: impl FnOnce(&mut Evidence)) -> Evidence {
    let mut next = record.clone();
    change(&mut next);
    next
}

/// The IDs of the records a change puts, in order. Any other operation fails the test.
fn put_ids(operations: &[Operation]) -> Vec<String> {
    operations
        .iter()
        .map(|operation| match operation {
            Operation::PutEvidence { evidence } => evidence.id.clone(),
            other => panic!("only evidence records are sent, found {other:?}"),
        })
        .collect()
}

#[test]
fn a_fresh_read_of_a_reviewed_record_is_the_same_reading_and_is_not_sent_again() {
    let held = read_record("pdf-p11-l000", PAGE, "A activates B", "reviewed");
    let read = read_record("pdf-p11-l000", PAGE, "A activates B", "unreviewed");
    let import = evidence_import(&figure_holding(vec![held]), vec![read]);
    assert!(import.additions.is_empty());
    assert_eq!(import.unchanged, 1);
    assert!(import.conflicts.is_empty());
}

#[test]
fn any_other_content_under_a_held_id_is_a_conflict_whichever_field_differs() {
    let held = read_record("pdf-p11-l000", PAGE, "A activates B", "reviewed");
    let variants = [
        changed(&held, |record| record.source_hash = "b".repeat(64)),
        changed(&held, |record| record.origin = "user_statement".into()),
        changed(&held, |record| record.physical_page_index = Some(12)),
        changed(&held, |record| record.region = Some(Rect { x: 0, y: 0, width: 1, height: 1 })),
        changed(&held, |record| record.pdf_geometry.as_mut().expect("geometry").crop_box.x_max += 1),
        changed(&held, |record| record.pdf_geometry = None),
        changed(&held, |record| record.text.push('.')),
    ];
    for read in variants {
        let import = evidence_import(&figure_holding(vec![held.clone()]), vec![read]);
        assert_eq!(import.conflicts, vec!["pdf-p11-l000".to_string()]);
        assert!(import.additions.is_empty());
        assert_eq!(import.unchanged, 0);
    }
}

#[test]
fn a_user_statement_is_never_replaced_by_a_read_with_the_same_id() {
    let statement = Evidence {
        id: "pdf-p11-l000".into(),
        source_hash: "c".repeat(64),
        origin: "user_statement".into(),
        physical_page_index: None,
        region: None,
        pdf_geometry: None,
        text: "사용자가 적은 근거".into(),
        review_state: "user_provided".into(),
    };
    let read = read_record("pdf-p11-l000", PAGE, "A activates B", "unreviewed");
    let import = evidence_import(&figure_holding(vec![statement]), vec![read]);
    assert_eq!(import.conflicts, vec!["pdf-p11-l000".to_string()]);
    assert!(import.additions.is_empty());
}

#[test]
fn only_the_records_the_figure_lacks_are_sent() {
    let held = read_record("pdf-p11-l000", PAGE, "A activates B", "reviewed");
    let read = vec![
        read_record("pdf-p11-l000", PAGE, "A activates B", "unreviewed"),
        read_record("pdf-p11-l001", PAGE, "B activates C", "unreviewed"),
    ];
    let import = evidence_import(&figure_holding(vec![held]), read);
    assert_eq!(put_ids(&import.additions), vec!["pdf-p11-l001".to_string()]);
    assert_eq!(import.unchanged, 1);
    assert!(import.conflicts.is_empty());
}

#[test]
fn a_read_of_another_page_leaves_the_held_page_alone() {
    let held = read_record("pdf-p11-l000", PAGE, "A activates B", "reviewed");
    let read = vec![read_record("pdf-p12-l000", 12, "A activates B", "unreviewed")];
    let import = evidence_import(&figure_holding(vec![held]), read);
    assert_eq!(put_ids(&import.additions), vec!["pdf-p12-l000".to_string()]);
    assert_eq!(import.unchanged, 0);
    assert!(import.conflicts.is_empty());
}

#[test]
fn a_record_repeated_in_one_read_is_sent_once_and_a_different_repeat_is_a_conflict() {
    let figure = figure_holding(Vec::new());
    let first = read_record("pdf-p11-l000", PAGE, "A activates B", "unreviewed");
    let import = evidence_import(&figure, vec![first.clone(), first.clone()]);
    assert_eq!(put_ids(&import.additions), vec!["pdf-p11-l000".to_string()]);
    assert_eq!(import.unchanged, 1);
    assert!(import.conflicts.is_empty());

    let other = read_record("pdf-p11-l000", PAGE, "A activates D", "unreviewed");
    let import = evidence_import(&figure, vec![first, other]);
    assert_eq!(import.conflicts, vec!["pdf-p11-l000".to_string()]);
}

/// The page the window asks for, read through the real reader.
fn read_page_12(host: &mut Host, asset: &Value) -> Vec<Evidence> {
    let page = host
        .call("source.extract_pdf", json!({ "sha256": asset["sha256"], "pages": [PAGE] }))
        .expect("page 12 has text");
    serde_json::from_value(page["evidence"].clone()).expect("evidence")
}

#[test]
fn a_second_read_of_a_page_by_the_real_reader_is_the_same_reading_and_keeps_the_mark() {
    let mut host = Host::start();
    let figure_id = host.create("figure", "그림");
    let pdf = repository_root().join("fixtures/pdf/evidence-p12.pdf");
    let asset = host
        .call("asset.import", json!({ "path": pdf.to_string_lossy() }))
        .expect("import the PDF");
    let first = read_page_12(&mut host, &asset);
    assert!(!first.is_empty());
    host.apply(&figure_id, evidence_additions(first.clone()), content())
        .expect("the figure accepts the page's text");

    let (figure, _) = host.figure(&figure_id);
    let mark = review_change(&figure, &first[0].id, true).expect("an unreviewed read can be marked");
    host.apply(&figure_id, vec![mark], content()).expect("the host accepts the mark");

    let (figure, _) = host.figure(&figure_id);
    let second = read_page_12(&mut host, &asset);
    let import = evidence_import(&figure, second);
    assert!(import.additions.is_empty(), "the reader gives the same records again");
    assert!(import.conflicts.is_empty());
    assert_eq!(import.unchanged, first.len());
    let stored = figure
        .evidence
        .iter()
        .find(|record| record.id == first[0].id)
        .expect("the record is in the figure");
    assert_eq!(review_target(stored), Some(false), "the mark is still there");
}

/// The window with one open figure at `REVISION`, already holding `held`. The host is played by the test.
struct Window {
    app: DesktopApp,
    replies: Sender<RpcReply>,
    work: Receiver<Work>,
    ctx: Context,
}

impl Window {
    fn open_figure(held: Vec<Evidence>) -> Self {
        let (rpc, replies, work) = RpcWorker::detached();
        let mut app = DesktopApp::new(rpc, "그림 근거".into(), None, None);
        while work.try_recv().is_ok() {}
        app.active = Some(ArtifactSummary {
            id: FIGURE.into(),
            kind: ArtifactKind::Figure,
            title: "그림".into(),
            revision: REVISION,
            engine_id: "figure".into(),
            dirty: false,
            modified_at_ms: None,
            active_jobs: Vec::new(),
        });
        app.context = Some(context(REVISION, &app.view_id));
        let mut artifact = Artifact::new(
            ArtifactKind::Figure,
            "그림",
            "figure",
            ArtifactBody::Figure(figure_holding(held)),
        );
        artifact.id = FIGURE.into();
        artifact.revision = REVISION;
        app.artifact = Some(artifact);
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

    /// Answers the newest request for `method` with `value` and lets the window handle the reply.
    fn answer(&mut self, requests: &[Request], method: &str, value: Value) {
        let ticket = requests
            .iter()
            .rev()
            .find(|request| request.1 == method)
            .map(|request| request.0)
            .expect("the window sent the request");
        self.replies
            .send(RpcReply { ticket, result: Ok(RpcPayload::Json(value)) })
            .expect("the window still listens");
        self.app.process_replies(&self.ctx);
    }

    /// Answers the newest request for `method` with `error` and lets the window handle the reply.
    fn refuse(&mut self, requests: &[Request], method: &str, error: DltError) {
        let ticket = requests
            .iter()
            .rev()
            .find(|request| request.1 == method)
            .map(|request| request.0)
            .expect("the window sent the request");
        self.replies
            .send(RpcReply { ticket, result: Err(error) })
            .expect("the window still listens");
        self.app.process_replies(&self.ctx);
    }

    /// The window reads page 12 and the reader returns `read`. The import has captured the figure on screen first, as
    /// `import_pdf_evidence` does before it starts.
    fn import_page(&mut self, read: Vec<Evidence>) {
        let ticket = self
            .app
            .call("source.extract_pdf", json!({ "pages": [PAGE] }), PendingAction::PdfEvidenceImport)
            .expect("the read is sent");
        self.app.pdf_evidence_target = Some(pdf_target_awaiting(ticket));
        let requests = self.sent();
        self.answer(&requests, "source.extract_pdf", json!({ "evidence": read }));
    }

    /// The window reads physical page `page` of the figure at the revision on screen and the reader answers `reply`, as
    /// `import_pdf_evidence` starts the read.
    fn read_page(&mut self, page: u32, reply: Value) {
        let ticket = self
            .app
            .call("source.extract_pdf", json!({ "pages": [page] }), PendingAction::PdfEvidenceImport)
            .expect("the read is sent");
        let revision = self.app.artifact.as_ref().expect("a figure is open").revision;
        self.app.pdf_evidence_target =
            Some(PdfEvidenceTarget { page_index: page, revision, ..pdf_target_awaiting(ticket) });
        let requests = self.sent();
        self.answer(&requests, "source.extract_pdf", reply);
    }

    fn message(&self) -> Option<(Severity, String)> {
        self.app
            .notices
            .current(Instant::now())
            .map(|notice| (notice.severity, notice.text.clone()))
    }

    /// The records the figure on screen holds.
    fn evidence(&self) -> Vec<Evidence> {
        match self.app.artifact.as_ref().map(|artifact| &artifact.body) {
            Some(ArtifactBody::Figure(figure)) => figure.evidence.clone(),
            _ => panic!("the figure is open"),
        }
    }
}

/// A reply of `source.extract_pdf` for physical page `page`: the page count the reader reports (`None` when it reports none),
/// the pages it read, whether it read `page`, and the records of `page`.
fn reading(page: u32, page_count: Option<u32>, read_pages: &[u32], read: bool, evidence: Vec<Evidence>) -> Value {
    let evidence_count = evidence.len();
    json!({
        "evidence": evidence,
        "page_count": page_count,
        "read_pages": read_pages,
        "page_reports": [{
            "physical_page_index": page,
            "read": read,
            "evidence_count": evidence_count,
            "diagnostics": [],
        }],
    })
}

/// The figure on screen moves on to `revision`, as a refresh after a change shows it: the window, the open document and the
/// read context all name that revision.
fn move_figure_to(window: &mut Window, revision: u64) {
    window.app.active.as_mut().expect("a figure is open").revision = revision;
    window.app.artifact.as_mut().expect("a figure is open").revision = revision;
    window.app.context.as_mut().expect("a figure is open").revision = revision;
}

/// The window imports a PDF file with the hash `sha256` for the page of the import, and the reply of `asset.import` arrives.
fn import_asset(window: &mut Window, sha256: String) {
    let ticket = window
        .app
        .call("asset.import", json!({ "path": "paper.pdf" }), PendingAction::PdfAssetImport { page_index: PAGE })
        .expect("the import is sent");
    window.app.pdf_evidence_target = Some(PdfEvidenceTarget { awaiting: Some(ticket), ..pdf_target() });
    window.app.pdf_asset_imported(ticket, PAGE, json!({ "sha256": sha256, "mime": "application/pdf", "bytes": 8 }));
}

fn context(revision: u64, view_id: &str) -> ContextSnapshot {
    serde_json::from_value(json!({
        "workspace_id": "w1",
        "artifact_id": FIGURE,
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

#[test]
fn a_page_the_figure_already_holds_sends_nothing_and_keeps_its_marks() {
    let mut window = Window::open_figure(vec![read_record("pdf-p11-l000", PAGE, "A activates B", "reviewed")]);
    window.import_page(vec![read_record("pdf-p11-l000", PAGE, "A activates B", "unreviewed")]);
    assert!(
        window.sent().iter().all(|request| request.1 != "changes.apply"),
        "nothing is sent for a page the figure already holds"
    );
    assert_eq!(window.message(), Some((Severity::Info, "이미 넣은 근거 1개는 그대로 두었습니다".to_owned())));
    let evidence = window.evidence();
    assert_eq!(evidence.len(), 1);
    assert_eq!(evidence[0].review_state, "reviewed");
}

#[test]
fn a_read_that_conflicts_with_a_held_record_is_refused_as_a_whole() {
    let mut window = Window::open_figure(vec![read_record("pdf-p11-l000", PAGE, "A activates B", "reviewed")]);
    window.import_page(vec![
        read_record("pdf-p11-l000", PAGE, "A activates D", "unreviewed"),
        read_record("pdf-p11-l001", PAGE, "B activates C", "unreviewed"),
    ]);
    assert!(
        window.sent().iter().all(|request| request.1 != "changes.apply"),
        "the new line is not sent either"
    );
    assert_eq!(window.message(), Some((Severity::Error, conflict_message(1))));
    let evidence = window.evidence();
    assert_eq!(evidence.len(), 1, "the figure keeps only what it held");
    assert_eq!((evidence[0].text.as_str(), evidence[0].review_state.as_str()), ("A activates B", "reviewed"));
    assert_eq!(window.app.held_pdf_evidence.len(), 1, "the read is held, not dropped");
}

#[test]
fn a_page_the_figure_lacks_is_one_change_against_the_revision_it_was_read_at() {
    let mut window = Window::open_figure(vec![read_record("pdf-p11-l000", PAGE, "A activates B", "reviewed")]);
    window.import_page(vec![
        read_record("pdf-p11-l000", PAGE, "A activates B", "unreviewed"),
        read_record("pdf-p11-l001", PAGE, "C activates D", "unreviewed"),
    ]);
    let sent = window.sent();
    assert_eq!(sent.len(), 1, "one change set");
    let change = &sent[0];
    assert_eq!(change.1, "changes.apply");
    assert_eq!(change.2["artifact_id"], FIGURE);
    assert_eq!(change.2["base_revision"], REVISION);
    assert_eq!(change.2["allowed_intents"], json!(["content"]));
    assert_eq!(change.2["operations"].as_array().map(Vec::len), Some(1));
    assert_eq!(change.2["operations"][0]["op"], "put_evidence");
    assert_eq!(change.2["operations"][0]["evidence"]["id"], "pdf-p11-l001");
}

#[test]
fn an_open_cell_edit_keeps_its_draft_and_nothing_is_sent_while_it_is_open() {
    let mut window = Window::open_figure(Vec::new());
    window.app.dataset_edit = Some(DatasetCellEdit {
        dataset_id: "dataset-1".into(),
        version_hash: "v1".into(),
        row_id: "row-1".into(),
        column_id: "column-1".into(),
        original: "0.10".into(),
        draft: "0.25".into(),
    });
    window.import_page(vec![read_record("pdf-p11-l000", PAGE, "C activates D", "unreviewed")]);
    assert!(
        window.sent().iter().all(|request| request.1 != "changes.apply"),
        "a change would end the cell edit"
    );
    assert_eq!(window.app.dataset_edit.as_ref().map(|edit| edit.draft.as_str()), Some("0.25"));
    assert_eq!(window.message(), Some((Severity::Warning, PDF_HELD_DATASET_EDIT.to_owned())));
    assert!(window.evidence().is_empty());
    assert_eq!(window.app.held_pdf_evidence.len(), 1, "the read is held until the edit closes");
}

#[test]
fn a_read_that_arrives_after_the_window_moved_to_another_document_is_not_written_to_it() {
    let mut window = Window::open_figure(Vec::new());
    window.app.pdf_evidence_target = Some(pdf_target_awaiting(AWAITED));
    window.app.active.as_mut().expect("a figure is open").id = "figure-2".into();
    window.app.pdf_evidence_read(AWAITED, &json!({ "evidence": [read_record("pdf-p11-l000", PAGE, "A activates B", "unreviewed")] }));
    assert!(window.sent().iter().all(|request| request.1 != "changes.apply"), "nothing is written to the figure on screen");
    assert!(window.evidence().is_empty());
    assert_eq!(window.message(), Some((Severity::Warning, PDF_HELD_MOVED_ON.to_owned())));
    assert!(window.app.pdf_evidence_target.is_none(), "the import is over");
    assert_eq!(window.app.held_pdf_evidence.len(), 1, "the read is held for the figure it was started for");
}

#[test]
fn a_figure_that_changed_since_the_import_started_gets_no_records_and_the_window_says_so() {
    let mut window = Window::open_figure(Vec::new());
    window.app.pdf_evidence_target = Some(pdf_target_awaiting(AWAITED));
    // A commit moved the figure on while the page was being read.
    window.app.active.as_mut().expect("a figure is open").revision = REVISION + 1;
    window.app.artifact.as_mut().expect("a figure is open").revision = REVISION + 1;
    window.app.pdf_evidence_read(AWAITED, &json!({ "evidence": [read_record("pdf-p11-l000", PAGE, "A activates B", "unreviewed")] }));
    assert!(window.sent().iter().all(|request| request.1 != "changes.apply"), "nothing is sent against the newer revision");
    assert!(window.evidence().is_empty());
    assert_eq!(window.message(), Some((Severity::Warning, PDF_HELD_MOVED_ON.to_owned())));
    assert_eq!(window.app.held_pdf_evidence.len(), 1, "the read is held, not dropped");
}

#[test]
fn a_file_imported_for_a_document_the_window_has_left_is_not_read() {
    let mut window = Window::open_figure(Vec::new());
    window.app.pdf_evidence_target = Some(pdf_target_awaiting(AWAITED));
    window.app.active.as_mut().expect("a figure is open").id = "figure-2".into();
    window.app.pdf_asset_imported(AWAITED, PAGE, json!({ "sha256": "a".repeat(64), "mime": "application/pdf", "bytes": 8 }));
    assert!(window.sent().is_empty(), "no page is read from the file");
    assert_eq!(window.message(), Some((Severity::Warning, PDF_IMPORT_MOVED_ON.to_owned())));
}

#[test]
fn a_cell_draft_started_while_the_evidence_commit_is_on_its_way_survives_its_success() {
    let mut window = Window::open_figure(Vec::new());
    window.import_page(vec![read_record("pdf-p11-l000", PAGE, "C activates D", "unreviewed")]);
    let commit = window.sent();
    assert_eq!(commit.iter().filter(|request| request.1 == "changes.apply").count(), 1, "the page's record is sent once");
    window.app.dataset_edit = Some(DatasetCellEdit {
        dataset_id: "dataset-1".into(),
        version_hash: "v1".into(),
        row_id: "row-1".into(),
        column_id: "column-1".into(),
        original: "0.10".into(),
        draft: "0.25".into(),
    });
    window.answer(&commit, "changes.apply", json!({ "revision": REVISION + 1, "changed_ids": [] }));
    assert_eq!(window.app.dataset_edit.as_ref().map(|edit| edit.draft.as_str()), Some("0.25"));
}

/// A reply of `source.extract_pdf` that carries `page_reports`: one page, read or not, with its diagnostics. `read_pages`
/// names the page when it was read, as the page reports do.
fn reply_with_report(page: u32, read: bool, evidence_count: u32, diagnostics: Value, evidence: Vec<Evidence>) -> Value {
    let read_pages = if read { vec![page] } else { Vec::new() };
    json!({
        "evidence": evidence,
        "read_pages": read_pages,
        "page_reports": [{
            "physical_page_index": page,
            "read": read,
            "evidence_count": evidence_count,
            "diagnostics": diagnostics,
        }],
    })
}

#[test]
fn the_page_report_decides_whether_the_reply_is_read_safely() {
    let record = read_record("pdf-p11-l000", PAGE, "A activates B", "unreviewed");
    assert_eq!(page_outcome(&json!({ "evidence": [] }), PAGE, 0), PageOutcome::Legacy, "a reply without page_reports reads as before");
    assert_eq!(page_outcome(&reply_with_report(PAGE, true, 1, json!([]), vec![record.clone()]), PAGE, 1), PageOutcome::Read);
    assert_eq!(page_outcome(&reply_with_report(PAGE, true, 0, json!([]), Vec::new()), PAGE, 0), PageOutcome::NoText);
    assert_eq!(
        page_outcome(&reply_with_report(PAGE, true, 1, json!([]), vec![record]), PAGE, 0),
        PageOutcome::Unsafe(Vec::new()),
        "records the report does not count are not read"
    );
    assert_eq!(
        page_outcome(&reply_with_report(PAGE, false, 0, json!([{ "code": "PDF_X", "message": "stopped" }]), Vec::new()), PAGE, 0),
        PageOutcome::Unsafe(vec!["PDF_X: stopped".to_owned()])
    );
    assert_eq!(
        page_outcome(&reply_with_report(12, true, 0, json!([]), Vec::new()), PAGE, 0),
        PageOutcome::Unsafe(Vec::new()),
        "a reply that does not account for the requested page is not safe"
    );
}

#[test]
fn a_page_the_reader_did_not_read_safely_adds_nothing_and_names_the_diagnostic() {
    let mut window = Window::open_figure(Vec::new());
    window.app.pdf_evidence_target = Some(pdf_target_awaiting(AWAITED));
    let reply = reply_with_report(PAGE, false, 0, json!([{ "code": "PDF_INTERNAL_PANIC", "message": "the parser stopped" }]), Vec::new());
    window.app.pdf_evidence_read(AWAITED, &reply);
    assert!(window.sent().iter().all(|request| request.1 != "changes.apply"), "nothing is sent for a page that was not read");
    assert!(window.evidence().is_empty());
    assert!(window.app.held_pdf_evidence.is_empty(), "there is no reading to keep");
    assert!(window.app.pdf_evidence_target.is_none(), "the import is over");
    assert_eq!(
        window.message(),
        Some((
            Severity::Error,
            "12쪽을 안전하게 읽지 못해 PDF 근거를 넣지 않았습니다. 진단: PDF_INTERNAL_PANIC: the parser stopped".to_owned()
        ))
    );
}

#[test]
fn a_page_read_without_text_says_so_and_adds_nothing() {
    let mut window = Window::open_figure(Vec::new());
    window.app.pdf_evidence_target = Some(pdf_target_awaiting(AWAITED));
    window.app.pdf_evidence_read(AWAITED, &reply_with_report(PAGE, true, 0, json!([]), Vec::new()));
    assert!(window.sent().iter().all(|request| request.1 != "changes.apply"), "nothing is sent for an empty page");
    assert!(window.evidence().is_empty());
    assert!(window.app.held_pdf_evidence.is_empty());
    assert_eq!(
        window.message(),
        Some((Severity::Error, "12쪽에는 글자가 없어 PDF 근거를 넣지 않았습니다".to_owned()))
    );
}

#[test]
fn a_read_that_arrives_while_a_change_is_on_its_way_is_held_and_goes_in_on_retry() {
    let mut window = Window::open_figure(Vec::new());
    let _ = window.app.call("changes.apply", json!({ "artifact_id": FIGURE }), PendingAction::Apply);
    window.sent();
    window.app.pdf_evidence_target = Some(pdf_target_awaiting(AWAITED));
    window.app.pdf_evidence_read(AWAITED, &json!({ "evidence": [read_record("pdf-p11-l000", PAGE, "A activates B", "unreviewed")] }));
    assert!(window.sent().is_empty(), "nothing is sent while the change is on its way");
    assert!(window.evidence().is_empty());
    assert_eq!(window.app.held_pdf_evidence.len(), 1, "the records are held");
    let id = window.app.held_pdf_evidence[0].id.clone();

    // The change's answer is not part of this test: the retry may send the records once the window waits for nothing.
    window.app.pending.clear();
    window.app.retry_held_pdf_evidence(&id);
    let sent = window.sent();
    assert_eq!(sent.len(), 1, "one change set");
    assert_eq!(sent[0].1, "changes.apply");
    assert_eq!(sent[0].2["base_revision"], REVISION);
    assert_eq!(sent[0].2["operations"][0]["evidence"]["id"], "pdf-p11-l000");
    assert_eq!(window.app.held_pdf_evidence[0].ticket, Some(sent[0].0), "the set waits for this commit");

    window.answer(&sent, "changes.apply", json!({ "revision": REVISION + 1, "changed_ids": [] }));
    assert!(window.app.held_pdf_evidence.is_empty(), "the commit landed, so nothing is held");
}

#[test]
fn a_commit_the_host_refuses_keeps_its_records_held_and_retry_sends_them_again() {
    let mut window = Window::open_figure(Vec::new());
    window.import_page(vec![read_record("pdf-p11-l000", PAGE, "A activates B", "unreviewed")]);
    let commit = window.sent();
    assert_eq!(commit.iter().filter(|request| request.1 == "changes.apply").count(), 1, "the page's record is sent once");
    window.refuse(&commit, "changes.apply", DltError::new(ErrorCode::RevisionConflict, "a third view committed first"));
    assert!(window.evidence().is_empty(), "the host refused the commit, so the figure holds nothing new");
    assert_eq!(window.app.held_pdf_evidence.len(), 1, "the records are held");
    assert!(window.app.held_pdf_evidence[0].ticket.is_none());
    assert!(window.app.held_pdf_evidence[0].message.contains("보류"));

    // The refusal asks the window to read the figure again; those reads are not answered here.
    window.app.pending.clear();
    let id = window.app.held_pdf_evidence[0].id.clone();
    window.app.retry_held_pdf_evidence(&id);
    let again: Vec<Request> = window.sent().into_iter().filter(|request| request.1 == "changes.apply").collect();
    assert_eq!(again.len(), 1, "the records are sent once more");
    assert_eq!(again[0].2["base_revision"], REVISION);
}

#[test]
fn a_page_read_that_returns_after_the_window_left_the_figure_is_held_for_that_figure() {
    let mut window = Window::open_figure(Vec::new());
    let ticket = window.app.call("source.extract_pdf", json!({ "pages": [PAGE] }), PendingAction::PdfEvidenceImport).expect("the read is sent");
    window.app.pdf_evidence_target = Some(pdf_target_awaiting(ticket));
    let requests = window.sent();
    window.app.active.as_mut().expect("a figure is open").id = "figure-2".into();
    window.answer(
        &requests,
        "source.extract_pdf",
        json!({ "evidence": [read_record("pdf-p11-l000", PAGE, "A activates B", "unreviewed")] }),
    );
    assert!(window.sent().iter().all(|request| request.1 != "changes.apply"), "nothing is written to the document on screen");
    assert!(window.evidence().is_empty());
    assert_eq!(window.app.held_pdf_evidence.len(), 1, "the read is held for the figure it was started for");
    assert_eq!(window.app.held_pdf_evidence[0].target.artifact_id, FIGURE);
    assert_eq!(window.message(), Some((Severity::Warning, PDF_HELD_MOVED_ON.to_owned())));
}

#[test]
fn a_refused_page_read_names_the_page_and_the_reader_diagnostic() {
    let mut window = Window::open_figure(Vec::new());
    let ticket = window.app.call("source.extract_pdf", json!({ "pages": [PAGE] }), PendingAction::PdfEvidenceImport).expect("the read is sent");
    window.app.pdf_evidence_target = Some(pdf_target_awaiting(ticket));
    let requests = window.sent();
    // The core refuses a request whose page the reader left unread, and names the page in `affected_ids`.
    let mut refusal = DltError::new(ErrorCode::Internal, "the PDF reader left 1 page(s) unread without failing the request");
    refusal.affected_ids = vec![PAGE.to_string()];
    refusal.diagnostics = vec![Diagnostic {
        code: "PDF_INTERNAL_PANIC".into(),
        message: "the parser stopped".into(),
        node_id: None,
        blocking: true,
        params: Default::default(),
    }];
    window.refuse(&requests, "source.extract_pdf", refusal);
    assert!(window.sent().iter().all(|request| request.1 != "changes.apply"), "nothing is sent for a refused read");
    assert!(window.evidence().is_empty());
    assert!(window.app.held_pdf_evidence.is_empty(), "a refusal leaves no reading to keep");
    assert!(window.app.pdf_evidence_target.is_none(), "the import is over");
    assert_eq!(
        window.message(),
        Some((
            Severity::Error,
            "12쪽을 안전하게 읽지 못해 PDF 근거를 넣지 않았습니다. 진단: PDF_INTERNAL_PANIC: the parser stopped".to_owned()
        ))
    );
}

#[test]
fn a_read_names_the_page_count_and_the_pages_read_so_far() {
    let mut window = Window::open_figure(Vec::new());
    window.read_page(2, reading(2, Some(12), &[2], true, Vec::new()));
    assert_eq!(window.app.pdf_coverage_text().as_deref(), Some("전체 12쪽 중 읽은 쪽: 3쪽"));
    window.read_page(4, reading(4, Some(12), &[4], true, Vec::new()));
    assert_eq!(window.app.pdf_coverage_text().as_deref(), Some("전체 12쪽 중 읽은 쪽: 3쪽, 5쪽"), "the pages of the earlier reads stay");
}

#[test]
fn a_count_the_reader_does_not_report_is_said_so_and_the_pages_read_are_still_named() {
    let mut window = Window::open_figure(Vec::new());
    window.read_page(2, reading(2, None, &[2], true, Vec::new()));
    assert_eq!(window.app.pdf_coverage_text().as_deref(), Some("전체 쪽 수 모름 · 읽은 쪽: 3쪽"));
}

#[test]
fn a_page_the_window_did_not_read_safely_adds_no_page_and_keeps_the_count() {
    let mut window = Window::open_figure(Vec::new());
    window.read_page(2, reading(2, Some(12), &[], false, Vec::new()));
    assert_eq!(window.app.pdf_coverage_text().as_deref(), Some("전체 12쪽 중 읽은 쪽: 없음"));
}

#[test]
fn a_page_read_without_text_names_its_page_and_the_reason() {
    let mut window = Window::open_figure(Vec::new());
    window.read_page(2, reading(2, Some(12), &[2], true, Vec::new()));
    assert_eq!(
        window.message(),
        Some((Severity::Error, "3쪽에는 글자가 없어 PDF 근거를 넣지 않았습니다".to_owned()))
    );
}

#[test]
fn a_document_whose_last_page_has_no_text_reads_as_complete_and_says_why_the_page_adds_nothing() {
    let mut window = Window::open_figure(Vec::new());
    let record = read_record("pdf-p0-l000", 0, "A activates B", "unreviewed");
    window.read_page(0, reading(0, Some(2), &[0], true, vec![record]));
    window.read_page(1, reading(1, Some(2), &[1], true, Vec::new()));
    assert_eq!(window.app.pdf_coverage_text().as_deref(), Some("전체 읽음 · 전체 2쪽"), "a page without text is read");
    assert_eq!(
        window.message(),
        Some((Severity::Error, "2쪽에는 글자가 없어 PDF 근거를 넣지 않았습니다".to_owned()))
    );
}

#[test]
fn a_page_not_read_safely_keeps_the_document_from_reading_as_complete() {
    let mut window = Window::open_figure(Vec::new());
    window.read_page(0, reading(0, Some(2), &[0], true, Vec::new()));
    window.read_page(1, reading(1, Some(2), &[], false, Vec::new()));
    assert_eq!(window.app.pdf_coverage_text().as_deref(), Some("전체 2쪽 중 읽은 쪽: 1쪽"));
    assert_eq!(
        window.message(),
        Some((Severity::Error, "2쪽을 안전하게 읽지 못해 PDF 근거를 넣지 않았습니다".to_owned()))
    );
}

#[test]
fn a_reading_whose_read_pages_the_request_and_the_page_reports_do_not_back_adds_no_page() {
    // Page 12 of 12 is asked for and reported, but the reading names every page of the document as read.
    let mut every_page = Window::open_figure(Vec::new());
    let all: Vec<u32> = (0..12).collect();
    every_page.read_page(PAGE, reading(PAGE, Some(12), &all, true, Vec::new()));
    assert_eq!(every_page.app.pdf_coverage_text(), None, "no page of the reading is shown as read");
    assert!(every_page.app.pdf_coverage.is_none());
    assert_eq!(
        every_page.message(),
        Some((Severity::Error, "12쪽을 안전하게 읽지 못해 PDF 근거를 넣지 않았습니다".to_owned()))
    );

    // The page report says page 12 was read, but the read pages leave it out.
    let mut none_read = Window::open_figure(Vec::new());
    none_read.read_page(PAGE, reading(PAGE, Some(12), &[], true, Vec::new()));
    assert!(none_read.app.pdf_coverage.is_none());
    assert_eq!(
        none_read.message(),
        Some((Severity::Error, "12쪽을 안전하게 읽지 못해 PDF 근거를 넣지 않았습니다".to_owned()))
    );
}

#[test]
fn the_whole_document_reads_as_complete_only_when_every_page_is_read() {
    let mut window = Window::open_figure(Vec::new());
    window.read_page(0, reading(0, Some(2), &[0], true, Vec::new()));
    assert_eq!(window.app.pdf_coverage_text().as_deref(), Some("전체 2쪽 중 읽은 쪽: 1쪽"));
    window.read_page(1, reading(1, Some(2), &[1], true, Vec::new()));
    assert_eq!(window.app.pdf_coverage_text().as_deref(), Some("전체 읽음 · 전체 2쪽"));

    let mut longer = Window::open_figure(Vec::new());
    longer.read_page(0, reading(0, Some(3), &[0], true, Vec::new()));
    longer.read_page(1, reading(1, Some(3), &[1], true, Vec::new()));
    assert_eq!(longer.app.pdf_coverage_text().as_deref(), Some("전체 3쪽 중 읽은 쪽: 1쪽, 2쪽"), "page 3 is unread");
}

#[test]
fn a_figure_that_moves_on_hides_the_coverage_of_its_earlier_revision_and_a_new_read_starts_over() {
    let mut window = Window::open_figure(Vec::new());
    window.read_page(2, reading(2, Some(12), &[2], true, Vec::new()));
    assert_eq!(window.app.pdf_coverage_text().as_deref(), Some("전체 12쪽 중 읽은 쪽: 3쪽"));
    move_figure_to(&mut window, REVISION + 2);
    assert_eq!(window.app.pdf_coverage_text(), None, "a change that is not this window's own commit ends the coverage");
    window.read_page(4, reading(4, Some(12), &[4], true, Vec::new()));
    assert_eq!(
        window.app.pdf_coverage_text().as_deref(),
        Some("전체 12쪽 중 읽은 쪽: 5쪽"),
        "the pages read at the earlier revision are not added to the new one"
    );
}

#[test]
fn a_file_imported_again_keeps_its_coverage_and_another_file_drops_it() {
    let mut window = Window::open_figure(Vec::new());
    window.read_page(2, reading(2, Some(12), &[2], true, Vec::new()));
    import_asset(&mut window, "a".repeat(64));
    assert_eq!(window.app.pdf_coverage_text().as_deref(), Some("전체 12쪽 중 읽은 쪽: 3쪽"), "the same file");
    import_asset(&mut window, "b".repeat(64));
    assert_eq!(window.app.pdf_coverage_text(), None, "the coverage of the other file is dropped");
    assert!(window.app.pdf_coverage.is_none(), "the coverage itself is gone, not only hidden");
}

#[test]
fn a_reply_the_import_does_not_await_adds_no_coverage() {
    let mut window = Window::open_figure(Vec::new());
    window.read_page(2, reading(2, Some(12), &[2], true, Vec::new()));
    window.app.pdf_evidence_target = Some(pdf_target_awaiting(AWAITED));
    window.app.pdf_evidence_read(REPLACED, &reading(4, Some(12), &[4], true, Vec::new()));
    assert_eq!(window.app.pdf_coverage_text().as_deref(), Some("전체 12쪽 중 읽은 쪽: 3쪽"), "the late reply is not read");
}

#[test]
fn a_reading_that_contradicts_its_count_leaves_the_coverage_as_it_was() {
    let mut window = Window::open_figure(Vec::new());
    window.read_page(2, reading(2, Some(12), &[2], true, Vec::new()));
    window.read_page(4, reading(4, Some(0), &[4], true, Vec::new()));
    assert_eq!(window.app.pdf_coverage_text().as_deref(), Some("전체 12쪽 중 읽은 쪽: 3쪽"));
}

#[test]
fn a_commit_of_the_records_read_keeps_the_coverage_with_the_figure() {
    let mut window = Window::open_figure(Vec::new());
    let record = read_record("pdf-p11-l000", PAGE, "A activates B", "unreviewed");
    window.read_page(PAGE, reading(PAGE, Some(12), &[PAGE], true, vec![record]));
    assert_eq!(window.app.pdf_coverage_text().as_deref(), Some("전체 12쪽 중 읽은 쪽: 12쪽"), "shown while the commit is on its way");
    let commit = window.sent();
    window.answer(&commit, "changes.apply", json!({ "revision": REVISION + 1, "changed_ids": [] }));
    move_figure_to(&mut window, REVISION + 1);
    assert_eq!(window.app.pdf_coverage_text().as_deref(), Some("전체 12쪽 중 읽은 쪽: 12쪽"), "the commit made the next revision of this reading");
}

#[test]
fn a_commit_answered_without_the_next_revision_does_not_carry_the_coverage() {
    for answer in [json!({ "revision": REVISION + 3, "changed_ids": [] }), json!({ "changed_ids": [] })] {
        let mut window = Window::open_figure(Vec::new());
        let record = read_record("pdf-p11-l000", PAGE, "A activates B", "unreviewed");
        window.read_page(PAGE, reading(PAGE, Some(12), &[PAGE], true, vec![record]));
        let commit = window.sent();
        window.answer(&commit, "changes.apply", answer.clone());
        move_figure_to(&mut window, REVISION + 1);
        assert_eq!(window.app.pdf_coverage_text(), None, "{answer}: the commit did not make the next revision of this reading");
    }
}

/// One read whose records do not all come from the imported file and its page: the set is held, not sent, and the copy
/// names the source of each record.
fn mismatched_read(change: impl FnOnce(&mut Evidence)) -> Vec<Evidence> {
    vec![changed(&read_record("pdf-p11-l000", PAGE, "A activates B", "unreviewed"), change)]
}

fn assert_held_for_source(window: &mut Window) {
    assert!(window.sent().iter().all(|request| request.1 != "changes.apply"), "nothing is sent for a record of another source");
    assert!(window.evidence().is_empty());
    assert_eq!(window.app.held_pdf_evidence.len(), 1, "the read is held, so it can be copied or discarded");
    assert_eq!(window.message(), Some((Severity::Error, PDF_SOURCE_MISMATCH.to_owned())));
    let held = window.app.held_pdf_evidence[0].clone();
    assert_eq!(window.app.held_pdf_wait(&held), Some(PDF_SOURCE_MISMATCH), "retry is not offered for it");
    let id = held.id.clone();
    window.app.retry_held_pdf_evidence(&id);
    assert!(window.sent().iter().all(|request| request.1 != "changes.apply"), "a retry sends nothing either");
    assert_eq!(window.app.held_pdf_evidence.len(), 1);
}

#[test]
fn a_record_with_another_file_hash_is_held_with_an_error_and_never_sent() {
    let mut window = Window::open_figure(Vec::new());
    window.import_page(mismatched_read(|record| record.source_hash = "b".repeat(64)));
    assert_held_for_source(&mut window);
}

#[test]
fn a_record_from_another_physical_page_is_held_with_an_error_and_never_sent() {
    let mut window = Window::open_figure(Vec::new());
    window.import_page(mismatched_read(|record| record.physical_page_index = Some(12)));
    assert_held_for_source(&mut window);
}

#[test]
fn a_record_without_a_physical_page_is_held_with_an_error_and_never_sent() {
    let mut window = Window::open_figure(Vec::new());
    window.import_page(mismatched_read(|record| record.physical_page_index = None));
    assert_held_for_source(&mut window);
}

#[test]
fn the_imported_file_hash_is_kept_on_the_target_and_the_page_is_read_from_that_file() {
    let mut window = Window::open_figure(Vec::new());
    let mut target = pdf_target_awaiting(AWAITED);
    target.asset_sha256 = None;
    window.app.pdf_evidence_target = Some(target);
    window.app.pdf_asset_imported(AWAITED, PAGE, json!({ "sha256": "c".repeat(64), "mime": "application/pdf", "bytes": 8 }));
    let requests = window.sent();
    let read = requests
        .iter()
        .find(|request| request.1 == "source.extract_pdf")
        .expect("the page is read from the imported file");
    assert_eq!(read.2["sha256"], "c".repeat(64));
    assert_eq!(read.2["pages"], json!([PAGE]));
    assert_eq!(
        window.app.pdf_evidence_target.as_ref().and_then(|target| target.asset_sha256.clone()),
        Some("c".repeat(64))
    );
}

#[test]
fn a_late_page_read_of_a_replaced_import_holds_nothing_and_leaves_the_import_in_progress() {
    let mut window = Window::open_figure(Vec::new());
    window.app.pdf_evidence_target = Some(pdf_target_awaiting(AWAITED));
    window.app.pdf_evidence_read(REPLACED, &json!({ "evidence": [read_record("pdf-p11-l000", PAGE, "A activates B", "unreviewed")] }));
    assert!(window.app.held_pdf_evidence.is_empty(), "the read of the replaced import is not held");
    assert!(window.app.pdf_evidence_target.is_some(), "the import in progress is still the window's");
    assert!(window.message().is_none(), "no notice is said for the replaced import");
}

#[test]
fn a_late_file_answer_of_a_replaced_import_reads_no_page() {
    let mut window = Window::open_figure(Vec::new());
    let mut target = pdf_target_awaiting(AWAITED);
    target.asset_sha256 = None;
    window.app.pdf_evidence_target = Some(target);
    window.app.pdf_asset_imported(REPLACED, PAGE, json!({ "sha256": "c".repeat(64), "mime": "application/pdf", "bytes": 8 }));
    assert!(window.sent().is_empty(), "no page is read from the file of the replaced import");
    let target = window.app.pdf_evidence_target.as_ref().expect("the import in progress is kept");
    assert_eq!(target.asset_sha256, None);
    assert_eq!(target.awaiting, Some(AWAITED));
}

#[test]
fn a_discarded_pdf_import_holds_no_held_records_back_from_the_figure() {
    // Regression: the import the window had discarded still counted as a wait, so the held records were not put in.
    let mut window = Window::open_figure(Vec::new());
    let _ = window.app.call("asset.import", json!({ "path": "old.pdf" }), PendingAction::PdfAssetImport { page_index: PAGE });
    window.sent();
    window.app.pdf_evidence_target = None;
    window.app.held_pdf_evidence.push(HeldPdfEvidence {
        id: "held-1".into(),
        target: pdf_target(),
        records: vec![read_record("pdf-p11-l000", PAGE, "A activates B", "unreviewed")],
        message: String::new(),
        ticket: None,
    });
    window.app.retry_held_pdf_evidence("held-1");
    let sent = window.sent();
    let commit = sent.iter().find(|request| request.1 == "changes.apply").expect("the records are sent");
    assert_eq!(window.app.held_pdf_evidence[0].ticket, Some(commit.0), "the set waits for this commit");
}

#[test]
fn a_late_failure_or_drop_of_a_replaced_import_neither_ends_nor_announces_the_import() {
    let mut window = Window::open_figure(Vec::new());
    window.app.pdf_evidence_target = Some(pdf_target_awaiting(AWAITED));
    window.app.pdf_evidence_import_failed(&PendingAction::PdfAssetImport { page_index: PAGE }, REPLACED, "the file was moved");
    assert!(window.app.pdf_evidence_target.is_some(), "a failure of the replaced import does not end this one");
    let dropped = Ok(RpcPayload::Json(json!({ "sha256": "c".repeat(64) })));
    window.app.pdf_evidence_reply_dropped(&PendingAction::PdfAssetImport { page_index: PAGE }, REPLACED, &dropped);
    assert!(window.app.pdf_evidence_target.is_some(), "a dropped answer of the replaced import does not end this one");
    assert!(window.message().is_none(), "the moved-on notice is not said for the replaced import");
}

#[test]
fn an_answer_that_comes_through_the_window_for_a_replaced_import_starts_no_page_read() {
    let mut window = Window::open_figure(Vec::new());
    let _ = window.app.call("asset.import", json!({ "path": "old.pdf" }), PendingAction::PdfAssetImport { page_index: PAGE });
    let old = window.sent();
    let awaited = window.app.call("source.extract_pdf", json!({ "pages": [PAGE] }), PendingAction::PdfEvidenceImport);
    window.sent();
    let mut target = pdf_target_awaiting(awaited.expect("the read is sent"));
    target.asset_sha256 = None;
    window.app.pdf_evidence_target = Some(target);

    window.answer(&old, "asset.import", json!({ "sha256": "c".repeat(64), "mime": "application/pdf", "bytes": 8 }));
    assert!(window.sent().iter().all(|request| request.1 != "source.extract_pdf"), "the file of the replaced import is not read");
    assert_eq!(window.app.pdf_evidence_target.as_ref().and_then(|target| target.asset_sha256.clone()), None);
}

#[test]
fn the_copy_of_held_records_names_the_file_the_page_and_each_records_hash() {
    let mut window = Window::open_figure(Vec::new());
    let mut mismatched = read_record("pdf-p11-l001", PAGE, "B activates C", "unreviewed");
    mismatched.source_hash = "b".repeat(64);
    window.import_page(vec![read_record("pdf-p11-l000", PAGE, "A activates B", "unreviewed"), mismatched]);
    assert_eq!(window.app.held_pdf_evidence.len(), 1);
    assert_eq!(
        held_pdf_copy_text(&window.app.held_pdf_evidence[0]),
        format!(
            "A activates B [paper.pdf 12쪽 sha256:{}]\nB activates C [paper.pdf 12쪽 sha256:{}]",
            "a".repeat(64),
            "b".repeat(64)
        )
    );
}

#[test]
fn a_record_on_the_last_page_number_is_copied_without_overflow() {
    // Regression: the copy text added one to the page without saturating.
    let mut record = read_record("pdf-p11-l000", PAGE, "A activates B", "unreviewed");
    record.physical_page_index = Some(u32::MAX);
    let held = HeldPdfEvidence {
        id: "held".into(),
        target: pdf_target(),
        records: vec![record],
        message: String::new(),
        ticket: None,
    };
    assert_eq!(held_pdf_copy_text(&held), format!("A activates B [paper.pdf {}쪽 sha256:{}]", u32::MAX, "a".repeat(64)));
}

/// Turns the window into a smoke window that reports into a fresh folder, before any reply comes, as `run_smoke` does: the
/// window keeps the reader's codes only while it is one.
fn smoke_report_dir(window: &mut Window, name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("designllm-pdf-report-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a test folder");
    window.app.smoke_output = Some(dir.join("smoke.png"));
    dir
}

/// The PDF report the smoke window writes into `dir`, which is removed afterwards.
fn pdf_report(window: &mut Window, dir: &Path) -> Value {
    window.app.write_pdf_report("paper.pdf");
    let text = std::fs::read_to_string(dir.join("smoke.pdf.json")).expect("the report is written");
    let _ = std::fs::remove_dir_all(dir);
    serde_json::from_str(&text).expect("the report is JSON")
}

fn report_codes(report: &Value) -> Vec<&str> {
    report["codes"].as_array().expect("codes").iter().filter_map(Value::as_str).collect()
}

/// A call the host refuses with a sentence that names no code, which takes the window's message.
fn refused_create(window: &mut Window) {
    window.app.call("artifact.create", json!({ "kind": "figure" }), PendingAction::Create).expect("the call is sent");
    let requests = window.sent();
    window.refuse(&requests, "artifact.create", DltError::new(ErrorCode::InvalidArgument, "the create was refused"));
}

/// The core's refusal of a request whose page the reader did not read safely: the page and the reader's diagnostic.
fn page_refusal(code: &str) -> DltError {
    let mut refusal = DltError::new(ErrorCode::UnsupportedOperation, "the PDF could not be read");
    refusal.affected_ids = vec![PAGE.to_string()];
    refusal.diagnostics = vec![Diagnostic {
        code: code.into(),
        message: "the reader stopped".into(),
        node_id: None,
        blocking: true,
        params: Default::default(),
    }];
    refusal
}

/// The smoke report after a refused call, then the PDF request `method` refused with `refusal`, then a later refused call
/// that takes the window's message.
fn report_after_a_pdf_refusal_between_errors(name: &str, method: &str, params: Value, action: PendingAction, refusal: DltError) -> Value {
    let mut window = Window::open_figure(Vec::new());
    let dir = smoke_report_dir(&mut window, name);
    refused_create(&mut window);
    let pdf = window.app.call(method, params, action).expect("the PDF request is sent");
    window.app.pdf_evidence_target = Some(pdf_target_awaiting(pdf));
    let requests = window.sent();
    window.refuse(&requests, method, refusal);
    refused_create(&mut window);
    assert!(
        window.message().is_some_and(|(_, text)| !text.contains("PDF_")),
        "the later error's message names no reader code: {:?}",
        window.message()
    );
    pdf_report(&mut window, &dir)
}

#[test]
fn the_pdf_report_keeps_the_codes_of_a_refused_page_read_between_other_errors() {
    let report = report_after_a_pdf_refusal_between_errors(
        "refused-read",
        "source.extract_pdf",
        json!({ "pages": [PAGE] }),
        PendingAction::PdfEvidenceImport,
        page_refusal("PDF_REQUEST_BUDGET"),
    );
    assert!(report["first_refusal"].as_str().is_some_and(|text| text.starts_with("artifact.create: ")), "the first refusal is the earlier error: {report}");
    assert_eq!(report_codes(&report), vec!["PDF_REQUEST_BUDGET"], "the report names the reader's code: {report}");
}

#[test]
fn the_pdf_report_keeps_the_codes_of_a_refused_pdf_import_between_other_errors() {
    let report = report_after_a_pdf_refusal_between_errors(
        "refused-import",
        "asset.import",
        json!({ "path": "paper.pdf" }),
        PendingAction::PdfAssetImport { page_index: PAGE },
        page_refusal("PDF_STACK_LIMIT"),
    );
    assert_eq!(report_codes(&report), vec!["PDF_STACK_LIMIT"], "the report names the reader's code: {report}");
}

#[test]
fn the_pdf_report_keeps_the_reader_codes_when_a_later_error_takes_the_window_message() {
    // An earlier refused call sets `smoke_failure`, then the reader's unsafe page is shown, then a later call fails and
    // replaces the window's message. The report must still name the reader's code.
    let mut window = Window::open_figure(Vec::new());
    let dir = smoke_report_dir(&mut window, "unsafe-read");
    refused_create(&mut window);
    let pdf = window.app.call("source.extract_pdf", json!({ "pages": [PAGE] }), PendingAction::PdfEvidenceImport).expect("the read is sent");
    window.app.pdf_evidence_target = Some(pdf_target_awaiting(pdf));
    let requests = window.sent();
    let reply = reply_with_report(PAGE, false, 0, json!([{ "code": "PDF_MEMORY_LIMIT", "message": "the reader ran out of memory" }]), Vec::new());
    window.answer(&requests, "source.extract_pdf", reply);
    assert!(window.message().is_some_and(|(_, text)| text.contains("PDF_MEMORY_LIMIT")), "the reader's message is shown first");
    refused_create(&mut window);

    let report = pdf_report(&mut window, &dir);
    assert_eq!(report_codes(&report), vec!["PDF_MEMORY_LIMIT"], "the report names the reader's code: {report}");
}

#[test]
fn the_pdf_report_names_the_reader_code_once_when_the_reading_is_the_first_error() {
    let mut window = Window::open_figure(Vec::new());
    let dir = smoke_report_dir(&mut window, "first");
    let pdf = window.app.call("source.extract_pdf", json!({ "pages": [PAGE] }), PendingAction::PdfEvidenceImport).expect("the read is sent");
    window.app.pdf_evidence_target = Some(pdf_target_awaiting(pdf));
    let requests = window.sent();
    window.answer(
        &requests,
        "source.extract_pdf",
        reply_with_report(PAGE, false, 0, json!([{ "code": "PDF_MEMORY_LIMIT", "message": "the reader ran out of memory" }]), Vec::new()),
    );
    // Both the window's message and the kept codes name it.
    assert!(window.message().is_some_and(|(_, text)| text.contains("PDF_MEMORY_LIMIT")), "the reader's message is shown");
    assert_eq!(window.app.smoke_pdf_codes, vec!["PDF_MEMORY_LIMIT".to_owned()], "the smoke window keeps the reader's code");
    assert_eq!(report_codes(&pdf_report(&mut window, &dir)), vec!["PDF_MEMORY_LIMIT"], "the reader's code is named once");
}

#[test]
fn a_successful_page_read_shows_and_reports_nonblocking_diagnostics() {
    let mut window = Window::open_figure(Vec::new());
    let dir = smoke_report_dir(&mut window, "read-diagnostic");
    let record = read_record("pdf-p11-l000", PAGE, "A activates B", "unreviewed");
    window.read_page(PAGE, reply_with_report(
        PAGE, true, 1,
        json!([{ "code": "PDF_TYPE1_ENCODING_UNREADABLE", "message": "encoding unavailable" }]),
        vec![record],
    ));
    let requests = window.sent();
    assert_eq!(requests.iter().filter(|request| request.1 == "changes.apply").count(), 1);
    window.answer(&requests, "changes.apply", json!({ "revision": REVISION + 1, "changed_ids": [] }));
    assert!(window.message().is_some_and(|(severity, text)| severity == Severity::Warning
        && text.contains("일부 내장 글꼴 인코딩") && !text.contains("PDF_TYPE1_ENCODING_UNREADABLE") && !text.contains("encoding unavailable")));
    assert_eq!(window.app.smoke_pdf_codes, vec!["PDF_TYPE1_ENCODING_UNREADABLE".to_owned()]);
    assert_eq!(report_codes(&pdf_report(&mut window, &dir)), vec!["PDF_TYPE1_ENCODING_UNREADABLE"]);
}

#[test]
fn diagnostic_messages_are_not_collected_as_codes() {
    for (name, read, count) in [("fake-read", true, 1), ("fake-empty", true, 0), ("fake-unsafe", false, 0)] {
        let mut window = Window::open_figure(Vec::new());
        let dir = smoke_report_dir(&mut window, name);
        let records = if count == 1 { vec![read_record("pdf-p11-l000", PAGE, "A activates B", "unreviewed")] } else { Vec::new() };
        window.read_page(PAGE, reply_with_report(PAGE, read, count,
            json!([{ "code": "PDF_TYPE1_ENCODING_UNREADABLE", "message": "see PDF_FAKE_IN_MSG" }]), records));
        assert_eq!(window.app.smoke_pdf_codes, vec!["PDF_TYPE1_ENCODING_UNREADABLE".to_owned()]);
        assert_eq!(report_codes(&pdf_report(&mut window, &dir)), vec!["PDF_TYPE1_ENCODING_UNREADABLE"]);
    }
}

#[test]
fn warning_is_combined_with_an_unchanged_or_held_read() {
    for held_for_edit in [false, true] {
        let record = read_record("pdf-p11-l000", PAGE, "A activates B", "unreviewed");
        let mut window = Window::open_figure(if held_for_edit { Vec::new() } else { vec![record.clone()] });
        if held_for_edit {
            window.app.dataset_edit = Some(DatasetCellEdit {
                dataset_id: "dataset-1".into(), version_hash: "v1".into(), row_id: "row-1".into(),
                column_id: "column-1".into(), original: "0.10".into(), draft: "0.25".into(),
            });
        }
        window.read_page(PAGE, reply_with_report(PAGE, true, 1,
            json!([{ "code": "PDF_XOBJECT_MISSING", "message": "missing" }]), vec![record]));
        assert!(window.sent().iter().all(|request| request.1 != "changes.apply"));
        let (_, text) = window.message().expect("the result is visible");
        assert!(text.contains("일부 누락된 PDF 요소"));
        assert!(!text.contains("PDF_XOBJECT_MISSING") && !text.contains("missing"));
    }
}

#[test]
fn refusal_message_is_not_collected_as_a_code() {
    let mut window = Window::open_figure(Vec::new());
    let dir = smoke_report_dir(&mut window, "fake-refusal");
    let ticket = window.app.call("source.extract_pdf", json!({ "pages": [PAGE] }), PendingAction::PdfEvidenceImport).expect("the read is sent");
    window.app.pdf_evidence_target = Some(pdf_target_awaiting(ticket));
    let requests = window.sent();
    let mut refusal = page_refusal("PDF_MEMORY_LIMIT");
    refusal.diagnostics[0].message = "see PDF_FAKE_IN_MSG".into();
    window.refuse(&requests, "source.extract_pdf", refusal);
    assert_eq!(window.app.smoke_pdf_codes, vec!["PDF_MEMORY_LIMIT".to_owned()]);
    assert_eq!(report_codes(&pdf_report(&mut window, &dir)), vec!["PDF_MEMORY_LIMIT"]);
}

#[test]
fn a_successful_page_read_without_diagnostics_adds_no_warning_or_codes() {
    let mut window = Window::open_figure(Vec::new());
    let dir = smoke_report_dir(&mut window, "read-no-diagnostic");
    let record = read_record("pdf-p11-l000", PAGE, "A activates B", "unreviewed");
    window.read_page(PAGE, reply_with_report(PAGE, true, 1, json!([]), vec![record]));
    assert_eq!(window.sent().iter().filter(|request| request.1 == "changes.apply").count(), 1);
    assert!(window.message().is_none());
    assert!(window.app.smoke_pdf_codes.is_empty());
    assert!(report_codes(&pdf_report(&mut window, &dir)).is_empty());
}

#[test]
fn a_diagnostic_on_a_conflicting_read_keeps_the_error_visible() {
    let held = read_record("pdf-p11-l000", PAGE, "A activates B", "reviewed");
    let read = read_record("pdf-p11-l000", PAGE, "A activates D", "unreviewed");
    let mut window = Window::open_figure(vec![held]);
    let dir = smoke_report_dir(&mut window, "read-conflict-diagnostic");
    window.read_page(PAGE, reply_with_report(
        PAGE, true, 1,
        json!([{ "code": "PDF_TYPE1_ENCODING_UNREADABLE", "message": "encoding unavailable" }]),
        vec![read],
    ));
    assert!(window.sent().iter().all(|request| request.1 != "changes.apply"));
    let (severity, text) = window.message().expect("the conflict is visible");
    assert_eq!(severity, Severity::Error);
    assert!(text.contains(conflict_message(1).as_str()));
    assert!(text.contains("일부 내장 글꼴 인코딩"));
    assert!(!text.contains("PDF_TYPE1_ENCODING_UNREADABLE") && !text.contains("encoding unavailable"));
    assert_eq!(report_codes(&pdf_report(&mut window, &dir)), vec!["PDF_TYPE1_ENCODING_UNREADABLE"]);
}

#[test]
fn an_unknown_nonblocking_diagnostic_uses_a_generic_korean_warning() {
    let mut window = Window::open_figure(Vec::new());
    window.read_page(PAGE, reply_with_report(PAGE, true, 0,
        json!([{ "code": "PDF_UNKNOWN_NOTE", "message": "private English text" }]), Vec::new()));
    let (_, text) = window.message().expect("the empty page is reported");
    assert!(text.contains("PDF 읽기 중 확인이 필요한 항목"));
    assert!(!text.contains("PDF_UNKNOWN_NOTE") && !text.contains("private English text"));
}

#[test]
fn an_empty_page_read_shows_and_reports_nonblocking_diagnostics() {
    let mut window = Window::open_figure(Vec::new());
    let dir = smoke_report_dir(&mut window, "empty-read-diagnostic");
    window.read_page(PAGE, reply_with_report(
        PAGE, true, 0,
        json!([{ "code": "PDF_LINE_GAP_AMBIGUOUS", "message": "line gap unclear" }]),
        Vec::new(),
    ));
    assert!(window.sent().iter().all(|request| request.1 != "changes.apply"));
    let (severity, text) = window.message().expect("the empty page is reported");
    assert_eq!(severity, Severity::Error);
    assert!(text.contains("12쪽에는 글자가 없어"));
    assert!(text.contains("일부 줄 간격이 애매해"));
    assert!(!text.contains("PDF_LINE_GAP_AMBIGUOUS") && !text.contains("line gap unclear"));
    assert_eq!(window.app.smoke_pdf_codes, vec!["PDF_LINE_GAP_AMBIGUOUS".to_owned()]);
    assert_eq!(report_codes(&pdf_report(&mut window, &dir)), vec!["PDF_LINE_GAP_AMBIGUOUS"]);
}

#[test]
fn the_pdf_report_has_no_codes_when_the_reader_gives_none() {
    let mut window = Window::open_figure(Vec::new());
    let dir = smoke_report_dir(&mut window, "none");
    let pdf = window.app.call("source.extract_pdf", json!({ "pages": [PAGE] }), PendingAction::PdfEvidenceImport).expect("the read is sent");
    window.app.pdf_evidence_target = Some(pdf_target_awaiting(pdf));
    let requests = window.sent();
    window.answer(&requests, "source.extract_pdf", reply_with_report(PAGE, false, 0, json!([]), Vec::new()));
    assert!(window.app.smoke_pdf_codes.is_empty(), "a reading without diagnostics keeps no code");
    assert!(report_codes(&pdf_report(&mut window, &dir)).is_empty(), "a reading without diagnostics adds no code");
}
