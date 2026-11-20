//! designllm-sources: PDF evidence extraction for figure provenance.
//!
//! Contract (docs/API_V1.md, docs/FIGURES.md, contracts `Evidence` and `PdfPageGeometry`):
//! - `physical_page_index` is 0-based. `printed_page_label` comes only from the catalog's /PageLabels and is
//!   never used as an index. A document without /PageLabels reports `None`.
//! - `Evidence.region` is in EMU, relative to the CropBox top-left, unrotated, x right and y down.
//!   The absolute PDF box is `PdfBox` (user space, x right and y up) with UserUnit applied.
//! - Coverage (`source_scopes`) lists only pages whose content was actually read.
//! - `PdfExtraction::page_count` is the page tree's own count. Every `/Count` in the tree must agree with the pages
//!   below it and every `/Parent` must name the node that lists it. A malformed tree is refused before any page is read.
//!
//! Implementation: lopdf parser plus an own text state machine (G0 candidate A). Text comes from /ToUnicode, or for a simple
//! font from its encoding (WinAnsi, MacRoman and Standard tables, and /Differences names through the Adobe Glyph List).
//! Anything else is an explicit unsupported error, not a guess.
//! Known limits are listed in docs of the modules and in the implementation report.

mod boxes;
mod builtin;
mod content;
mod decode;
mod error;
mod font;
mod glyph_names;
mod lines;
mod units;

use boxes::{LabelRange, Rect4};
use content::{Meters, RawGlyph, Request, Unit};
use designllm_contracts::{
    sha256, AssetRef, AssetStore, Diagnostic, DltError, DltResult, ErrorCode, Evidence, PdfBox, PdfPageGeometry,
    Rect, SourceCoverage,
};
use designllm_contracts::{PDF_INTERNAL_PANIC, PDF_TOO_LARGE};
use error::{PResult, PdfErr};
use lopdf::{Dictionary, Document, Object, ObjectId};
use std::collections::BTreeSet;
use std::panic::{catch_unwind, AssertUnwindSafe};
use units::Units;

/// Evidence origin for text extracted from a paper (FIGURES.md section 3).
pub const EVIDENCE_ORIGIN: &str = designllm_contracts::figure_rules::PAPER_EVIDENCE_ORIGIN;
/// Automatically extracted evidence is unreviewed until a user confirms it.
pub const REVIEW_UNREVIEWED: &str = "unreviewed";
/// Largest PDF accepted, in bytes.
pub const MAX_PDF_BYTES: u64 = 256 * 1024 * 1024;
/// Largest number of physical pages in one request.
pub const MAX_REQUEST_PAGES: usize = 4096;
/// Message of PDF_PAGE_LABELS_UNREADABLE: the document's labels could not be read, so its pages have no printed label.
const LABELS_UNREADABLE_MESSAGE: &str = "the page labels could not be read; printed page labels are omitted";
/// Message of PDF_PAGE_LABEL_OMITTED: this page's printed label is longer than the supported length, so it is not written.
const PAGE_LABEL_OMITTED_MESSAGE: &str = "a printed page label is omitted: it is longer than the supported length";

/// A diagnostic for a finding that a page counted. Its text is fixed words with the count, so it carries no text of the document.
fn counted_diagnostic(code: &str, count: u32) -> Diagnostic {
    let message = match code {
        "PDF_WIDTH_UNKNOWN" => format!("{count} font(s) on this page have no widths; their glyph advances are 0"),
        "PDF_XOBJECT_MISSING" => {
            format!("{count} use(s) of an XObject that is missing: its name is not in the page resources, or its entry is null or names no object; nothing is drawn for them")
        }
        "PDF_XOBJECT_POSTSCRIPT" => {
            format!("{count} use(s) of a PostScript XObject; PDF readers do not execute it and it draws no text")
        }
        "PDF_TEXT_FROM_ENCODING" => {
            format!("{count} font(s) on this page read some text from their encoding, because ToUnicode has no entry for it")
        }
        "PDF_TOUNICODE_ENTRY_IGNORED" => {
            format!(
                "{count} font(s) on this page have ToUnicode ranges whose destination would overflow its last byte; \
                 their codes past 0xFF are not read"
            )
        }
        _ => "reader diagnostic; see the code".to_string(),
    };
    Diagnostic { code: code.into(), message, node_id: None, blocking: false, params: Default::default() }.with_param("count", count)
}

/// What happened to one requested physical page. Contract types do not implement `PartialEq`.
#[derive(Debug, Clone)]
pub struct PageReport {
    pub physical_page_index: u32,
    /// True when every text-showing operator of the page was interpreted, so its text evidence is complete. An XObject that a
    /// `Do` uses, whose /Subtype is not Form, Image or PS (or is missing), fails the page with PDF_XOBJECT_UNKNOWN, so the
    /// flag is false. Content that is not read (images, annotations, text that is invisible or outside the page) does not
    /// clear this flag. Each such case is listed by a non-blocking diagnostic.
    pub read: bool,
    pub printed_page_label: Option<String>,
    pub geometry: Option<PdfPageGeometry>,
    /// Visible glyphs inside the CropBox that were used for evidence.
    pub visible_glyphs: u32,
    pub excluded_invisible: u32,
    pub excluded_outside_crop: u32,
    pub excluded_clipped: u32,
    pub evidence_count: u32,
    pub diagnostics: Vec<Diagnostic>,
}

/// Result of a partial-tolerant extraction (`extract_pdf_scope`).
#[derive(Debug, Clone)]
pub struct PdfExtraction {
    /// Pages whose content was read successfully, sorted ascending. Never includes a failed page.
    pub coverage: SourceCoverage,
    pub evidence: Vec<Evidence>,
    /// One entry per requested page, sorted ascending.
    pub pages: Vec<PageReport>,
    /// Physical pages in the page tree, verified against every `/Count` of the tree. Not the number of pages read.
    pub page_count: u32,
    /// The requested pages that could not be read, ascending. Each carries the error that the strict path would report for it.
    /// Its message and its diagnostics' messages can contain parser text (lopdf) and document text, so a caller that shows
    /// them to other people keeps only the codes and the page indices.
    pub failures: Vec<PageFailure>,
}

/// A requested page that could not be read, with the error for it. The message is not sanitized (see `PdfExtraction::failures`).
#[derive(Debug, Clone)]
pub struct PageFailure {
    pub physical_page_index: u32,
    pub error: DltError,
}

/// Strict extraction matching the frozen contract. `pages` are 0-based physical page indices. An empty list means every
/// physical page of the document (docs/WORKER_INTERFACES.md), and the pages actually read are reported by
/// `extract_pdf_scope` (`coverage`).
///
/// Every requested page must be read. If any page fails, the call fails and reports the failing pages in
/// `affected_ids`. Its messages are fixed text that names the page: the reader's detail is withheld, and the codes stay.
/// Use `extract_pdf_scope` to receive the partial result with explicit per-page status.
pub fn extract_pdf(asset: &AssetRef, pages: &[u32], assets: &dyn AssetStore) -> DltResult<Vec<Evidence>> {
    let (extraction, failures) = run_scope(asset, pages, assets)?;
    let Some((first_index, first_error)) = failures.first() else {
        return Ok(extraction.evidence);
    };
    let mut error = DltError::from(first_error.clone());
    error.message = withheld_page_message(*first_index);
    error.affected_ids = failures.iter().map(|(index, _)| index.to_string()).collect();
    error.diagnostics = failures
        .iter()
        .map(|(index, err)| {
            let mut diagnostic = err.diagnostic();
            diagnostic.message = withheld_page_message(*index);
            diagnostic
        })
        .collect();
    Err(error)
}

/// The message of a page that could not be read, as the strict path and the page reports give it: the page and no reader text.
fn withheld_page_message(index: u32) -> String {
    format!("physical page {index} could not be read; the reader's detail is withheld")
}

/// Partial-tolerant extraction. Request-level problems (out-of-range pages, too many pages for one request, hash
/// mismatch, a file that cannot be parsed) are errors. Per-page failures are returned in `pages` with `read == false`.
/// An empty `pages` reads every physical page.
///
/// The messages of the diagnostics in `pages` are fixed text with counts, and no document text. The message of a request error
/// (`Err`) can contain parser text from lopdf, and so can the messages in `PdfExtraction::failures`.
pub fn extract_pdf_scope(asset: &AssetRef, pages: &[u32], assets: &dyn AssetStore) -> DltResult<PdfExtraction> {
    run_scope(asset, pages, assets).map(|(extraction, _)| extraction)
}

fn run_scope(
    asset: &AssetRef,
    pages: &[u32],
    assets: &dyn AssetStore,
) -> DltResult<(PdfExtraction, Vec<(u32, PdfErr)>)> {
    // The request's time limit starts here, so it covers reading the asset and parsing the document as well as the pages.
    let request = Request::new();
    let listed = normalize_pages(pages)?;
    let bytes = assets.read(asset)?;
    verify_asset(asset, &bytes)?;
    let doc = load_document(&bytes)?;
    let leaves = page_tree_leaves(&doc).map_err(DltError::from)?;
    let page_count = leaves.len() as u32;
    let requested = resolve_pages(listed, page_count)?;

    // The work of the whole request is counted in `request`: its pages, its label tree and the fonts that its pages load.
    // The reader's own message for a broken label tree can name objects of the document, so it is not copied: the fixed text is.
    let (labels, labels_unreadable) = match boxes::read_labels(&doc, &request.meter) {
        Ok(labels) => (labels, false),
        Err(_) => (Vec::new(), true),
    };

    let source_hash = asset.sha256.to_ascii_lowercase();
    let mut evidence = Vec::new();
    let mut read_indices = Vec::new();
    let mut reports = Vec::with_capacity(requested.len());
    let mut failures = Vec::new();
    for &index in &requested {
        let page_id = leaves[index as usize];
        let outcome = catch_unwind(AssertUnwindSafe(|| read_page(&doc, page_id, index, &labels, &source_hash, &request)));
        match outcome {
            Ok(Ok(page)) => {
                read_indices.push(index);
                let mut report = page.report;
                if labels_unreadable {
                    report.diagnostics.push(Diagnostic {
                        code: "PDF_PAGE_LABELS_UNREADABLE".into(),
                        message: LABELS_UNREADABLE_MESSAGE.into(),
                        node_id: None,
                        blocking: false,
                        params: Default::default(),
                    });
                }
                evidence.extend(page.evidence);
                reports.push(report);
            }
            Ok(Err(err)) => {
                reports.push(failed_report(index, &err));
                failures.push((index, err));
            }
            Err(_) => {
                let err = PdfErr::invalid(PDF_INTERNAL_PANIC, "the PDF interpreter stopped unexpectedly on this page");
                reports.push(failed_report(index, &err));
                failures.push((index, err));
            }
        }
    }

    let page_failures = failures
        .iter()
        .map(|(index, err)| PageFailure { physical_page_index: *index, error: DltError::from(err.clone()) })
        .collect();
    let extraction = PdfExtraction {
        coverage: SourceCoverage { source_hash, physical_page_indices: read_indices },
        evidence,
        pages: reports,
        page_count,
        failures: page_failures,
    };
    Ok((extraction, failures))
}

/// Sorts and de-duplicates the listed pages. An empty list is valid here and means every page (see `resolve_pages`).
fn normalize_pages(pages: &[u32]) -> DltResult<Vec<u32>> {
    if pages.len() > MAX_REQUEST_PAGES {
        return Err(request_error(
            "PDF_TOO_MANY_PAGES",
            format!("at most {MAX_REQUEST_PAGES} pages can be requested at once"),
        ));
    }
    let mut out = pages.to_vec();
    out.sort_unstable();
    out.dedup();
    Ok(out)
}

/// The physical pages a request covers, ascending. An empty request covers every page of the document. Nothing is cut:
/// a document too large for one request is refused with an explicit error that asks for a page range.
fn resolve_pages(listed: Vec<u32>, page_count: u32) -> DltResult<Vec<u32>> {
    if listed.is_empty() {
        if page_count == 0 {
            return Err(request_error("PDF_NO_PAGES", "the document has no pages"));
        }
        if page_count as usize > MAX_REQUEST_PAGES {
            return Err(request_error(
                "PDF_TOO_MANY_PAGES",
                format!(
                    "the document has {page_count} pages and at most {MAX_REQUEST_PAGES} can be read in one request; request a page range"
                ),
            ));
        }
        return Ok((0..page_count).collect());
    }
    if let Some(bad) = listed.iter().find(|&&p| p >= page_count) {
        return Err(out_of_range(*bad, page_count));
    }
    Ok(listed)
}

fn request_error(code: &str, message: impl Into<String>) -> DltError {
    let message = message.into();
    let mut error = DltError::invalid(message.clone());
    error.diagnostics.push(Diagnostic { code: code.into(), message, node_id: None, blocking: true, params: Default::default() });
    error
}

fn out_of_range(index: u32, page_count: u32) -> DltError {
    let mut error = request_error(
        "PDF_PAGE_OUT_OF_RANGE",
        format!("physical page index {index} is out of range; the document has {page_count} pages (0-based)"),
    );
    error.affected_ids = vec![index.to_string()];
    error
}

fn verify_asset(asset: &AssetRef, bytes: &[u8]) -> DltResult<()> {
    if MAX_PDF_BYTES < bytes.len() as u64 {
        return Err(request_error(PDF_TOO_LARGE, "PDF exceeds the supported size"));
    }
    if bytes.len() as u64 != asset.bytes {
        let mut error = DltError::new(
            ErrorCode::IntegrityError,
            format!("asset has {} bytes but its reference declares {}", bytes.len(), asset.bytes),
        );
        error.diagnostics.push(Diagnostic {
            code: "PDF_ASSET_SIZE_MISMATCH".into(),
            message: "asset size differs from its reference".into(),
            node_id: None,
            blocking: true,
            params: Default::default(),
        });
        return Err(error);
    }
    let actual = sha256(bytes);
    if actual != asset.sha256.to_ascii_lowercase() {
        let mut error = DltError::new(ErrorCode::IntegrityError, "asset bytes do not match the declared sha256");
        error.diagnostics.push(Diagnostic {
            code: "PDF_ASSET_HASH_MISMATCH".into(),
            message: format!("expected {}, computed {actual}", asset.sha256),
            node_id: None,
            blocking: true,
            params: Default::default(),
        });
        return Err(error);
    }
    Ok(())
}

/// Stack of the thread that parses a document (task 34b). lopdf recurses once per link of a `/Length` reference chain, at 1.7 to
/// 1.8 KiB per link in a release build (measured 2026-11-18, Windows x64), so a chain of 100,000 links needs about 170 MiB.
/// The stack is reserved address space, and Windows commits its pages only as the thread uses them.
const READER_STACK_BYTES: usize = 256 * 1024 * 1024;

/// Parses the document on a thread with `READER_STACK_BYTES` of stack. A panic in the parser is PDF_INTERNAL_PANIC, and a thread
/// that the system does not start is an internal error. Nothing here refuses a deep chain: a stack overflow ends the process,
/// so the product reads documents in a worker process (task 34c).
fn load_document(bytes: &[u8]) -> DltResult<Document> {
    load_document_with(bytes, Document::load_mem)
}

/// `load_document` with the parser as a parameter, so that a test can make the parser panic or return a document.
fn load_document_with<F>(bytes: &[u8], parse: F) -> DltResult<Document>
where
    F: FnOnce(&[u8]) -> Result<Document, lopdf::Error> + Send,
{
    std::thread::scope(|scope| -> DltResult<Document> {
        // The spawn error has no test: the OS refuses a thread when it runs out of resources (address space for
        // READER_STACK_BYTES, for one), and no test can cause that reliably. The branch passes the OS message on.
        let handle = std::thread::Builder::new()
            .name("designllm-pdf-reader".into())
            .stack_size(READER_STACK_BYTES)
            .spawn_scoped(scope, move || parse(bytes))
            .map_err(|err| DltError::new(ErrorCode::Internal, format!("the PDF reader thread could not be started: {err}")))?;
        match handle.join() {
            Ok(Ok(doc)) => Ok(doc),
            Ok(Err(err)) => Err(DltError::from(PdfErr::from(err))),
            Err(_) => Err(DltError::from(PdfErr::invalid(
                PDF_INTERNAL_PANIC,
                "the PDF parser stopped unexpectedly while loading the document",
            ))),
        }
    })
}

/// Largest depth of a Pages node, counted from the root (the root is depth 0). Real documents nest a few levels. A node
/// deeper than this is refused rather than cut short.
const MAX_PAGE_TREE_DEPTH: usize = 64;

/// Physical pages in tree order, checked against the tree before any page is read. lopdf's `get_pages` checks no `/Count`,
/// drops some broken kids silently, and differs between versions on a kid that fails to load (0.42 drops it, 0.47 counts
/// it as a blank page). So each irregularity is refused here: a kid that is not an indirect reference, a kid that is not a
/// Page or Pages node, a node reached twice, a missing `/Kids` array, a missing or negative `/Count`, a `/Count` that
/// differs from the pages below it, a `/Parent` that does not name the node that lists the node or page, or a tree with no
/// pages.
fn page_tree_leaves(doc: &Document) -> PResult<Vec<ObjectId>> {
    let root = doc
        .catalog()
        .ok()
        .and_then(|catalog| catalog.get(b"Pages").ok())
        .and_then(|object| object.as_reference().ok())
        .ok_or_else(|| PdfErr::invalid("PDF_PAGE_TREE_INVALID", "the document catalog has no /Pages reference"))?;
    let mut leaves = Vec::new();
    let mut visited = BTreeSet::new();
    walk_page_node(doc, root, None, 0, &mut visited, &mut leaves)?;
    if leaves.is_empty() {
        return Err(PdfErr::invalid("PDF_NO_PAGES", "the document has no pages"));
    }
    Ok(leaves)
}

/// Walks one Pages node in order, appends its pages to `leaves`, and returns how many pages lie below it. `parent` is
/// the node that lists `node` in its /Kids, or None for the root.
fn walk_page_node(
    doc: &Document,
    node: ObjectId,
    parent: Option<ObjectId>,
    depth: usize,
    visited: &mut BTreeSet<ObjectId>,
    leaves: &mut Vec<ObjectId>,
) -> PResult<usize> {
    if depth > MAX_PAGE_TREE_DEPTH {
        return Err(PdfErr::invalid("PDF_PAGE_TREE_INVALID", "the page tree is nested deeper than the supported depth"));
    }
    if !visited.insert(node) {
        return Err(PdfErr::invalid("PDF_PAGE_TREE_INVALID", "a page tree node is reached more than once"));
    }
    let dict = tree_dict(doc, node)?;
    if !matches!(dict.get_type(), Ok(b"Pages")) {
        return Err(PdfErr::invalid("PDF_PAGE_TREE_INVALID", "a page tree node is not a Pages node"));
    }
    check_parent(dict, parent)?;
    let declared = dict
        .get_deref(b"Count", doc)
        .and_then(Object::as_i64)
        .ok()
        .and_then(|count| usize::try_from(count).ok())
        .ok_or_else(|| PdfErr::invalid("PDF_PAGE_COUNT_UNREADABLE", "a page tree node has no valid /Count"))?;
    let kids = dict
        .get_deref(b"Kids", doc)
        .and_then(Object::as_array)
        .map_err(|_| PdfErr::invalid("PDF_PAGE_TREE_INVALID", "a page tree node has no /Kids array"))?;
    let mut below = 0usize;
    for kid in kids {
        let kid_id = kid
            .as_reference()
            .map_err(|_| PdfErr::invalid("PDF_PAGE_TREE_INVALID", "a page tree kid is not an indirect reference"))?;
        let kid_dict = tree_dict(doc, kid_id)?;
        match kid_dict.get_type() {
            Ok(b"Page") => {
                check_parent(kid_dict, Some(node))?;
                if !visited.insert(kid_id) {
                    return Err(PdfErr::invalid("PDF_PAGE_TREE_INVALID", "a page is listed more than once in the page tree"));
                }
                leaves.push(kid_id);
                below += 1;
            }
            Ok(b"Pages") => below += walk_page_node(doc, kid_id, Some(node), depth + 1, visited, leaves)?,
            _ => return Err(PdfErr::invalid("PDF_PAGE_TREE_INVALID", "a page tree kid is neither a Page nor a Pages node")),
        }
    }
    if below != declared {
        return Err(PdfErr::invalid(
            "PDF_PAGE_COUNT_MISMATCH",
            format!("a page tree node declares /Count {declared} but has {below} page(s) below it"),
        ));
    }
    Ok(below)
}

/// A page tree node. A reference that does not resolve to a dictionary, or a missing object, is a malformed tree.
fn tree_dict(doc: &Document, node: ObjectId) -> PResult<&Dictionary> {
    doc.get_dictionary(node)
        .map_err(|_| PdfErr::invalid("PDF_PAGE_TREE_INVALID", "a page tree node is missing or is not a dictionary"))
}

/// Inherited attributes (MediaBox, CropBox, Rotate, Resources) are read up the /Parent chain (`boxes::inherited`), so that
/// chain must be the tree path. The root has no /Parent at all. Every other Pages node and every Page must have a /Parent
/// that is a reference to the node listing it. A missing, non-reference or different /Parent refuses the tree.
fn check_parent(dict: &Dictionary, expected: Option<ObjectId>) -> PResult<()> {
    match (dict.get(b"Parent"), expected) {
        (Err(_), None) => Ok(()),
        (Ok(Object::Reference(actual)), Some(parent)) if *actual == parent => Ok(()),
        _ => Err(PdfErr::invalid(
            "PDF_PAGE_TREE_INVALID",
            "a page tree /Parent is missing, is not a reference, or does not name the node that lists it",
        )),
    }
}

fn failed_report(index: u32, err: &PdfErr) -> PageReport {
    // The code and the blocking flag are kept. The reader's message is replaced, since it can carry parser or document text.
    let diagnostic = Diagnostic {
        code: err.code().into(),
        message: withheld_page_message(index),
        node_id: None,
        blocking: true,
        params: Default::default(),
    };
    PageReport {
        physical_page_index: index,
        read: false,
        printed_page_label: None,
        geometry: None,
        visible_glyphs: 0,
        excluded_invisible: 0,
        excluded_outside_crop: 0,
        excluded_clipped: 0,
        evidence_count: 0,
        diagnostics: vec![diagnostic],
    }
}

struct PageRead {
    report: PageReport,
    evidence: Vec<Evidence>,
}

fn pdf_box(units: &Units, rect: Rect4) -> PResult<PdfBox> {
    Ok(PdfBox {
        x_min: units.emu(rect.x0)?,
        y_min: units.emu(rect.y0)?,
        x_max: units.emu(rect.x1)?,
        y_max: units.emu(rect.y1)?,
    })
}

/// Region of a line in the unrotated CropBox frame. Bounds are clamped to the crop, and EMU differences are exact.
fn region_for(units: &Units, crop: Rect4, bbox: [f64; 4]) -> PResult<Option<Rect>> {
    let x0 = bbox[0].max(crop.x0);
    let x1 = bbox[2].min(crop.x1);
    let y0 = bbox[1].max(crop.y0);
    let y1 = bbox[3].min(crop.y1);
    if x1 < x0 || y1 < y0 {
        return Ok(None);
    }
    let left = units.emu(x0)?;
    let right = units.emu(x1)?;
    let top = units.emu(y1)?;
    let bottom = units.emu(y0)?;
    let crop_left = units.emu(crop.x0)?;
    let crop_top = units.emu(crop.y1)?;
    Ok(Some(Rect { x: left - crop_left, y: crop_top - top, width: right - left, height: top - bottom }))
}

/// Annotations listed on a page: the number of entries and how many are not /Link. An entry that cannot be resolved counts as
/// not /Link, and an /Annots that is not an array counts as one entry, so unreadable annotations are still reported.
fn annotation_counts(doc: &Document, page_id: ObjectId, meters: Meters<'_>) -> PResult<(u32, u32)> {
    let Ok(page) = doc.get_dictionary(page_id) else {
        return Ok((1, 1));
    };
    let Ok(listed) = page.get(b"Annots") else {
        return Ok((0, 0));
    };
    // A null list (directly or through a reference) lists nothing, as the PDF null object means an absent entry.
    let entries = match boxes::resolve(doc, listed) {
        Ok(Object::Null) => return Ok((0, 0)),
        Ok(object) => match object.as_array() {
            Ok(entries) => entries,
            Err(_) => return Ok((1, 1)),
        },
        Err(_) => return Ok((1, 1)),
    };
    let mut total = 0u32;
    let mut links = 0u32;
    for entry in entries {
        meters.charge(Unit::Work, 1)?;
        // A null entry, or a reference to null, is not an annotation. A reference that does not resolve to a dictionary
        // is counted as one that is not a link, so the page still reports it.
        let annotation = match entry {
            Object::Null => continue,
            Object::Reference(id) => match doc.get_object(*id) {
                Ok(Object::Null) => continue,
                Ok(Object::Dictionary(dict)) => Some(dict),
                _ => None,
            },
            Object::Dictionary(dict) => Some(dict),
            _ => None,
        };
        total += 1;
        // lopdf decodes `#xx` escapes in names, so `/#4Cink` is `Link` here. An indirect /Subtype is followed.
        if annotation.is_some_and(|dict| matches!(dict.get_deref(b"Subtype", doc).and_then(Object::as_name), Ok(b"Link"))) {
            links += 1;
        }
    }
    Ok((total, total - links))
}

fn read_page(
    doc: &Document,
    page_id: ObjectId,
    index: u32,
    labels: &[LabelRange],
    source_hash: &str,
    request: &Request,
) -> PResult<PageRead> {
    let boxes = boxes::page_boxes(doc, page_id, index)?;
    let units = Units::new(boxes.user_unit)?;
    let label_state = boxes::label_state(labels, index);
    let omitted = label_state == boxes::LabelState::Omitted;
    let label = match label_state {
        boxes::LabelState::Label(text) => Some(text),
        boxes::LabelState::None | boxes::LabelState::Omitted => None,
    };
    let interpreted = content::interpret(doc, page_id, request)?;
    let images = interpreted.images;
    // The evidence and the annotations of the page are charged to the page's meter, and to the request's.
    let meters = Meters { page: &interpreted.page, request: &request.meter };

    let mut diagnostics: Vec<Diagnostic> =
        interpreted.warnings.iter().map(|(code, count)| counted_diagnostic(code, *count)).collect();

    let (mut invisible, mut outside, mut clipped) = (0u32, 0u32, 0u32);
    let mut kept: Vec<RawGlyph> = Vec::new();
    for glyph in interpreted.glyphs {
        if glyph.render_mode == 3 || glyph.render_mode == 7 {
            invisible += 1;
        } else if glyph.clipped {
            clipped += 1;
        } else if !boxes.crop.contains(glyph.origin_x, glyph.origin_y) {
            outside += 1;
        } else {
            kept.push(glyph);
        }
    }
    let info = |code: &str, count: u32, what: &str| {
        Diagnostic {
            code: code.into(),
            message: format!("{count} glyph(s) {what} were not used as evidence"),
            node_id: None,
            blocking: false,
            params: Default::default(),
        }
        .with_param("count", count)
    };
    if invisible > 0 {
        diagnostics.push(info("PDF_INVISIBLE_TEXT_EXCLUDED", invisible, "with invisible text rendering"));
    }
    if clipped > 0 {
        diagnostics.push(info("PDF_FORM_CLIPPED_EXCLUDED", clipped, "outside a form XObject BBox"));
    }
    if outside > 0 {
        diagnostics.push(info("PDF_OUTSIDE_CROP_EXCLUDED", outside, "outside the CropBox"));
    }
    if omitted {
        diagnostics.push(Diagnostic {
            code: "PDF_PAGE_LABEL_OMITTED".into(),
            message: PAGE_LABEL_OMITTED_MESSAGE.into(),
            node_id: None,
            blocking: false,
            params: Default::default(),
        });
    }

    let geometry = PdfPageGeometry {
        media_box: pdf_box(&units, boxes.media)?,
        crop_box: pdf_box(&units, boxes.crop)?,
        user_unit: format!("{}", boxes.user_unit),
        rotation_degrees: boxes.rotate,
        printed_page_label: label.clone(),
    };

    let mut evidence = Vec::new();
    let lines = lines::group_lines(&kept);
    diagnostics.extend(lines::gap_diagnostic(&lines));
    for line in &lines {
        if line.text.trim().is_empty() {
            continue;
        }
        let Some(region) = region_for(&units, boxes.crop, line.bbox)? else {
            continue;
        };
        // One item per line. It is counted before its geometry is copied, and a page or request over its limit fails, never truncates.
        meters.charge(Unit::Evidence, 1)?;
        evidence.push(Evidence {
            id: format!("pdf-{}-p{:05}-l{:03}", &source_hash[..12], index, line.index),
            source_hash: source_hash.to_string(),
            origin: EVIDENCE_ORIGIN.to_string(),
            physical_page_index: Some(index),
            region: Some(region),
            pdf_geometry: Some(geometry.clone()),
            text: line.text.clone(),
            review_state: REVIEW_UNREVIEWED.to_string(),
        });
    }

    if evidence.is_empty() {
        let message = if images == 0 {
            "no text was used as evidence on this page".to_string()
        } else {
            format!(
                "no text was used as evidence on this page; it draws {images} image(s), whose content is not read (OCR is not supported)"
            )
        };
        let no_text = Diagnostic {
            code: "PDF_PAGE_NO_TEXT".into(),
            message,
            node_id: None,
            blocking: false,
            params: Default::default(),
        };
        diagnostics.push(if images > 0 { no_text.with_param("count", images) } else { no_text });
    }
    // Images beside text: their pixels are not read. A page without text reports its images in PDF_PAGE_NO_TEXT above.
    if !evidence.is_empty() && images > 0 {
        diagnostics.push(
            Diagnostic {
                code: "PDF_IMAGE_NOT_READ".into(),
                message: format!("the page draws {images} image(s) whose content is not read (OCR is not supported)"),
                node_id: None,
                blocking: false,
                params: Default::default(),
            }
            .with_param("count", images),
        );
    }
    let (annotations, not_link) = annotation_counts(doc, page_id, meters)?;
    if not_link > 0 {
        diagnostics.push(
            Diagnostic {
                code: "PDF_ANNOTATIONS_NOT_READ".into(),
                message: format!(
                    "the page has {annotations} annotation(s), {not_link} not of type Link; annotation appearance content is not read"
                ),
                node_id: None,
                blocking: false,
                params: Default::default(),
            }
            .with_param("annotations", annotations)
            .with_param("not_link", not_link),
        );
    }

    let report = PageReport {
        physical_page_index: index,
        read: true,
        printed_page_label: label,
        geometry: Some(geometry),
        visible_glyphs: kept.len() as u32,
        excluded_invisible: invisible,
        excluded_outside_crop: outside,
        excluded_clipped: clipped,
        evidence_count: evidence.len() as u32,
        diagnostics,
    };
    Ok(PageRead { report, evidence })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listed_pages_are_sorted_and_deduplicated_and_the_list_is_bounded() {
        assert_eq!(normalize_pages(&[11, 0, 11]).unwrap(), vec![0, 11]);
        assert!(normalize_pages(&[]).unwrap().is_empty());
        let too_many: Vec<u32> = (0..=MAX_REQUEST_PAGES as u32).collect();
        assert_eq!(normalize_pages(&too_many).unwrap_err().diagnostics[0].code, "PDF_TOO_MANY_PAGES");
    }

    #[test]
    fn empty_request_covers_every_physical_page_and_a_listed_page_must_exist() {
        assert_eq!(resolve_pages(vec![], 3).unwrap(), vec![0, 1, 2]);
        assert_eq!(resolve_pages(vec![], 0).unwrap_err().diagnostics[0].code, "PDF_NO_PAGES");
        assert_eq!(resolve_pages(vec![], MAX_REQUEST_PAGES as u32 + 1).unwrap_err().diagnostics[0].code, "PDF_TOO_MANY_PAGES");
        assert_eq!(resolve_pages(vec![2], 3).unwrap(), vec![2]);
        assert_eq!(resolve_pages(vec![3], 3).unwrap_err().diagnostics[0].code, "PDF_PAGE_OUT_OF_RANGE");
    }

    #[test]
    fn region_is_relative_to_crop_top_left_and_unrotated() {
        let units = Units::new(1.0).unwrap();
        let crop = Rect4 { x0: 18.0, y0: 20.0, x1: 594.0, y1: 760.0 };
        // Glyph cell x 90..350, y 680..700 (PDF user space).
        let region = region_for(&units, crop, [90.0, 680.0, 350.0, 700.0]).unwrap().unwrap();
        assert_eq!(region.x, 72 * 12_700); // 914400
        assert_eq!(region.y, 60 * 12_700); // 762000
        assert_eq!(region.width, 260 * 12_700);
        assert_eq!(region.height, 20 * 12_700);
    }

    #[test]
    fn region_outside_crop_is_dropped_and_partial_overlap_is_clamped() {
        let units = Units::new(1.0).unwrap();
        let crop = Rect4 { x0: 18.0, y0: 20.0, x1: 594.0, y1: 760.0 };
        assert!(region_for(&units, crop, [-100.0, -100.0, 0.0, 0.0]).unwrap().is_none());
        let clamped = region_for(&units, crop, [0.0, 700.0, 30.0, 800.0]).unwrap().unwrap();
        assert_eq!(clamped.x, 0);
        assert_eq!(clamped.y, 0);
    }

    #[test]
    fn asset_verification_rejects_wrong_hash_and_size() {
        let bytes = b"%PDF-1.4 test";
        let good = AssetRef { sha256: sha256(bytes), mime: "application/pdf".into(), bytes: bytes.len() as u64 };
        assert!(verify_asset(&good, bytes).is_ok());
        let wrong_hash = AssetRef { sha256: "0".repeat(64), ..good.clone() };
        assert_eq!(verify_asset(&wrong_hash, bytes).unwrap_err().code, ErrorCode::IntegrityError);
        let wrong_size = AssetRef { bytes: 3, ..good };
        assert_eq!(verify_asset(&wrong_size, bytes).unwrap_err().code, ErrorCode::IntegrityError);
    }

    #[test]
    fn the_reader_thread_returns_the_parsers_document_and_a_parser_panic_is_pdf_internal_panic() {
        assert!(load_document_with(b"", |_| Ok(Document::new())).is_ok());
        let Err(error) = load_document_with(b"", |_: &[u8]| -> Result<Document, lopdf::Error> { panic!("the parser panics") }) else {
            panic!("a panic in the parser is an error");
        };
        assert_eq!(error.diagnostics[0].code, PDF_INTERNAL_PANIC);
    }
}
