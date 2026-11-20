//! The PDF pages a figure's readings cover. A reading of `source.extract_pdf` reports the page count of the document and the
//! pages it read. The window keeps them for one revision of one figure and one PDF file, shows them in the figure's evidence
//! panel, and drops them when the PDF file changes. `coverage_is_consistent` checks each reply first, so a reply whose fields
//! contradict each other shows no page as read.

use std::collections::BTreeSet;

use designllm_contracts::pdf_coverage::{document_complete, marks_contradiction, pages_contradiction, PageFields};
use serde_json::Value;

use super::{DesktopApp, PdfEvidenceTarget};

/// What the window has read of one PDF file, for one revision of one figure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct PdfCoverage {
    artifact_id: String,
    revision: u64,
    asset_sha256: String,
    /// The page count the reader reported; `None` while no reading reported one.
    page_count: Option<u32>,
    /// Physical pages (from 0) that accepted readings reported as read.
    read_pages: BTreeSet<u32>,
}

impl PdfCoverage {
    fn new(artifact_id: &str, revision: u64, asset_sha256: &str) -> Self {
        Self {
            artifact_id: artifact_id.to_owned(),
            revision,
            asset_sha256: asset_sha256.to_owned(),
            page_count: None,
            read_pages: BTreeSet::new(),
        }
    }

    /// Whether every page of the document is read: the count is known and the pages read are exactly 0..count.
    fn complete(&self) -> bool {
        document_complete(self.page_count, self.read_pages.iter().copied())
    }

    /// The line the evidence panel shows for this coverage.
    fn text(&self) -> String {
        let read = if self.read_pages.is_empty() {
            "없음".to_owned()
        } else {
            self.read_pages.iter().map(|page| format!("{}쪽", page.saturating_add(1))).collect::<Vec<_>>().join(", ")
        };
        match self.page_count {
            Some(count) if self.complete() => format!("전체 읽음 · 전체 {count}쪽"),
            Some(count) => format!("전체 {count}쪽 중 읽은 쪽: {read}"),
            None => format!("전체 쪽 수 모름 · 읽은 쪽: {read}"),
        }
    }

    /// Whether the reading of `target` (its figure, revision and PDF file) is the one this coverage holds.
    fn is_reading_of(&self, target: &PdfEvidenceTarget, asset_sha256: &str) -> bool {
        self.artifact_id == target.artifact_id && self.revision == target.revision && self.asset_sha256 == asset_sha256
    }

    /// Whether a reply's page count may join this coverage: a count the coverage already has must be the same one.
    fn agrees_with(&self, page_count: Option<u32>) -> bool {
        match (self.page_count, page_count) {
            (Some(known), Some(reported)) => known == reported,
            _ => true,
        }
    }
}

/// A page number of a reply: an integer that fits a `u32`.
fn page_number(value: &Value) -> Option<u32> {
    value.as_u64().and_then(|page| u32::try_from(page).ok())
}

/// Whether the coverage fields of a reply to the one-page request for physical page `requested` agree with the request and
/// with each other. The rules are the shared ones of `designllm_contracts::pdf_coverage`, which core applies in
/// `PdfReading::validate` as well; this function reads the JSON fields into them. A reply without `page_count` and
/// `read_pages` (the earlier shape) has nothing to contradict, and a `null` count is a reader that reports no count.
///
/// Core also refuses a requested page that is not read, a blocking diagnostic and evidence on a page that was not read. This
/// function checks none of them, since they are not coverage fields: a page that is not read is consistent here. `page_outcome`
/// then shows that page as not read safely when the reply has `page_reports`. Core's coverage-reader replies always carry them
/// (`crates/core/src/pdf_read.rs:143-145`); the legacy four-field reply (`crates/core/src/routes.rs:692`) does not, and
/// `page_outcome` shows it as `PageOutcome::Legacy`.
pub(super) fn coverage_is_consistent(reply: &Value, requested: u32) -> bool {
    let page_count = match reply.get("page_count") {
        None | Some(Value::Null) => None,
        Some(value) => match page_number(value) {
            Some(count) => Some(count),
            None => return false,
        },
    };
    let read: Vec<u32> = match reply.get("read_pages") {
        None | Some(Value::Null) => return page_count.is_none(),
        Some(Value::Array(pages)) => match pages.iter().map(page_number).collect::<Option<Vec<u32>>>() {
            Some(read) => read,
            None => return false,
        },
        Some(_) => return false,
    };
    let reports: Option<Vec<(u32, bool)>> = match reply.get("page_reports") {
        None | Some(Value::Null) => None,
        Some(Value::Array(reports)) => {
            let pairs = reports.iter().map(|report| {
                let page = report.get("physical_page_index").and_then(page_number)?;
                Some((page, report.get("read").and_then(Value::as_bool) == Some(true)))
            });
            match pairs.collect::<Option<Vec<(u32, bool)>>>() {
                Some(pairs) => Some(pairs),
                None => return false,
            }
        }
        Some(_) => return false,
    };
    let fields = PageFields { page_count, read_pages: read.as_slice(), reports: reports.as_deref() };
    let asked = [requested];
    pages_contradiction(&asked, &[], &fields).is_none() && marks_contradiction(&asked, &fields).is_none()
}

/// Moves the coverage of the reading `target` belongs to to the next revision, once the window's own commit of that reading's
/// records has succeeded. `committed_revision` is the revision the commit's reply names: only a commit at the reading's
/// revision plus one carries the coverage, since the figure is then at the next revision and the coverage still belongs to
/// it. A reply that names another revision or none, or a commit made from another revision or another file, leaves the
/// coverage where it is.
pub(super) fn carry_pdf_coverage(
    coverage: &mut Option<PdfCoverage>,
    target: &PdfEvidenceTarget,
    committed_revision: Option<u64>,
) {
    let Some(asset_sha256) = target.asset_sha256.as_deref() else {
        return;
    };
    let Some(next) = target.revision.checked_add(1).filter(|next| committed_revision == Some(*next)) else {
        return;
    };
    if let Some(current) = coverage.as_mut().filter(|current| current.is_reading_of(target, asset_sha256)) {
        current.revision = next;
    }
}

/// The page count of a reply that passed `coverage_is_consistent`.
fn reply_page_count(reply: &Value) -> Option<u32> {
    reply.get("page_count").and_then(page_number)
}

/// The read pages of a reply that passed `coverage_is_consistent`.
fn reply_read_pages(reply: &Value) -> impl Iterator<Item = u32> + '_ {
    reply.get("read_pages").and_then(Value::as_array).into_iter().flatten().filter_map(page_number)
}

impl DesktopApp {
    /// Adds one reply of `source.extract_pdf` to the coverage of its figure revision and PDF file. Only a reply the import
    /// awaits gets here (`pdf_evidence_read`). `accepted` is false for a page the window did not show as read: its pages are
    /// not added, but its page count is kept. A reply whose fields contradict each other is not recorded.
    pub(super) fn record_pdf_reading(&mut self, target: &PdfEvidenceTarget, reply: &Value, accepted: bool) {
        let Some(asset_sha256) = target.asset_sha256.as_deref() else {
            return;
        };
        let has_coverage_fields = reply.get("page_count").is_some() || reply.get("read_pages").is_some();
        if !has_coverage_fields || !coverage_is_consistent(reply, target.page_index) {
            return;
        }
        let page_count = reply_page_count(reply);
        let mut coverage = match self.pdf_coverage.take() {
            Some(previous) if previous.is_reading_of(target, asset_sha256) && previous.agrees_with(page_count) => previous,
            _ => PdfCoverage::new(&target.artifact_id, target.revision, asset_sha256),
        };
        coverage.page_count = coverage.page_count.or(page_count);
        if accepted {
            coverage.read_pages.extend(reply_read_pages(reply));
        }
        if let Some(count) = coverage.page_count {
            coverage.read_pages.retain(|page| *page < count);
        }
        self.pdf_coverage = Some(coverage);
    }

    /// The line the evidence panel shows for the open figure, `None` when no reading covers it or the reading belongs to another
    /// revision of the figure.
    pub(super) fn pdf_coverage_text(&self) -> Option<String> {
        let coverage = self.pdf_coverage.as_ref()?;
        let artifact = self.artifact.as_ref()?;
        let shows_its_revision = artifact.id == coverage.artifact_id && self.shown_figure_revision() == Some(coverage.revision);
        shows_its_revision.then(|| coverage.text())
    }

    /// Drops the coverage read from another PDF file, once the window holds the file `asset_sha256`.
    pub(super) fn forget_pdf_coverage_of_other_file(&mut self, asset_sha256: &str) {
        if self.pdf_coverage.as_ref().is_some_and(|coverage| coverage.asset_sha256 != asset_sha256) {
            self.pdf_coverage = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{json, Value};

    use super::{carry_pdf_coverage, coverage_is_consistent, PdfCoverage, PdfEvidenceTarget};
    use crate::app::figure_tools::{page_outcome, PageOutcome};

    /// The physical page the window asks for in these tests (physical pages count from 0).
    const PAGE: u32 = 11;

    /// A reply that reads `read_pages` and reports `PAGE` as read with no text.
    fn reply(page_count: Value, read_pages: Value) -> Value {
        reply_on(PAGE, true, page_count, read_pages)
    }

    /// A reply that reads `read_pages` and has one page report, for `reported`, read or not, with no text.
    fn reply_on(reported: u32, read: bool, page_count: Value, read_pages: Value) -> Value {
        json!({
            "evidence": [],
            "page_count": page_count,
            "read_pages": read_pages,
            "page_reports": [{ "physical_page_index": reported, "read": read, "evidence_count": 0, "diagnostics": [] }],
        })
    }

    /// The coverage of "figure-1" at revision 7 and one PDF file, with the count and the pages read (physical, from 0).
    fn coverage(page_count: Option<u32>, read_pages: &[u32]) -> PdfCoverage {
        let mut coverage = PdfCoverage::new("figure-1", 7, &"a".repeat(64));
        coverage.page_count = page_count;
        coverage.read_pages = read_pages.iter().copied().collect();
        coverage
    }

    /// The import target of a reading of physical page 2 of "figure-1" at `revision`, from the PDF file `sha256`.
    fn target(revision: u64, sha256: &str) -> PdfEvidenceTarget {
        PdfEvidenceTarget {
            artifact_id: "figure-1".into(),
            revision,
            page_index: 2,
            file_name: "paper.pdf".into(),
            asset_sha256: Some(sha256.into()),
            awaiting: None,
        }
    }

    #[test]
    fn a_reply_without_coverage_fields_has_nothing_to_contradict() {
        assert!(coverage_is_consistent(&json!({ "evidence": [] }), PAGE));
        assert!(coverage_is_consistent(&json!({ "read_pages": [PAGE] }), PAGE), "without a count the pages are not checked against one");
    }

    #[test]
    fn a_null_count_is_a_reader_that_reports_no_count() {
        assert!(coverage_is_consistent(&reply(Value::Null, json!([PAGE])), PAGE));
        assert!(coverage_is_consistent(&reply_on(PAGE, false, Value::Null, json!([])), PAGE));
    }

    #[test]
    fn a_count_must_be_a_page_number_above_the_requested_page() {
        for count in [json!(0), json!(-1), json!(1.5), json!("12"), json!(4_294_967_296_u64), json!(11)] {
            assert!(!coverage_is_consistent(&reply(count.clone(), json!([PAGE])), PAGE), "page_count {count} is refused");
        }
        assert!(coverage_is_consistent(&reply(json!(12), json!([PAGE])), PAGE));
        assert!(coverage_is_consistent(&reply_on(0, true, json!(1), json!([0])), 0), "a one-page document is consistent");
    }

    #[test]
    fn read_pages_are_the_requested_page_once() {
        assert!(!coverage_is_consistent(&reply(json!(12), json!([0, PAGE])), PAGE), "page 1 was not asked for");
        assert!(!coverage_is_consistent(&reply(json!(12), json!((0..12).collect::<Vec<u32>>())), PAGE), "every page");
        assert!(!coverage_is_consistent(&reply(json!(12), json!([PAGE, PAGE])), PAGE), "the page twice");
        assert!(!coverage_is_consistent(&reply(json!(12), json!(["11"])), PAGE));
        assert!(!coverage_is_consistent(&reply(json!(12), json!([-1])), PAGE));
        assert!(!coverage_is_consistent(&json!({ "read_pages": [3] }), PAGE), "without a count too");
    }

    #[test]
    fn the_page_reports_mark_exactly_the_read_pages_as_read() {
        assert!(!coverage_is_consistent(&reply(json!(12), json!([])), PAGE), "a read report the read pages leave out");
        assert!(!coverage_is_consistent(&reply_on(PAGE, false, json!(12), json!([PAGE])), PAGE), "a read page reported unread");
        assert!(coverage_is_consistent(&reply_on(PAGE, false, json!(12), json!([])), PAGE), "an unread page in both");
        assert!(!coverage_is_consistent(&reply_on(10, true, json!(12), json!([PAGE])), PAGE), "a report of another page");
        let twice = json!({
            "page_count": 12,
            "read_pages": [PAGE],
            "page_reports": [
                { "physical_page_index": PAGE, "read": true, "evidence_count": 0, "diagnostics": [] },
                { "physical_page_index": PAGE, "read": true, "evidence_count": 0, "diagnostics": [] },
            ],
        });
        assert!(!coverage_is_consistent(&twice, PAGE), "the page reported twice");
        assert!(!coverage_is_consistent(&json!({ "page_count": 12, "read_pages": [PAGE], "page_reports": {} }), PAGE));
    }

    #[test]
    fn a_count_comes_with_the_read_pages_it_covers() {
        assert!(!coverage_is_consistent(&json!({ "page_count": 12 }), PAGE));
        assert!(!coverage_is_consistent(&reply(json!(12), Value::Null), PAGE));
        assert!(!coverage_is_consistent(&reply(json!(12), json!("11")), PAGE), "read_pages must be a list");
    }

    #[test]
    fn a_contradicting_reply_shows_the_requested_page_as_not_read() {
        // The page report says the page was read with no text, but the coverage fields put the page past the count.
        assert_eq!(page_outcome(&reply(json!(11), json!([PAGE])), PAGE, 0), PageOutcome::Unsafe(Vec::new()));
        assert_eq!(page_outcome(&reply(json!(0), json!([])), PAGE, 0), PageOutcome::Unsafe(Vec::new()));
        // The page report says the page was read, but the read pages name every page of the document.
        let every_page = reply(json!(12), json!((0..12).collect::<Vec<u32>>()));
        assert_eq!(page_outcome(&every_page, PAGE, 0), PageOutcome::Unsafe(Vec::new()));
        assert_eq!(
            page_outcome(&json!({ "page_count": 0, "read_pages": [] }), PAGE, 0),
            PageOutcome::Unsafe(Vec::new()),
            "the earlier reply shape too"
        );
    }

    #[test]
    fn a_consistent_reply_keeps_its_page_outcome() {
        assert_eq!(page_outcome(&json!({ "evidence": [] }), PAGE, 0), PageOutcome::Legacy);
        assert_eq!(page_outcome(&reply(json!(12), json!([PAGE])), PAGE, 0), PageOutcome::NoText);
        assert_eq!(page_outcome(&reply(Value::Null, json!([PAGE])), PAGE, 0), PageOutcome::NoText);
    }

    #[test]
    fn the_line_names_the_count_and_the_pages_read_from_one() {
        assert_eq!(coverage(Some(12), &[2]).text(), "전체 12쪽 중 읽은 쪽: 3쪽");
        assert_eq!(coverage(Some(12), &[2, 4]).text(), "전체 12쪽 중 읽은 쪽: 3쪽, 5쪽");
        assert_eq!(coverage(Some(12), &[]).text(), "전체 12쪽 중 읽은 쪽: 없음");
    }

    #[test]
    fn an_unknown_count_is_said_so_and_the_pages_read_are_still_named() {
        assert_eq!(coverage(None, &[2]).text(), "전체 쪽 수 모름 · 읽은 쪽: 3쪽");
    }

    #[test]
    fn the_whole_document_reads_as_complete_only_when_every_page_is_read() {
        assert!(!coverage(Some(2), &[0]).complete());
        assert_eq!(coverage(Some(2), &[0]).text(), "전체 2쪽 중 읽은 쪽: 1쪽");
        assert!(!coverage(Some(3), &[0, 1]).complete(), "page 3 is unread");
        assert!(coverage(Some(2), &[0, 1]).complete());
        assert_eq!(coverage(Some(2), &[0, 1]).text(), "전체 읽음 · 전체 2쪽");
        assert!(!coverage(None, &[0, 1]).complete(), "without a count nothing is complete");
    }

    #[test]
    fn a_commit_of_the_reading_moves_the_coverage_to_the_next_revision_of_the_same_file() {
        let mut same = Some(coverage(Some(12), &[2]));
        carry_pdf_coverage(&mut same, &target(7, &"a".repeat(64)), Some(8));
        assert_eq!(same.as_ref().map(|coverage| coverage.revision), Some(8));

        let mut other_revision = Some(coverage(Some(12), &[2]));
        carry_pdf_coverage(&mut other_revision, &target(6, &"a".repeat(64)), Some(7));
        assert_eq!(other_revision.as_ref().map(|coverage| coverage.revision), Some(7));

        let mut other_file = Some(coverage(Some(12), &[2]));
        carry_pdf_coverage(&mut other_file, &target(7, &"b".repeat(64)), Some(8));
        assert_eq!(other_file.as_ref().map(|coverage| coverage.revision), Some(7));
    }

    #[test]
    fn a_commit_whose_reply_names_another_revision_or_none_leaves_the_coverage() {
        for committed in [Some(7), Some(9), None] {
            let mut kept = Some(coverage(Some(12), &[2]));
            carry_pdf_coverage(&mut kept, &target(7, &"a".repeat(64)), committed);
            assert_eq!(kept.as_ref().map(|coverage| coverage.revision), Some(7), "{committed:?}");
        }
    }
}
