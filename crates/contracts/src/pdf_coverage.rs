//! The page rules of one `source.extract_pdf` reading, shared by core and the desktop window. Core checks a reader's result
//! with them (`PdfReading::validate`) and reads completion with `document_complete`. The window checks each reply with the
//! same rules (`coverage_is_consistent`) and reads completion the same way (`PdfCoverage::complete`). Each side passes its own
//! input shape: core its typed fields, the window the JSON fields. Core's acceptance checks run between the two stages below: a
//! page left unread (a requested page missing from `read_pages`, or a page reported unread), a whole document left unread, and
//! blocking diagnostics. Core's evidence checks (source, physical page, page read, evidence count) run after stage two. The
//! window runs none of these checks, since they are not coverage fields. Evidence pages still count in stage one: core passes
//! them, the window passes none.
//!
//! Premise (한 응답 한 쪽 전제): one reply answers one request for one page, and the window asks for one page per reply. The
//! rules take the requested pages as a list, so core's multi-page requests use them unchanged, and the window passes a list of
//! one page. Where the premise would change: `coverage_is_consistent` (its `requested` argument), `page_outcome` in
//! `desktop/src/app/figure_tools.rs`, `PdfEvidenceTarget.page_index` in `desktop/src/app/state.rs:135`, the target match in
//! `desktop/src/app/figure_tools.rs:933`, and the one-page request (`"pages": [page_index]`) in `desktop/src/app/figure_tools.rs:945`.
//! Each of them assumes one page per reply.

use std::collections::BTreeSet;

/// The page fields of one reading, as the rules read them.
#[derive(Debug, Clone, Copy)]
pub struct PageFields<'a> {
    /// The page count the reader reported; `None` from a reader that reports none.
    pub page_count: Option<u32>,
    /// `read_pages`, strictly ascending.
    pub read_pages: &'a [u32],
    /// `page_reports` as (physical page, read flag), strictly ascending by page. `None` when the reply has no `page_reports`:
    /// nothing is then checked against reports.
    pub reports: Option<&'a [(u32, bool)]>,
}

/// A contradiction between the page fields, the request and the page count. Each set names the pages it concerns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PageContradiction {
    /// `read_pages` is not strictly ascending: a page is listed twice or out of order.
    ReadPagesOrder,
    /// `page_reports` is not strictly ascending: a page is reported twice or out of order.
    ReportsOrder,
    /// The page count is zero.
    ZeroCount,
    /// The reading names pages that the request did not ask for.
    NotRequested(BTreeSet<u32>),
    /// The reading names pages at or past the page count.
    BeyondCount(BTreeSet<u32>),
    /// The request asks for pages at or past the page count.
    RequestedBeyondCount(BTreeSet<u32>),
    /// `read_pages` and the pages the reports mark as read differ. The set is the symmetric difference of the two.
    ReadMismatch(BTreeSet<u32>),
}

/// Stage one, in the order core refuses them: the order of `read_pages` and of `page_reports`, a zero count, pages the request
/// did not ask for, pages at or past the count. The named pages are `read_pages`, the pages of `page_reports` and
/// `evidence_pages`. An empty `requested` is a whole-document request, which names no page of its own. The window passes no
/// evidence pages, since its check reads the coverage fields only.
pub fn pages_contradiction(requested: &[u32], evidence_pages: &[u32], fields: &PageFields<'_>) -> Option<PageContradiction> {
    if !fields.read_pages.windows(2).all(|pair| pair[0] < pair[1]) {
        return Some(PageContradiction::ReadPagesOrder);
    }
    if let Some(reports) = fields.reports {
        if !reports.windows(2).all(|pair| pair[0].0 < pair[1].0) {
            return Some(PageContradiction::ReportsOrder);
        }
    }
    if fields.page_count == Some(0) {
        return Some(PageContradiction::ZeroCount);
    }
    let named: BTreeSet<u32> = fields
        .read_pages
        .iter()
        .copied()
        .chain(fields.reports.into_iter().flatten().map(|&(page, _)| page))
        .chain(evidence_pages.iter().copied())
        .collect();
    if !requested.is_empty() {
        let asked: BTreeSet<u32> = requested.iter().copied().collect();
        let outside: BTreeSet<u32> = named.iter().copied().filter(|page| !asked.contains(page)).collect();
        if !outside.is_empty() {
            return Some(PageContradiction::NotRequested(outside));
        }
    }
    if let Some(count) = fields.page_count {
        let beyond: BTreeSet<u32> = named.iter().copied().filter(|&page| page >= count).collect();
        if !beyond.is_empty() {
            return Some(PageContradiction::BeyondCount(beyond));
        }
    }
    None
}

/// Stage two, which core runs after its unread and blocking checks: requested pages at or past the page count, and `read_pages`
/// that differ from the pages the reports mark as read. Core never sees the first case here. A requested page that `read_pages`
/// leaves out is an unread page, which core's unread check names before this stage; a requested page that `read_pages` lists
/// past the count is named by stage one. So core's refusal at this point is always a `ReadMismatch`.
/// Reports that are absent leave the second case unchecked. A report that is not marked as read does not count as read here:
/// the window accepts such a page as consistent and shows it as unsafe, while core refuses it as unread.
/// Precondition: the input has passed `pages_contradiction` first. This function compares `read_pages` as a set, so a duplicate or
/// an out-of-order entry is refused only by that earlier stage (`ReadPagesOrder`).
pub fn marks_contradiction(requested: &[u32], fields: &PageFields<'_>) -> Option<PageContradiction> {
    if let Some(count) = fields.page_count {
        let past: BTreeSet<u32> = requested.iter().copied().filter(|&page| page >= count).collect();
        if !past.is_empty() {
            return Some(PageContradiction::RequestedBeyondCount(past));
        }
    }
    let reports = fields.reports?;
    let marked: BTreeSet<u32> = reports.iter().filter(|&&(_, read)| read).map(|&(page, _)| page).collect();
    let read: BTreeSet<u32> = fields.read_pages.iter().copied().collect();
    if read != marked {
        return Some(PageContradiction::ReadMismatch(read.symmetric_difference(&marked).copied().collect()));
    }
    None
}

/// Whether a reading covers the whole document: the page count is known and above zero, and `read_pages` (strictly ascending)
/// is exactly `0..count`. Core's `PdfReading::document_complete` and the window's completion check both use it.
pub fn document_complete(page_count: Option<u32>, read_pages: impl IntoIterator<Item = u32>) -> bool {
    match page_count {
        Some(count) if count > 0 => read_pages.into_iter().eq(0..count),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::{document_complete, marks_contradiction, pages_contradiction, PageContradiction, PageFields};
    use std::collections::BTreeSet;

    /// One reply as the rules see it.
    struct Reply<'a> {
        requested: &'a [u32],
        evidence: &'a [u32],
        count: Option<u32>,
        read: &'a [u32],
        reports: Option<&'a [(u32, bool)]>,
    }

    /// The first contradiction in the order the two stages give, with no unread or blocking check (the window's view).
    fn first_contradiction(reply: &Reply<'_>) -> Option<PageContradiction> {
        let fields = PageFields { page_count: reply.count, read_pages: reply.read, reports: reply.reports };
        pages_contradiction(reply.requested, reply.evidence, &fields).or_else(|| marks_contradiction(reply.requested, &fields))
    }

    fn set(pages: &[u32]) -> BTreeSet<u32> {
        pages.iter().copied().collect()
    }

    #[test]
    fn a_contradicting_reply_names_the_first_contradiction_its_stage_finds() {
        let cases: [(&str, Reply<'_>, PageContradiction); 13] = [
            (
                "read_pages lists a page twice",
                Reply { requested: &[11], evidence: &[], count: Some(12), read: &[11, 11], reports: Some(&[(11, true)]) },
                PageContradiction::ReadPagesOrder,
            ),
            (
                "read_pages out of order",
                Reply { requested: &[], evidence: &[], count: Some(12), read: &[1, 0], reports: Some(&[(0, true), (1, true)]) },
                PageContradiction::ReadPagesOrder,
            ),
            (
                "page_reports lists a page twice",
                Reply { requested: &[11], evidence: &[], count: Some(12), read: &[11], reports: Some(&[(11, true), (11, true)]) },
                PageContradiction::ReportsOrder,
            ),
            (
                "page count of zero",
                Reply { requested: &[], evidence: &[], count: Some(0), read: &[], reports: Some(&[]) },
                PageContradiction::ZeroCount,
            ),
            (
                "a read page that was not requested",
                Reply { requested: &[11], evidence: &[], count: Some(12), read: &[0, 11], reports: Some(&[(0, true), (11, true)]) },
                PageContradiction::NotRequested(set(&[0])),
            ),
            (
                "evidence on a page that was not requested",
                Reply { requested: &[11], evidence: &[4], count: Some(12), read: &[11], reports: Some(&[(11, true)]) },
                PageContradiction::NotRequested(set(&[4])),
            ),
            (
                "a report of another page",
                Reply { requested: &[11], evidence: &[], count: Some(12), read: &[11], reports: Some(&[(10, false), (11, true)]) },
                PageContradiction::NotRequested(set(&[10])),
            ),
            (
                "a read page past the count",
                Reply { requested: &[], evidence: &[], count: Some(5), read: &[7], reports: Some(&[(7, true)]) },
                PageContradiction::BeyondCount(set(&[7])),
            ),
            (
                "a requested page read past the count",
                Reply { requested: &[11], evidence: &[], count: Some(5), read: &[11], reports: Some(&[(11, true)]) },
                PageContradiction::BeyondCount(set(&[11])),
            ),
            (
                "a requested page at the count, not read",
                Reply { requested: &[11], evidence: &[], count: Some(11), read: &[], reports: Some(&[]) },
                PageContradiction::RequestedBeyondCount(set(&[11])),
            ),
            (
                "a read page without a report",
                Reply { requested: &[], evidence: &[], count: None, read: &[0, 1], reports: Some(&[(0, true)]) },
                PageContradiction::ReadMismatch(set(&[1])),
            ),
            (
                "a report marks a page read that read_pages leaves out",
                Reply { requested: &[11], evidence: &[], count: Some(12), read: &[], reports: Some(&[(11, true)]) },
                PageContradiction::ReadMismatch(set(&[11])),
            ),
            (
                "read_pages lists a page the report marks unread",
                Reply { requested: &[11], evidence: &[], count: Some(12), read: &[11], reports: Some(&[(11, false)]) },
                PageContradiction::ReadMismatch(set(&[11])),
            ),
        ];
        for (name, reply, expected) in &cases {
            assert_eq!(first_contradiction(reply).as_ref(), Some(expected), "{name}");
        }
    }

    #[test]
    fn a_consistent_reply_has_no_contradiction_even_when_it_leaves_a_page_unread() {
        let cases: [(&str, Reply<'_>); 7] = [
            ("no count and no reports", Reply { requested: &[11], evidence: &[], count: None, read: &[], reports: None }),
            ("no count, the page read and reported", Reply { requested: &[11], evidence: &[], count: None, read: &[11], reports: Some(&[(11, true)]) }),
            ("no reports to check", Reply { requested: &[11], evidence: &[], count: Some(12), read: &[11], reports: None }),
            ("the page read and reported", Reply { requested: &[11], evidence: &[], count: Some(12), read: &[11], reports: Some(&[(11, true)]) }),
            (
                "the page unread and reported unread (core refuses it as unread)",
                Reply { requested: &[11], evidence: &[], count: Some(12), read: &[], reports: Some(&[(11, false)]) },
            ),
            (
                "whole document read to its count",
                Reply { requested: &[], evidence: &[], count: Some(3), read: &[0, 1, 2], reports: Some(&[(0, true), (1, true), (2, true)]) },
            ),
            (
                "whole document with a page left out (core refuses it as unread)",
                Reply { requested: &[], evidence: &[], count: Some(3), read: &[0, 1], reports: Some(&[(0, true), (1, true)]) },
            ),
        ];
        for (name, reply) in &cases {
            assert_eq!(first_contradiction(reply), None, "{name}");
        }
    }

    #[test]
    fn the_document_is_complete_only_when_its_count_is_known_and_every_page_is_read() {
        assert!(!document_complete(None, [0, 1]), "without a count nothing is complete");
        assert!(!document_complete(Some(0), []), "a count of zero");
        assert!(document_complete(Some(1), [0]));
        assert!(!document_complete(Some(2), [0]), "page 2 is unread");
        assert!(!document_complete(Some(3), [1, 2]), "page 1 is unread");
        assert!(document_complete(Some(2), [0, 1]));
        assert!(!document_complete(Some(2), [0, 1, 2]), "a page past the count is not the document");
    }
}
