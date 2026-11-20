//! The review mark on PDF-extracted evidence against the real figure engine, store and renderer: which records
//! the window offers a mark for, the change it builds, and what the host does with that change. The window never
//! marks a record reviewed by itself, and a mark keeps the text, source hash, page, region and PDF geometry.

use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

use designllm_contracts::{new_id, sha256, ChangeSet, ErrorCode, Evidence, Intent, Operation};
use serde_json::json;

use super::{
    evidence_additions, figure_change_on_its_way, figure_wait_reason, pdf_import_on_its_way, review_change,
    review_target, FIGURE_CHANGE_WAIT, FIGURE_READ_WAIT, PDF_IMPORT_WAIT,
};
use super::super::{figure_flow_tests::Host, state::ApplyContext, PendingAction, PendingRequest};

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

/// A figure holding the text of PDF page 12 as the window adds it. Returns the figure id and the records the host
/// stored.
fn figure_with_page_12(host: &mut Host) -> (String, Vec<Evidence>) {
    let figure_id = host.create("figure", "그림");
    let pdf = repository_root().join("fixtures/pdf/evidence-p12.pdf");
    let asset = host
        .call("asset.import", json!({ "path": pdf.to_string_lossy() }))
        .expect("import the PDF");
    // The window asks for the page the user typed (12, counted from one) as physical page 11.
    let page = host
        .call("source.extract_pdf", json!({ "sha256": asset["sha256"], "pages": [11] }))
        .expect("page 12 has text");
    let evidence: Vec<Evidence> = serde_json::from_value(page["evidence"].clone()).expect("evidence");
    assert!(!evidence.is_empty());
    host.apply(&figure_id, evidence_additions(evidence), content())
        .expect("the figure accepts the page's text");
    let (figure, _) = host.figure(&figure_id);
    (figure_id, figure.evidence)
}

fn record<'a>(evidence: &'a [Evidence], id: &str) -> &'a Evidence {
    evidence
        .iter()
        .find(|item| item.id == id)
        .expect("the record is in the figure")
}

/// Everything except the review state is the record the PDF reader returned.
fn assert_same_source(original: &Evidence, stored: &Evidence) {
    assert_eq!(original.id, stored.id);
    assert_eq!(original.origin, stored.origin);
    assert_eq!(original.source_hash, stored.source_hash);
    assert_eq!(original.physical_page_index, stored.physical_page_index);
    assert_eq!(original.text, stored.text);
    assert_eq!(
        serde_json::to_value(&original.region).expect("region"),
        serde_json::to_value(&stored.region).expect("region")
    );
    assert_eq!(
        serde_json::to_value(&original.pdf_geometry).expect("geometry"),
        serde_json::to_value(&stored.pdf_geometry).expect("geometry")
    );
}

#[test]
fn a_pdf_extraction_starts_unreviewed_and_offers_a_mark_only_in_the_forward_direction() {
    let mut host = Host::start();
    let (_, evidence) = figure_with_page_12(&mut host);
    assert!(!evidence.is_empty());
    for item in &evidence {
        assert_eq!(item.review_state, "unreviewed");
        assert_eq!(review_target(item), Some(true));
    }
}

#[test]
fn marking_an_extraction_reviewed_changes_only_its_review_state() {
    let mut host = Host::start();
    let (figure_id, before) = figure_with_page_12(&mut host);
    let target = before[0].id.clone();

    let (figure, revision) = host.figure(&figure_id);
    let operation = review_change(&figure, &target, true).expect("an unreviewed extraction can be marked");
    assert_eq!(operation.intent(), Intent::Content);
    let next = host.apply(&figure_id, vec![operation], content()).expect("the host accepts the mark");
    assert_eq!(next, revision + 1);

    let (after, after_revision) = host.figure(&figure_id);
    assert_eq!(after_revision, next);
    assert_eq!(after.evidence.len(), before.len());
    for stored in &after.evidence {
        assert_same_source(record(&before, &stored.id), stored);
        let expected = if stored.id == target { "reviewed" } else { "unreviewed" };
        assert_eq!(stored.review_state, expected);
    }
}

#[test]
fn taking_the_mark_back_returns_the_extraction_to_unreviewed_with_its_source_kept() {
    let mut host = Host::start();
    let (figure_id, before) = figure_with_page_12(&mut host);
    let target = before[0].id.clone();

    let (figure, _) = host.figure(&figure_id);
    let marked = review_change(&figure, &target, true).expect("an unreviewed extraction can be marked");
    host.apply(&figure_id, vec![marked], content()).expect("the host accepts the mark");

    let (figure, revision) = host.figure(&figure_id);
    assert_eq!(review_target(record(&figure.evidence, &target)), Some(false));
    let unmarked = review_change(&figure, &target, false).expect("a reviewed extraction can be unmarked");
    let next = host.apply(&figure_id, vec![unmarked], content()).expect("the host accepts the unmark");
    assert_eq!(next, revision + 1);

    let (after, _) = host.figure(&figure_id);
    let stored = record(&after.evidence, &target);
    assert_eq!(stored.review_state, "unreviewed");
    assert_same_source(record(&before, &target), stored);
}

#[test]
fn user_statements_have_no_mark_and_a_mark_is_offered_only_for_a_record_in_the_figure() {
    let mut host = Host::start();
    let (figure_id, before) = figure_with_page_12(&mut host);
    let text = "사용자가 직접 적은 근거";
    let statement = Evidence {
        id: "ev-statement".into(),
        source_hash: sha256(text.as_bytes()),
        origin: "user_statement".into(),
        physical_page_index: None,
        region: None,
        pdf_geometry: None,
        text: text.into(),
        review_state: "user_provided".into(),
    };
    host.apply(&figure_id, vec![Operation::PutEvidence { evidence: statement.clone() }], content())
        .expect("a user statement is accepted");
    let (figure, _) = host.figure(&figure_id);

    assert_eq!(review_target(&statement), None);
    assert!(review_change(&figure, "ev-statement", true).is_none());
    assert!(review_change(&figure, "ev-statement", false).is_none());
    assert!(review_change(&figure, &before[0].id, false).is_none(), "an unreviewed extraction cannot be unmarked");
    assert!(review_change(&figure, "no-such-record", true).is_none());
}

#[test]
fn a_mark_prepared_on_an_older_revision_is_refused_and_the_figure_stays_as_it_was() {
    let mut host = Host::start();
    let (figure_id, before) = figure_with_page_12(&mut host);
    let (figure, seen_revision) = host.figure(&figure_id);
    let operation = review_change(&figure, &before[0].id, true).expect("an unreviewed extraction can be marked");

    // Another edit lands before the window's mark arrives.
    host.apply(
        &figure_id,
        vec![Operation::SetCaption { caption: "다른 편집".into() }],
        content(),
    )
    .expect("the other edit lands first");
    let (_, current_revision) = host.figure(&figure_id);
    assert_eq!(current_revision, seen_revision + 1);

    let change = ChangeSet {
        request_id: new_id(),
        actor: "desktop".into(),
        workspace_id: host.workspace_id.clone(),
        artifact_id: figure_id.clone(),
        base_revision: seen_revision,
        allowed_intents: content(),
        job_ref: None,
        read_dependencies: Vec::new(),
        operations: vec![operation],
    };
    let error = host
        .call("changes.apply", serde_json::to_value(change).expect("change set"))
        .expect_err("a mark on a stale revision is refused");
    assert!(matches!(error.code, ErrorCode::RevisionConflict));

    let (after, revision) = host.figure(&figure_id);
    assert_eq!(revision, current_revision);
    assert_eq!(record(&after.evidence, &before[0].id).review_state, "unreviewed");
}

#[test]
fn a_mark_in_a_change_set_with_a_refused_operation_is_not_applied_at_all() {
    let mut host = Host::start();
    let (figure_id, before) = figure_with_page_12(&mut host);
    let (figure, revision) = host.figure(&figure_id);
    let mark = review_change(&figure, &before[0].id, true).expect("an unreviewed extraction can be marked");
    let refused = Operation::SetPanelLabel { panel_id: "no-such-panel".into(), label: "(a)".into() };

    host.apply(&figure_id, vec![mark, refused], content())
        .expect_err("the panel does not exist, so the whole change set is refused");
    let (after, now) = host.figure(&figure_id);
    assert_eq!(now, revision);
    assert_eq!(record(&after.evidence, &before[0].id).review_state, "unreviewed");
}

fn pending(action: PendingAction, method: &str) -> PendingRequest {
    PendingRequest { action, method: method.into(), artifact_id: None, require_active: false, apply: None, ui_run: None }
}

#[test]
fn a_pdf_import_is_the_reason_a_mark_waits_until_its_records_are_in_the_figure() {
    let asset = pending(PendingAction::PdfAssetImport { page_index: 11 }, "asset.import");
    let extraction = pending(PendingAction::PdfEvidenceImport, "source.extract_pdf");
    let commit = pending(PendingAction::FigureEvidence, "changes.apply");
    assert!(pdf_import_on_its_way(&asset) && pdf_import_on_its_way(&extraction));
    assert!(!pdf_import_on_its_way(&commit), "the commit of the records is a change, not an import");
    assert_eq!(figure_wait_reason(vec![&asset]), Some(PDF_IMPORT_WAIT));
    assert_eq!(figure_wait_reason(vec![&extraction]), Some(PDF_IMPORT_WAIT));
    assert_eq!(figure_wait_reason(vec![&commit]), Some(FIGURE_CHANGE_WAIT));
    assert_eq!(figure_wait_reason(vec![&commit, &asset]), Some(PDF_IMPORT_WAIT), "an import is named before a change");
}

#[test]
fn every_change_to_the_open_figure_on_its_way_keeps_a_mark_waiting_and_settled_imports_do_not() {
    let changes = [
        pending(PendingAction::Apply, "changes.apply"),
        pending(PendingAction::FigureDataset, "changes.apply"),
        pending(PendingAction::FigureEvidence, "changes.apply"),
        pending(PendingAction::FigureImageAsset { origin: "image".into(), source_ref: None }, "asset.import"),
        pending(PendingAction::DatasetEdit, "figure.dataset.edit"),
        pending(PendingAction::History, "history.undo"),
    ];
    for change in &changes {
        assert!(figure_change_on_its_way(change));
        assert_eq!(figure_wait_reason(vec![change]), Some(FIGURE_CHANGE_WAIT));
    }
    let settled = [
        pending(PendingAction::CsvAssetImport, "asset.import"),
        pending(PendingAction::CsvDatasetImport, "source.import_csv"),
        pending(PendingAction::CsvDeclaredImport, "source.import_csv"),
    ];
    for request in &settled {
        assert!(!figure_change_on_its_way(request));
        assert_eq!(figure_wait_reason(vec![request]), None);
    }
}

#[test]
fn a_change_set_on_its_way_keeps_a_mark_waiting_whatever_its_action() {
    let change = PendingRequest {
        action: PendingAction::Selection,
        method: "changes.apply".into(),
        artifact_id: None,
        require_active: false,
        apply: Some(ApplyContext {
            request_id: "request".into(),
            workspace_id: "workspace".into(),
            artifact_id: "figure".into(),
            base_revision: 1,
            operations: Vec::new(),
            allowed_intents: content(),
            actor: "desktop".into(),
            user_edit: true,
            retryable: true,
            held_edit_id: None,
            after_action: PendingAction::Selection,
        }),
        ui_run: None,
    };
    assert!(figure_change_on_its_way(&change));
    assert_eq!(figure_wait_reason(vec![&change]), Some(FIGURE_CHANGE_WAIT));
}

#[test]
fn a_read_of_the_figure_after_a_change_keeps_a_mark_waiting_until_it_lands() {
    let read = pending(PendingAction::Context, "context.read");
    let inspect = pending(PendingAction::Inspect, "artifact.inspect");
    assert!(!figure_change_on_its_way(&read) && !figure_change_on_its_way(&inspect));
    assert_eq!(figure_wait_reason(vec![&read]), Some(FIGURE_READ_WAIT));
    assert_eq!(figure_wait_reason(vec![&inspect]), Some(FIGURE_READ_WAIT));
    assert_eq!(figure_wait_reason(Vec::<&PendingRequest>::new()), None, "nothing on its way leaves the mark available");
}
