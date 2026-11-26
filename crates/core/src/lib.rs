//! designllm-core: the single owner of a workspace. SQLite (WAL, synchronous=FULL) holds revision
//! pointers, history, receipts, jobs, proposals and events; model snapshots and assets are
//! immutable content-addressed blobs under `<root>/blobs`. Format engines and the renderer are
//! injected (`EngineAdapter`, `RenderProvider`); core never links a format library.
//!
//! Route and result shapes: crates/core/API.md.
mod b64;
pub mod caller;
mod dataset;
mod design_themes;
mod blobs;
mod errors;
mod files;
mod home;
mod imagesize;
mod fsx;
mod jobs;
mod migration_checkpoints;
mod pdf_read;
mod pipeline;
mod receipts;
mod render_split;
mod revert;
mod routes;
mod scope;
mod store;
mod strict;
mod snapshot_guard;
mod style_patches;
mod ui;
mod views;

pub use blobs::{FaultHook, FaultPoint};
pub use dataset::DatasetEditor;
pub use pdf_read::{current_pdf_read_cancel, Dispatch, PdfCancel, PdfRead};
pub use pipeline::PdfOpen;
pub use receipts::{normalize_request_id, REQUEST_HASH_ALG};
pub use render_split::{RenderJob, RenderedPage};
pub use home::ClientConnection;
pub use views::{emu_to_screen, screen_to_emu, ScreenRect};
pub use migration_checkpoints::{list_migration_checkpoints, MigrationCheckpointSummary};

use blobs::{AssetBase, BlobStore, SharedHook, StagingAssets};
use designllm_contracts::pdf_coverage::{self, marks_contradiction, pages_contradiction, PageContradiction, PageFields};
use designllm_contracts::*;
use errors::{err, integrity, invalid, io_store, unsupported, ErrExt};
use rusqlite::Connection;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// RPC methods routed by [`Workspace::dispatch`].
pub const METHODS: &[&str] = &[
    "system.hello", "workspace.list", "design_system.list", "design_themes.search", "design_themes.record", "design_themes.apply", "artifact.create", "artifact.open", "artifact.inspect",
    "artifact.validate", "artifact.save", "artifact.save_state", "artifact.export", "changes.validate",
    "changes.apply", "history.undo", "history.redo", "render.get", "render.hit_test",
    "render.caret_rect", "render.selection_rects", "asset.import", "asset.read", "asset.lookup",
    "selection.set", "targets.reserve", "targets.release", "view.transform.set", "context.read",
    "figure.render_link", "design_system.publish", "design_system.set",
    "job.start", "job.get", "job.cancel", "job.resume", "proposal.create",
    "proposal.get", "proposal.preview", "proposal.apply", "events.poll", "source.extract_pdf", "source.import_csv", "web.materialize",
    "figure.dataset.edit",
    // P1: one change batch undone alone; reservations (job targets included) of an artifact;
    // agents read and drive the GUI view (docs/UI_CHANNEL.md).
    "history.undo_change", "targets.list", "clients.list", "templates.list", "templates.preview", "flow.styles",
    "ui.state.read", "ui.command.list", "ui.command.run", "ui.command.get", "ui.screenshot",
];
/// Methods for the GUI view itself (docs/UI_CHANNEL.md) and for the bridge of a job (`job.set_client`,
/// after MCP `initialize`). Routed like [`METHODS`], but they are not agent tools: an MCP bridge
/// does not expose them.
pub const CLIENT_METHODS: &[&str] = &["ui.state.publish", "ui.command.complete", "job.set_client", "render.pixels"];
/// Methods a job-bound connection may call. Artifact access stays in the job artifact; Figure rendering has an exact `figure_refs` scope.
pub const BOUND_JOB_METHODS: &[&str] = &[
    "system.hello", "workspace.list", "design_themes.search", "design_themes.record", "design_themes.apply", "artifact.inspect", "artifact.validate", "artifact.save_state",
    "changes.validate", "changes.apply", "history.undo", "history.redo", "render.get", "render.hit_test",
    "render.caret_rect", "render.selection_rects", "asset.read", "asset.lookup", "context.read",
    "figure.render_link",
    "job.get", "proposal.create", "proposal.get", "proposal.preview", "events.poll", "source.extract_pdf", "source.import_csv",
    "figure.dataset.edit",
    // P1
    "history.undo_change", "job.set_client", "clients.list", "flow.styles", "templates.list", "templates.preview",
];
/// Exclusive (no-share) handle held by an open Workspace under its root.
pub const CORE_LOCK_FILE: &str = "core.lock";
/// Largest raw chunk returned by `asset.read` (base64 adds 1/3, still below the 8 MiB frame).
pub const MAX_CHUNK_BYTES: usize = 4 * 1024 * 1024;
/// Largest file accepted by artifact.open / asset.import.
pub const MAX_IMPORT_BYTES: u64 = 1 << 30;
/// Serialized result budget: one response must fit one IPC frame with envelope slack.
pub const RESPONSE_BUDGET: usize = MAX_FRAME_BYTES - 64 * 1024;

pub type PdfHandler = fn(&AssetRef, &[u32], &dyn AssetStore) -> DltResult<Vec<Evidence>>;
pub type CsvHandler = fn(&AssetRef, &dyn AssetStore) -> DltResult<Dataset>;
/// `source.import_csv` with `columns`: receives the caller's names, types and units unchanged
/// (ids empty) instead of inferring them.
pub type CsvDeclaredHandler = fn(&AssetRef, &dyn AssetStore, &[DataColumn]) -> DltResult<Dataset>;
/// Coverage-reporting PDF reader for `source.extract_pdf`: the evidence plus what was read. Registered with
/// [`Workspace::set_pdf_read_handler`]. [`PdfHandler`] stays for callers that need only the evidence.
pub type PdfReadHandler = fn(&AssetRef, &[u32], &dyn AssetStore) -> DltResult<PdfReading>;
pub type PdfEditImporter = fn(&[u8], &PdfCancel) -> DltResult<Vec<u8>>;

/// One requested physical PDF page as the reader reports it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PdfPageReport {
    pub physical_page_index: u32,
    /// True when every text-showing operator of the page was interpreted. Content that is not read (images, annotations, XObjects
    /// of an unknown subtype, text that is invisible or outside the page) does not clear it: a non-blocking diagnostic lists it.
    /// A page read without text has `evidence_count` 0.
    pub read: bool,
    pub evidence_count: u32,
    pub diagnostics: Vec<Diagnostic>,
}

/// What a [`PdfReadHandler`] read for one request. Serde carries it from the PDF reader worker to the host (task 34c).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PdfReading {
    pub evidence: Vec<Evidence>,
    /// Physical pages whose content was read, strictly ascending. Includes pages that produced no evidence.
    pub read_pages: Vec<u32>,
    /// One entry per read page, strictly ascending. An entry with `read == false` refuses the request.
    pub page_reports: Vec<PdfPageReport>,
    /// Physical page count of the document as the reader verified it against the page tree. `None` from a reader that
    /// reports no count (legacy): such a result is accepted but never reported as a complete document.
    pub page_count: Option<u32>,
}

impl PdfReading {
    /// Accepts a reader result only if it matches the request and the asset (fail-closed). An empty `requested` means the
    /// whole document, and then only the consistency checks apply. Checks run in this order:
    /// 1. `read_pages` and `page_reports` are strictly ascending, so no page is listed twice. A reported `page_count` is not zero.
    /// 2. With an explicit request, every read, reported and evidence page is one the request named.
    /// 3. With a `page_count`, no read, reported or evidence page lies outside `0..page_count`.
    /// 4. Every requested page is read and no reported page is unread. The refusal lists those pages and their diagnostics,
    ///    with the reader's message withheld (see `withheld`).
    /// 5. A whole-document request with a `page_count` reads every page in `0..page_count`.
    /// 6. No page marked read carries a blocking diagnostic.
    /// 7. `read_pages` equals the set of `page_reports` pages. Each evidence item has `source_sha256`, sits on a read page,
    ///    and each page's `evidence_count` equals the evidence returned for that page.
    /// A refusal names pages and fixed text only. It never carries evidence text or reader messages.
    pub(crate) fn validate(&mut self, source_sha256: &str, requested: &[u32]) -> DltResult<()> {
        let reports: Vec<(u32, bool)> = self.page_reports.iter().map(|page| (page.physical_page_index, page.read)).collect();
        let evidence_pages: Vec<u32> = self.evidence.iter().filter_map(|item| item.physical_page_index).collect();
        let fields = PageFields { page_count: self.page_count, read_pages: self.read_pages.as_slice(), reports: Some(reports.as_slice()) };
        let read: BTreeSet<u32> = self.read_pages.iter().copied().collect();
        // A page is named when the request asked for it. A whole-document request names every page below the count. The set makes
        // each lookup logarithmic, since a result can carry one evidence item per line of a page.
        let requested_set: BTreeSet<u32> = requested.iter().copied().collect();
        let named = |page: u32| -> bool {
            if requested.is_empty() {
                self.page_count.map_or(true, |count| page < count)
            } else {
                requested_set.contains(&page)
            }
        };
        // Reader detail reaches the caller only for a page the request named, and then only as its code. Any other page
        // gets the code the refusal gives it. Each affected page yields exactly one diagnostic, in ascending page order.
        let diagnostic_for = |page: u32, default_code: &str| -> Diagnostic {
            let chosen = self
                .page_reports
                .iter()
                .find(|report| report.physical_page_index == page && named(page))
                .and_then(|report| report.diagnostics.iter().find(|d| d.blocking).or_else(|| report.diagnostics.first()));
            let mut diagnostic = match chosen {
                Some(detail) => withheld_diagnostic(detail, "reader detail withheld from the refusal; see the code"),
                None => Diagnostic {
                    code: default_code.to_string(),
                    message: String::new(),
                    node_id: None,
                    blocking: true,
                    params: Default::default(),
                },
            };
            diagnostic.message = format!("physical page {page}: reader detail withheld; see the code");
            diagnostic
        };
        let refuse = |message: String, pages: BTreeSet<u32>, default_code: &str| -> DltError {
            let mut error = pdf_reader_refusal(message, pages.iter().copied().collect());
            error.diagnostics = pages.iter().map(|&page| diagnostic_for(page, default_code)).collect();
            error
        };
        let refuse_contradiction = |contradiction: PageContradiction| -> DltError {
            match contradiction {
                PageContradiction::ReadPagesOrder => pdf_reader_refusal("returned read_pages that are not strictly ascending", Vec::new()),
                PageContradiction::ReportsOrder => pdf_reader_refusal("returned page_reports that are not strictly ascending", Vec::new()),
                PageContradiction::ZeroCount => pdf_reader_refusal("reported a page count of zero", Vec::new()),
                PageContradiction::NotRequested(pages) => refuse("returned pages that the request did not name".into(), pages, "PDF_PAGE_REFUSED"),
                // Stage two never reaches a requested page past the count (see `marks_contradiction`). This arm keeps the match
                // exhaustive, with the text that such a page would name.
                PageContradiction::BeyondCount(pages) | PageContradiction::RequestedBeyondCount(pages) => {
                    refuse("returned pages beyond the document's page count".into(), pages, "PDF_PAGE_REFUSED")
                }
                PageContradiction::ReadMismatch(pages) => {
                    refuse("returned read_pages that differ from the pages in page_reports".into(), pages, "PDF_PAGE_REFUSED")
                }
            }
        };
        if let Some(contradiction) = pages_contradiction(requested, &evidence_pages, &fields) {
            return Err(refuse_contradiction(contradiction));
        }

        let mut unread: BTreeSet<u32> = requested.iter().copied().filter(|page| !read.contains(page)).collect();
        unread.extend(self.page_reports.iter().filter(|page| !page.read).map(|page| page.physical_page_index));
        if !unread.is_empty() {
            let message = format!("left {} page(s) unread without failing the request", unread.len());
            return Err(refuse(message, unread, "PDF_PAGE_UNREAD"));
        }

        if requested.is_empty() {
            if let Some(count) = self.page_count {
                let missing: BTreeSet<u32> = (0..count).filter(|page| !read.contains(page)).collect();
                if !missing.is_empty() {
                    let message = format!("left {} of the document's {count} page(s) unread without failing the request", missing.len());
                    return Err(refuse(message, missing, "PDF_PAGE_UNREAD"));
                }
            }
        }

        let blocked: BTreeSet<u32> = self
            .page_reports
            .iter()
            .filter(|page| page.read && page.diagnostics.iter().any(|diagnostic| diagnostic.blocking))
            .map(|page| page.physical_page_index)
            .collect();
        if !blocked.is_empty() {
            return Err(refuse("reported blocking diagnostics on pages it marked as read".into(), blocked, "PDF_PAGE_REFUSED"));
        }

        if let Some(contradiction) = marks_contradiction(requested, &fields) {
            return Err(refuse_contradiction(contradiction));
        }
        let mut evidence_per_page: BTreeMap<u32, u32> = BTreeMap::new();
        for item in &self.evidence {
            if item.source_hash != source_sha256 {
                return Err(pdf_reader_refusal("returned evidence from a different source", Vec::new()));
            }
            let Some(page) = item.physical_page_index else {
                return Err(pdf_reader_refusal("returned evidence without a physical page", Vec::new()));
            };
            if !read.contains(&page) {
                let message = "returned evidence on a page it did not read".into();
                return Err(refuse(message, BTreeSet::from([page]), "PDF_PAGE_REFUSED"));
            }
            *evidence_per_page.entry(page).or_insert(0) += 1;
        }
        for report in &self.page_reports {
            let found = evidence_per_page.get(&report.physical_page_index).copied().unwrap_or(0);
            if found != report.evidence_count {
                let message = "returned an evidence_count that differs from the evidence on that page".into();
                return Err(refuse(message, BTreeSet::from([report.physical_page_index]), "PDF_PAGE_REFUSED"));
            }
        }
        self.sanitize_page_diagnostics();
        Ok(())
    }

    /// Rewrites the page diagnostics of a result that is about to be returned. Each keeps its code, its blocking flag and its
    /// counts, and takes the fixed text of its code. A page has one diagnostic per code. The reader's messages are not passed
    /// on, so they cannot carry text of the document or of the parser. Failed pages are rewritten the same way.
    fn sanitize_page_diagnostics(&mut self) {
        for page in &mut self.page_reports {
            let mut kept: Vec<Diagnostic> = Vec::new();
            for diagnostic in std::mem::take(&mut page.diagnostics) {
                let Diagnostic { code, blocking, params, .. } = diagnostic;
                let code = shown_reader_code(&code);
                let mut params = numeric_params(&params);
                match kept.iter().position(|k| k.code == code) {
                    Some(at) => {
                        // A repeated code adds its counts to the diagnostic kept for it, and stays blocking if either is.
                        let total = match (params_count(&kept[at].params), params_count(&params)) {
                            (None, None) => None,
                            (first, more) => Some(first.unwrap_or(0).saturating_add(more.unwrap_or(0))),
                        };
                        if let Some(total) = total {
                            params.insert("count".to_string(), Value::from(total));
                        }
                        let blocking = blocking || kept[at].blocking;
                        kept[at] = success_diagnostic(&code, blocking, params);
                    }
                    None => kept.push(success_diagnostic(&code, blocking, params)),
                }
            }
            page.diagnostics = kept;
        }
    }

    /// True only when the reader reported the page count and `read_pages` is exactly every physical page, in order.
    /// Call it after [`PdfReading::validate`], which has checked the pages.
    pub(crate) fn document_complete(&self) -> bool {
        pdf_coverage::document_complete(self.page_count, self.read_pages.iter().copied())
    }
}

/// Refusal of a PDF reader result. It carries the given pages and fixed text only, never reader strings or evidence text.
fn pdf_reader_refusal(message: impl Into<String>, pages: Vec<u32>) -> DltError {
    let mut error = DltError::new(ErrorCode::Internal, format!("the PDF reader {}", message.into()));
    error.affected_ids = pages.iter().map(|page| page.to_string()).collect();
    error
}

/// The one withholding rule of PDF reader diagnostics. A diagnostic that leaves the reader keeps its code and its blocking flag,
/// and takes the given fixed text: the reader's message can name fonts, streams or other details of the document. Callers that
/// refuse a result (this crate and the application crate, which withholds the errors of the reader) all go through it.
pub fn withheld_diagnostic(diagnostic: &Diagnostic, message: impl Into<String>) -> Diagnostic {
    Diagnostic {
        code: shown_reader_code(&diagnostic.code),
        message: message.into(),
        node_id: None,
        blocking: diagnostic.blocking,
        params: Default::default(),
    }
}

/// The code of a reader diagnostic as it may be shown: a plain upper-case identifier, else `PDF_READER_DIAGNOSTIC`.
pub fn shown_reader_code(code: &str) -> String {
    if is_diagnostic_code(code) { code.to_string() } else { "PDF_READER_DIAGNOSTIC".to_string() }
}

fn is_diagnostic_code(code: &str) -> bool {
    (1..=64).contains(&code.len()) && code.bytes().all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
}

/// The counts that a success result may carry. Other parameters are dropped, so no document value crosses the boundary.
fn numeric_params(params: &BTreeMap<String, Value>) -> BTreeMap<String, Value> {
    params
        .iter()
        .filter(|(key, value)| matches!(key.as_str(), "count" | "annotations" | "not_link") && value.is_u64())
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

fn params_count(params: &BTreeMap<String, Value>) -> Option<u64> {
    params.get("count").and_then(Value::as_u64)
}

/// A page diagnostic of a result: the fixed text of its code, with its counts. The blocking flag is the reader's.
fn success_diagnostic(code: &str, blocking: bool, params: BTreeMap<String, Value>) -> Diagnostic {
    Diagnostic { code: code.to_string(), message: success_text(code, &params), node_id: None, blocking, params }
}

/// The text of a page diagnostic of a successful result, by code. The words are fixed. A count is written as a number. No text of
/// the document and no reader message is in it.
fn success_text(code: &str, params: &BTreeMap<String, Value>) -> String {
    let count = params_count(params);
    let n = count.map_or_else(String::new, |n| format!("{n} "));
    match code {
        "PDF_PAGE_NO_TEXT" => match count {
            Some(images) => format!(
                "no text was used as evidence on this page; it draws {images} image(s), whose content is not read (OCR is not supported)"
            ),
            None => "no text was used as evidence on this page".to_string(),
        },
        "PDF_IMAGE_NOT_READ" => format!("the page draws {n}image(s) whose content is not read (OCR is not supported)"),
        "PDF_ANNOTATIONS_NOT_READ" => {
            match (params.get("annotations").and_then(Value::as_u64), params.get("not_link").and_then(Value::as_u64)) {
                (Some(total), Some(not_link)) => format!(
                    "the page has {total} annotation(s), {not_link} not of type Link; annotation appearance content is not read"
                ),
                _ => "the page has annotations that are not of type Link; annotation appearance content is not read".to_string(),
            }
        }
        "PDF_INVISIBLE_TEXT_EXCLUDED" => format!("{n}glyph(s) with invisible text rendering were not used as evidence"),
        "PDF_FORM_CLIPPED_EXCLUDED" => format!("{n}glyph(s) outside a form XObject BBox were not used as evidence"),
        "PDF_OUTSIDE_CROP_EXCLUDED" => format!("{n}glyph(s) outside the CropBox were not used as evidence"),
        "PDF_WIDTH_UNKNOWN" => format!("{n}font(s) on this page have no widths; their glyph advances are 0"),
        "PDF_TEXT_FROM_ENCODING" => {
            format!("{n}font(s) on this page read some text from their encoding, because ToUnicode has no entry for it")
        }
        "PDF_TYPE1_ENCODING_UNREADABLE" => format!(
            "{n}embedded Type 1 font(s) on this page have a built-in encoding that is not read; some of their text is read with the standard encoding instead"
        ),
        "PDF_LINE_GAP_AMBIGUOUS" => {
            format!("{n}gap(s) of 0.6 to 1.5 font sizes were joined; a column gap of that width is not told apart from a word space")
        }
        "PDF_XOBJECT_MISSING" => {
            format!("{n}use(s) of an XObject that is missing: its name is not in the page resources, or its entry is null or names no object; nothing is drawn for them")
        }
        // PDF_XOBJECT_UNKNOWN is blocking: its page is refused, so a success result never carries it and it has no text here.
        "PDF_XOBJECT_POSTSCRIPT" => {
            format!("{n}use(s) of a PostScript XObject; PDF readers do not execute it and it draws no text")
        }
        "PDF_PAGE_LABELS_UNREADABLE" => "the page labels could not be read; printed page labels are omitted".to_string(),
        "PDF_PAGE_LABEL_OMITTED" => "a printed page label is omitted: it is longer than the supported length".to_string(),
        _ => "reader detail withheld; see the code".to_string(),
    }
}

/// Result of [`Workspace::events_after`] and `events.poll`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventBatch {
    pub events: Vec<CoreEvent>,
    pub last_seq: u64,
    /// `after_seq` is outside the retained range (or ahead of this workspace): re-read state.
    pub resync_required: bool,
    /// Current artifact summaries, filled when `resync_required`.
    #[serde(default)]
    pub artifacts: Vec<ArtifactSummary>,
    /// P1: with `view_id` + `view_token`, that GUI view's queued command runs (each handed out once).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ui_runs: Vec<UiRun>,
}

/// What open-time recovery did. Foreign entries in the blob directory are reported, never deleted.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct RecoveryReport {
    pub removed_temp_blobs: usize,
    pub removed_unreferenced_blobs: usize,
    pub removed_pending_temps: usize,
    /// Native saves whose target already held exactly the bytes being written at the crash.
    pub recovered_saves: usize,
    pub foreign_entries: Vec<String>,
    pub verified_current_snapshots: usize,
    pub pruned_events: usize,
    /// Warnings about preserved, unregistered or interrupted schema-migration staging files.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub migration_warnings: Vec<String>,
}

pub struct Workspace {
    // Field order is drop order: database first, owner lock last.
    pub(crate) conn: Connection,
    pub(crate) base: Arc<AssetBase>,
    pub(crate) hook: SharedHook,
    pub(crate) engines: Vec<Arc<dyn EngineAdapter>>,
    pub(crate) by_kind: BTreeMap<ArtifactKind, usize>,
    pub(crate) by_id: HashMap<String, usize>,
    pub(crate) renderer: Arc<dyn RenderProvider>,
    pub(crate) root: PathBuf,
    pub(crate) id: String,
    pub(crate) pdf: Option<PdfHandler>,
    pub(crate) pdf_read: Option<PdfReadHandler>,
    pub(crate) pdf_edit_importer: Option<PdfEditImporter>,
    pub(crate) csv: Option<CsvHandler>,
    pub(crate) csv_declared: Option<CsvDeclaredHandler>,
    pub(crate) dataset_editor: Option<DatasetEditor>,
    pub(crate) views: views::Views,
    pub(crate) renders: views::RenderCache,
    pub(crate) ui: ui::UiChannel,
    pub(crate) clients: home::Clients,
    pub(crate) template_previews: home::TemplatePreviewCache,
    pub(crate) job_template_previews: home::JobPreviews,
    pub(crate) recovery: RecoveryReport,
    /// Who named the file paths of the request being dispatched (`files::PathCaller`, set by `dispatch_start_gui`).
    pub(crate) path_caller: files::PathCaller,
    /// The host-fixed actor prefix of the connection whose request is being dispatched (`caller`); None in process and for the gui.
    pub(crate) caller: Option<String>,
    _lock: File,
}

/// A validated backup restored into a distinct new workspace folder.
pub struct RestoredWorkspace {
    pub workspace: Workspace,
    /// Includes a warning when the source checkpoint predates the old-core refusal marker.
    pub diagnostics: Vec<Diagnostic>,
}

fn acquire_lock(path: &Path) -> DltResult<File> {
    let mut opts = OpenOptions::new();
    opts.read(true).write(true).create(true).truncate(false);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // No sharing: a second handle (same or other process) fails until this one is closed,
        // which the OS also does when the owning process dies.
        opts.share_mode(0);
    }
    match opts.open(path) {
        Ok(f) => {
            #[cfg(not(windows))]
            match f.try_lock() {
                Ok(()) => {}
                Err(std::fs::TryLockError::WouldBlock) => return Err(err(ErrorCode::WorkspaceLocked, "workspace is owned by another process or handle")),
                Err(std::fs::TryLockError::Error(error)) => return Err(io_store(error)),
            }
            Ok(f)
        }
        // ERROR_SHARING_VIOLATION / ERROR_LOCK_VIOLATION
        Err(e) if matches!(e.raw_os_error(), Some(32) | Some(33)) => Err(err(ErrorCode::WorkspaceLocked, "workspace is owned by another process or handle")),
        Err(e) => Err(io_store(e)),
    }
}

pub(crate) fn acquire_restore_source_lock(root: &Path) -> DltResult<File> {
    let path = root.join(CORE_LOCK_FILE);
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Err(restore_source_lock_unavailable()),
        Err(error) => return Err(io_store(error)),
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(integrity("source workspace lock path is not a regular file"));
    }
    let mut opts = OpenOptions::new();
    opts.read(true).write(true).create(false).truncate(false);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // Match Workspace::open: no other process or handle may open the owner lock meanwhile.
        opts.share_mode(0);
    }
    let file = opts.open(&path).map_err(|error| {
        if matches!(error.raw_os_error(), Some(32) | Some(33)) {
            restore_source_lock_unavailable()
        } else {
            io_store(error)
        }
    })?;
    #[cfg(not(windows))]
    match file.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => return Err(restore_source_lock_unavailable()),
        Err(std::fs::TryLockError::Error(error)) => return Err(io_store(error)),
    }
    Ok(file)
}

fn restore_source_lock_unavailable() -> DltError {
    let message = "원본 작업공간의 잠금 상태를 확인할 수 없어 백업 복원을 중단했습니다. 현재 앱에서 해당 작업공간을 열고 닫은 뒤 다시 시도하세요. 이 시도에서는 원본이나 대상 폴더를 변경하지 않았습니다.";
    let mut error = err(ErrorCode::WorkspaceLocked, message);
    error.diagnostics.push(Diagnostic {
        code: "core.restore_source_lock_unavailable".into(),
        message: message.into(),
        node_id: None,
        blocking: true,
        params: Default::default(),
    });
    error
}

/// Request params as routed: an absent (null) body is the empty object.
pub(crate) fn object_params(params: Value) -> Value {
    if params.is_null() { json!({}) } else { params }
}

/// The response to one routed request: its result, unless the result is over the frame budget.
pub(crate) fn respond(id: String, method: &str, result: DltResult<Value>) -> RpcResponse {
    match result.and_then(|value| {
        let size = serde_json::to_vec(&value).map(|v| v.len()).unwrap_or(usize::MAX);
        if size > RESPONSE_BUDGET {
            return Err(invalid(format!("{method} result is {size} bytes, over the {RESPONSE_BUDGET}-byte frame budget; request a narrower range (node_ids, page_index, pages, offset/max_bytes)")));
        }
        Ok(value)
    }) {
        Ok(value) => RpcResponse::success(id, value),
        Err(e) => RpcResponse::failure(id, e),
    }
}

/// Typed route params. A key the typed model does not have, at any depth, is INVALID_ARGUMENT
/// naming its JSON pointer (never silently dropped). Runs before receipts, so it is not stored.
pub(crate) fn parse<T: DeserializeOwned + Serialize>(params: Value) -> DltResult<T> { strict::parse_at(&params, "") }
pub(crate) fn to_value<T: Serialize>(value: &T) -> DltResult<Value> { serde_json::to_value(value).map_err(|e| err(ErrorCode::Internal, e.to_string())) }

/// Collect the sha256 of every AssetRef-shaped object (`{sha256,mime,bytes}`) in a JSON tree.
pub(crate) fn asset_refs(value: &Value, out: &mut BTreeSet<String>) {
    match value {
        Value::Object(m) => {
            if m.len() == 3 && m.get("mime").is_some_and(Value::is_string) && m.get("bytes").is_some_and(Value::is_u64) {
                if let Some(h) = m.get("sha256").and_then(Value::as_str).filter(|h| blobs::is_hash(h)) { out.insert(h.to_string()); }
            }
            for v in m.values() { asset_refs(v, out); }
        }
        Value::Array(items) => for v in items { asset_refs(v, out); },
        _ => {}
    }
}

impl Workspace {
    /// Opens (or initializes) the workspace at `root`, takes the exclusive owner lock and runs
    /// crash recovery. Engines are registered by `id()` and by every kind they declare.
    pub fn open(root: &Path, engines: Vec<Arc<dyn EngineAdapter>>, renderer: Arc<dyn RenderProvider>) -> DltResult<Self> {
        Self::open_internal(root, engines, renderer, false)
    }

    /// Restores a registered migration checkpoint into a new folder and opens the migrated copy.
    /// The source workspace and checkpoint are read-only. A legacy unmarked checkpoint is
    /// accepted after the same integrity, pin, raw-snapshot and typed-restore checks, and returns
    /// a diagnostic because an older core may still open that raw file directly.
    pub fn restore_backup_to_new_folder(
        source_root: &Path,
        backup_id: &str,
        destination_root: &Path,
        engines: Vec<Arc<dyn EngineAdapter>>,
        renderer: Arc<dyn RenderProvider>,
    ) -> DltResult<RestoredWorkspace> {
        let supported = store::supported_persisted_features(&engines)?;
        let stage = store::prepare_backup_restore_stage(source_root, backup_id, destination_root, &engines, &supported)?;
        let staged = match Self::open_internal(&stage.staging_root, engines.clone(), renderer.clone(), true) {
            Ok(workspace) => workspace,
            Err(error) => return Err(store::restore_stage_error(error, &stage)),
        };
        drop(staged);
        if let Err(error) = store::mark_backup_restore_stage_ready(&stage) {
            return Err(store::restore_stage_error(error, &stage));
        }
        if let Err(error) = store::publish_backup_restore_stage(&stage) {
            return Err(store::restore_stage_error(error, &stage));
        }
        let workspace = Self::open(&stage.destination_root, engines, renderer).map_err(|mut error| {
            error.message = format!("{}; the restored workspace was published at {} and was preserved", error.message, stage.destination_root.display());
            error
        })?;
        let mut diagnostics = stage.diagnostics.clone();
        if let Err(error) = store::remove_published_restore_owner(&stage) {
            diagnostics.push(Diagnostic {
                code: "core.restore_backup_incomplete_left".into(),
                message: format!("The restored workspace is ready, but its restore marker remains: {}", error.message),
                node_id: None,
                blocking: false,
                params: Default::default(),
            });
        }
        Ok(RestoredWorkspace { workspace, diagnostics })
    }

    fn open_internal(root: &Path, engines: Vec<Arc<dyn EngineAdapter>>, renderer: Arc<dyn RenderProvider>, allow_backup_restore: bool) -> DltResult<Self> {
        let mut by_kind = BTreeMap::new();
        let mut by_id = HashMap::new();
        for (i, engine) in engines.iter().enumerate() {
            let id = engine.id();
            if id.trim().is_empty() { return Err(invalid("engine id is empty")); }
            if by_id.insert(id.to_string(), i).is_some() { return Err(invalid(format!("engine id {id:?} is registered twice"))); }
            for kind in engine.kinds() {
                if let Some(prev) = by_kind.insert(kind, i) {
                    return Err(invalid(format!("{kind:?} is claimed by both {:?} and {id:?}", engines[prev].id())));
                }
            }
        }
        // Reject ambiguous/unsupported extended Windows paths before creating the workspace
        // directory or owner lock. In particular, do not reinterpret `.`/`..` after adding `\\?\`.
        #[cfg(windows)]
        store::validate_workspace_path(root)?;
        fs::create_dir_all(root).map_err(io_store)?;
        let lock = acquire_lock(&root.join(CORE_LOCK_FILE))?;
        let supported_features = store::supported_persisted_features(&engines)?;
        let mut pre_migration_recovery = RecoveryReport::default();
        let (mut conn, id) = if allow_backup_restore {
            store::open_db_with_engines_for_backup_restore(root, &mut pre_migration_recovery, &engines, &supported_features)?
        } else {
            store::open_db_with_engines(root, &mut pre_migration_recovery, &engines, &supported_features)?
        };
        // Reject unsupported workspace features before opening the blob store or restoring any
        // Artifact snapshot. A pre-store3 core is rejected earlier by open_db's schema gate.
        store::ensure_store_features_supported(&conn, &supported_features)?;
        let hook: SharedHook = Arc::new(Mutex::new(None));
        let blobs = BlobStore::open(root.join(store::BLOB_DIR), hook.clone())?;
        store::backfill_store_features(&mut conn, &blobs, &engines, &supported_features)?;
        store::ensure_store_features_supported(&conn, &supported_features)?;
        store::validate_history_groups(&conn)?;
        let mut recovery = recover(&mut conn, &blobs, &engines, &by_id)?;
        recovery.removed_pending_temps += pre_migration_recovery.removed_pending_temps;
        recovery.recovered_saves += pre_migration_recovery.recovered_saves;
        recovery.pruned_events = conn
            .execute("DELETE FROM events WHERE event_seq <= (SELECT COALESCE(MAX(event_seq), 0) FROM events) - ?1", rusqlite::params![store::EVENT_RETENTION])
            .map_err(errors::sql)?;
        let base = Arc::new(AssetBase::new(blobs, store::asset_rows(&conn)?));
        let mut ws = Self {
            conn, base, hook, engines, by_kind, by_id, renderer, root: root.to_path_buf(), id, pdf: None, pdf_read: None, pdf_edit_importer: None, csv: None, csv_declared: None, dataset_editor: None,
            views: views::Views::default(), renders: views::RenderCache::default(), ui: ui::UiChannel::default(), clients: Default::default(), template_previews: Default::default(), job_template_previews: Default::default(), recovery, path_caller: Default::default(), caller: None, _lock: lock,
        };
        // Reservations live in memory: active jobs with target_ids hold theirs again.
        ws.restore_job_reservations()?;
        match store::acknowledge_migration_recovery(&ws.root, &ws.conn) {
            Ok(warnings) => ws.recovery.migration_warnings.extend(warnings),
            Err(error) => ws.recovery.migration_warnings.push(format!("migration recovery report marker was preserved because it could not be acknowledged: {}", error.message)),
        }
        Ok(ws)
    }
    pub fn root(&self) -> &Path { &self.root }
    pub fn id(&self) -> &str { &self.id }
    pub fn recovery_report(&self) -> &RecoveryReport { &self.recovery }
    pub fn registered_kinds(&self) -> Vec<ArtifactKind> { self.by_kind.keys().copied().collect() }
    /// Root wires designllm_sources::extract_pdf and designllm_figures::import_csv here so core
    /// does not depend on source/format crates. Used by `source.extract_pdf` / `source.import_csv`.
    pub fn set_source_handlers(&mut self, pdf: PdfHandler, csv: CsvHandler) { self.pdf = Some(pdf); self.csv = Some(csv); }
    /// Root wires designllm_figures::import_csv here alone, for a root that reads PDFs through `set_pdf_read_handler`: the legacy
    /// PDF handler is then never reached by `source.extract_pdf`.
    pub fn set_csv_handler(&mut self, csv: CsvHandler) { self.csv = Some(csv); }
    /// Root wires the coverage-reporting reader here (the application crate adapts `designllm_sources::extract_pdf_scope`).
    /// When set, `source.extract_pdf` uses it and also returns `read_pages` and `page_reports`. Its result must match the
    /// request and the asset, or the request is refused (see `PdfReading::validate`). It takes precedence over the PDF
    /// handler of `set_source_handlers`, which that route then does not call.
    pub fn set_pdf_read_handler(&mut self, reader: PdfReadHandler) { self.pdf_read = Some(reader); }
    pub fn set_pdf_edit_importer(&mut self, importer: PdfEditImporter) { self.pdf_edit_importer = Some(importer); }
    /// Root wires designllm_figures::import_csv_declared here. Used by `source.import_csv` when the
    /// request carries `columns`; independent of the inference handler above.
    pub fn set_csv_declared_handler(&mut self, handler: CsvDeclaredHandler) { self.csv_declared = Some(handler); }
    /// Test-only fault injection at durability points (blob/DB/native publication).
    #[doc(hidden)]
    pub fn set_fault_hook(&mut self, hook: Option<FaultHook>) { if let Ok(mut h) = self.hook.lock() { *h = hook; } }

    pub fn dispatch(&mut self, request: RpcRequest) -> RpcResponse { self.dispatch_bound(request, None) }

    /// `bound_job` is the host-enforced job binding of the calling connection. When present,
    /// only [`BOUND_JOB_METHODS`] are allowed, artifact access stays in the job's artifact except
    /// for exact Figure revisions pinned in `figure_refs`, and mutations run as that attempt
    /// whether or not the body carries a job_ref.
    pub fn dispatch_bound(&mut self, request: RpcRequest, bound_job: Option<JobRef>) -> RpcResponse {
        let RpcRequest { protocol_version, id, method, params } = request;
        if protocol_version != PROTOCOL_VERSION {
            return RpcResponse::failure(id, err(ErrorCode::ProtocolMismatch, format!("protocol_version {protocol_version} is not {PROTOCOL_VERSION}")));
        }
        let mut params = object_params(params);
        if files::take_agent_path(&mut params) { self.path_caller = files::PathCaller::Agent; }
        let result = match &bound_job {
            Some(job) => self.bound_scope(&method, params, job).and_then(|p| self.route(&method, p, Some(job))),
            None => self.route(&method, params, None),
        };
        respond(id, &method, result)
    }

    fn route(&mut self, method: &str, params: Value, job: Option<&JobRef>) -> DltResult<Value> {
        match method {
            "system.hello" => Ok(json!({
                "workspace_id": self.id, "protocol_version": PROTOCOL_VERSION, "schema_version": SCHEMA_VERSION,
                "registered_kinds": self.registered_kinds(), "methods": METHODS, "client_methods": CLIENT_METHODS, "sqlite_version": rusqlite::version(),
            })),
            "workspace.list" => self.list(job),
            "design_system.list" => { let _: home::NoParams = parse(params)?; self.design_system_list() }
            "design_themes.search" => design_themes::search(&self.conn, parse(params)?),
            "design_themes.record" => design_themes::record(&mut self.conn, parse(params)?),
            "design_themes.apply" => self.apply_design_theme(params, job),
            "artifact.create" => self.create_artifact(parse(params)?),
            "artifact.open" => self.open_artifact(parse(params)?),
            "artifact.inspect" => self.inspect(parse(params)?),
            "artifact.validate" => self.validate_artifact(parse(params)?),
            "artifact.save" => self.save(parse(params)?),
            "artifact.save_state" => self.save_state(parse(params)?),
            "artifact.export" => self.export(parse(params)?),
            "changes.validate" => self.validate_changes(params, job),
            "changes.apply" => self.apply_changes(params, job),
            "history.undo" => self.history(params, false, job),
            "history.redo" => self.history(params, true, job),
            "history.undo_change" => self.undo_change(params, job),
            "render.get" => self.render_get(parse(params)?),
            "render.pixels" => Err(render_split::pixels_in_process()),
            "render.hit_test" | "render.caret_rect" | "render.selection_rects" => self.render_query(method, parse(params)?),
            "asset.import" => self.asset_import(parse(params)?),
            "asset.read" => self.asset_read(parse(params)?, job),
            "asset.lookup" => self.asset_lookup(parse(params)?, job),
            "source.extract_pdf" => self.source_extract_pdf(parse(params)?, job),
            "source.import_csv" => self.source_import_csv(parse(params)?, job),
            "selection.set" => self.selection_set(parse(params)?),
            "targets.reserve" => self.targets_reserve(parse(params)?),
            "targets.release" => self.targets_release(parse(params)?),
            "targets.list" => self.targets_list(parse(params)?),
            "clients.list" => { let _: home::NoParams = parse(params)?; self.clients_list(job) }
            "templates.list" => { let _: home::NoParams = parse(params)?; self.templates_list() }
            "templates.preview" => self.templates_preview(parse(params)?, job),
            "flow.styles" => self.flow_styles(parse(params)?),
            "view.transform.set" => self.view_transform_set(parse(params)?),
            "context.read" => self.context_read(parse(params)?, job),
            "figure.render_link" => self.figure_render_link(parse(params)?, job),
            "design_system.publish" => self.design_system_publish(params),
            "design_system.set" => self.design_system_set(params),
            "web.materialize" => self.web_materialize(parse(params)?),
            "figure.dataset.edit" => self.dataset_edit(params, job),
            "job.start" => self.job_start(params),
            "job.get" => { let p: routes::JobIdParams = parse(params)?; to_value(&self.job(&p.job_id)?) }
            "job.cancel" => { let p: routes::JobIdParams = parse(params)?; let job = self.cancel_job_for_attempt(&p.job_id, p.attempt_id.as_deref())?; self.prune_job_previews(); to_value(&job) }
            "job.resume" => { let p: routes::JobIdParams = parse(params)?; to_value(&self.resume_job(&p.job_id)?) }
            "job.set_client" => self.set_job_client(parse(params)?, job),
            "proposal.create" => self.proposal_create(params, job),
            "proposal.get" => self.proposal_get(parse(params)?),
            "proposal.preview" => self.proposal_preview(parse(params)?, job),
            "proposal.apply" => self.proposal_apply(parse(params)?),
            "events.poll" => { let p: routes::PollParams = parse(params)?; to_value(&self.poll(p, job)?) }
            "ui.state.publish" => self.ui_publish(parse(params)?),
            "ui.state.read" => self.ui_state_read(parse(params)?),
            "ui.command.list" => self.ui_command_list(parse(params)?),
            "ui.command.run" => self.ui_command_run(parse(params)?),
            "ui.command.get" => self.ui_command_get(parse(params)?),
            "ui.command.complete" => self.ui_command_complete(parse(params)?),
            "ui.screenshot" => self.ui_screenshot(parse(params)?),
            "host.stop" => Err(unsupported("host.stop is handled by the host process, not the core")),
            other => Err(unsupported(format!("unknown method {other:?}"))),
        }
    }

    /// Method allowlist and per-method scope for job-bound connections.
    fn bound_scope(&mut self, method: &str, mut params: Value, job: &JobRef) -> DltResult<Value> {
        if !BOUND_JOB_METHODS.contains(&method) {
            return Err(unsupported(format!("{method} is not permitted on a job-bound connection")));
        }
        let row = store::job_row(&self.conn, &job.job_id)?;
        if store::attempt_state(&self.conn, &job.job_id, &job.attempt_id)?.is_none() {
            return Err(errors::not_found(format!("attempt {} of job {} not found", job.attempt_id, job.job_id)));
        }
        let scoped = |id: Option<&str>| -> DltResult<()> {
            match id {
                Some(a) if a != row.artifact_id => Err(err(ErrorCode::InvalidArgument, format!("job {} may only access artifact {}", job.job_id, row.artifact_id)).ids([a.to_string()])),
                _ => Ok(()),
            }
        };
        match method {
            "design_themes.record" => {
                if row.current_attempt != job.attempt_id || store::attempt_state(&self.conn, &job.job_id, &job.attempt_id)?.as_deref() != Some("active") {
                    return Err(err(ErrorCode::JobNotActive, "the job attempt is not active"));
                }
            }
            "proposal.create" => scoped(params.get("change_set").and_then(|c| c.get("artifact_id")).and_then(Value::as_str))?,
            "job.get" => {
                if params.get("job_id").and_then(Value::as_str) != Some(job.job_id.as_str()) { return Err(invalid("a job-bound connection may only read its own job")); }
            }
            "context.read" => {
                scoped(params.get("artifact_id").and_then(Value::as_str))?;
                if params.get("view_id").is_some() { return Err(invalid("job-bound context.read reads the job artifact, not GUI views")); }
                if let Value::Object(m) = &mut params { m.insert("artifact_id".into(), Value::String(row.artifact_id.clone())); }
            }
            "events.poll" => {
                if ["view_id", "view_token"].iter().any(|k| params.get(*k).is_some_and(|v| !v.is_null())) {
                    return Err(invalid("a job-bound connection cannot receive GUI view commands"));
                }
            }
            "figure.render_link" => {
                let figure_id = params.get("figure_id").and_then(Value::as_str);
                let figure_revision = params.get("figure_revision").and_then(Value::as_u64);
                let approved = figure_id.zip(figure_revision).is_some_and(|(figure_id, revision)| {
                    row.request.figure_refs.iter().any(|reference| reference.figure_id == figure_id && reference.figure_revision == revision)
                });
                if !approved { return Err(invalid("figure revision is outside this job's declared figure_refs")); }
            }
            "proposal.get" | "proposal.preview" => {
                let pid = params.get("proposal_id").and_then(Value::as_str).unwrap_or_default().to_string();
                let (artifact, owner) = jobs::proposal_owner(&self.conn, &pid)?;
                scoped(Some(&artifact))?;
                // 83: another job's proposal and the user's stay with them.
                if owner.as_deref() != Some(job.job_id.as_str()) { return Err(invalid(format!("job {} may only read its own proposals", job.job_id)).ids([pid])); }
            }
            _ => scoped(params.get("artifact_id").and_then(Value::as_str))?,
        }
        Ok(params)
    }

    // ------------------------------------------------------------------ shared helpers
    pub(crate) fn engine_by_id(&self, engine_id: &str) -> DltResult<Arc<dyn EngineAdapter>> {
        self.by_id.get(engine_id).map(|&i| self.engines[i].clone()).ok_or_else(|| unsupported(format!("engine {engine_id:?} is not registered in this build")))
    }
    pub(crate) fn engine_by_kind(&self, kind: ArtifactKind) -> DltResult<Arc<dyn EngineAdapter>> {
        self.by_kind.get(&kind).map(|&i| self.engines[i].clone()).ok_or_else(|| unsupported(format!("no engine is registered for {kind:?}")))
    }
    pub(crate) fn staging(&self) -> StagingAssets { StagingAssets::new(self.base.clone()) }
    pub(crate) fn validate_read_source(&self, engine: &dyn EngineAdapter, artifact: &Artifact) -> DltResult<()> {
        let staging = self.staging();
        let renderer = self.renderer.clone();
        engine.validate_read_source(artifact, &EngineContext { assets: &staging, renderer: &*renderer })
    }
    pub(crate) fn load(&self, engine: &dyn EngineAdapter, hash: &str, revision: Revision) -> DltResult<Artifact> {
        let bytes = self.base.blobs.get(hash)?;
        snapshot_guard::check(&bytes)?;
        let mut artifact = engine.restore(&bytes).map_err(|e| integrity(format!("snapshot {hash} does not restore: {}", e.message)))?;
        artifact.revision = revision;
        Ok(artifact)
    }
    /// Artifact at `revision` (current or historical) plus its engine and the store row.
    pub(crate) fn load_revision(&self, artifact_id: &str, revision: Option<Revision>) -> DltResult<(store::ArtifactRow, Arc<dyn EngineAdapter>, Artifact, String)> {
        let row = store::artifact_row(&self.conn, artifact_id)?;
        let engine = self.engine_by_id(&row.engine_id)?;
        let revision = revision.unwrap_or(row.current_revision);
        if revision > row.current_revision {
            return Err(err(ErrorCode::RevisionConflict, format!("revision {revision} does not exist yet; current is {}", row.current_revision)).at(row.current_revision));
        }
        let hash = if revision == row.current_revision { row.current_snapshot.clone() } else { store::snapshot_at(&self.conn, artifact_id, revision)? };
        let artifact = self.load(&*engine, &hash, revision)?;
        Ok((row, engine, artifact, hash))
    }
    /// Normalized (revision 0) engine snapshot; restore(snapshot(x)) must reproduce the bytes.
    pub(crate) fn snapshot_of(engine: &dyn EngineAdapter, artifact: &Artifact) -> DltResult<(Vec<u8>, String)> {
        let mut normalized = artifact.clone();
        normalized.revision = 0;
        let bytes = engine.snapshot(&normalized)?;
        snapshot_guard::check(&bytes)?;
        let back = engine.restore(&bytes).map_err(|e| err(ErrorCode::Internal, format!("engine {} cannot restore its own snapshot: {}", engine.id(), e.message)))?;
        if engine.snapshot(&back)? != bytes {
            return Err(err(ErrorCode::Internal, format!("engine {} snapshot/restore is not idempotent; refusing to store a state that would not reload identically", engine.id())));
        }
        let hash = sha256(&bytes);
        Ok((bytes, hash))
    }

    /// Kind of an existing artifact, without opening its snapshot.
    pub fn artifact_kind(&self, artifact_id: &str) -> DltResult<ArtifactKind> {
        Ok(store::artifact_row(&self.conn, artifact_id)?.kind)
    }

    // ------------------------------------------------------------------ public job/event API
    pub fn job(&self, job_id: &str) -> DltResult<Job> { Ok(store::job_row(&self.conn, job_id)?.job()) }
    /// Stored start request (runtime, model, effort, prompt, sources) for launching the current
    /// attempt, including after `job.resume` created a new attempt.
    pub fn job_start_request(&self, job_id: &str) -> DltResult<JobStartRequest> { Ok(store::job_row(&self.conn, job_id)?.request) }
    /// Jobs in queued/running/validating (host.stop cancels these).
    pub fn active_jobs(&self) -> DltResult<Vec<Job>> {
        store::job_ids_with_status(&self.conn, &["queued", "running", "validating"])?.iter().map(|id| self.job(id)).collect()
    }
    /// Stored events with `event_seq > after_seq` (at most `limit`, 1..=1000).
    pub fn events_after(&self, after_seq: u64, limit: usize) -> DltResult<EventBatch> {
        let limit = limit.clamp(1, 1000);
        let last_seq = store::last_event_seq(&self.conn)?;
        let min_seq = store::min_event_seq(&self.conn)?;
        let gap = match min_seq { Some(min) => after_seq + 1 < min, None => after_seq < last_seq };
        if after_seq > last_seq || gap {
            let artifacts = store::artifact_rows(&self.conn)?.iter().map(store::ArtifactRow::summary).collect();
            return Ok(EventBatch { events: vec![], last_seq, resync_required: true, artifacts, ui_runs: vec![] });
        }
        Ok(EventBatch { events: store::events_after(&self.conn, after_seq, limit)?, last_seq, resync_required: false, artifacts: vec![], ui_runs: vec![] })
    }
}

/// Open-time recovery: in-flight native publications, blob temps, unreferenced blobs, missing
/// referenced blobs (refuse to open), and decodable current snapshots.
fn recover(conn: &mut Connection, blobs: &BlobStore, engines: &[Arc<dyn EngineAdapter>], by_id: &HashMap<String, usize>) -> DltResult<RecoveryReport> {
    let mut report = RecoveryReport::default();
    report.removed_pending_temps += files::recover_pending(conn, &mut report.recovered_saves)?;
    let scan = blobs.scan()?;
    for temp in &scan.temps { fs::remove_file(temp).map_err(io_store)?; report.removed_temp_blobs += 1; }
    let referenced = store::referenced_blobs(conn)?;
    let mut present = std::collections::HashSet::new();
    for hash in scan.blobs {
        if referenced.contains(&hash) { present.insert(hash); }
        else { fs::remove_file(blobs.path_of(&hash)).map_err(io_store)?; report.removed_unreferenced_blobs += 1; }
    }
    let mut missing: Vec<String> = referenced.difference(&present).cloned().collect();
    if !missing.is_empty() {
        missing.sort();
        return Err(integrity(format!("{} referenced blob(s) are missing from {}", missing.len(), blobs.dir().display())).ids(missing));
    }
    report.foreign_entries = scan.foreign;
    for row in store::artifact_rows(conn)? {
        let bytes = blobs.get(&row.current_snapshot).map_err(|e| integrity(format!("current snapshot of {} is unusable: {}", row.id, e.message)).ids([row.id.clone()]))?;
        if let Some(&i) = by_id.get(&row.engine_id) {
            snapshot_guard::check(&bytes).map_err(|e| e.ids([row.id.clone()]))?;
            engines[i].restore(&bytes).map_err(|e| integrity(format!("current snapshot of {} does not restore with {}: {}", row.id, row.engine_id, e.message)).ids([row.id.clone()]))?;
        }
        report.verified_current_snapshots += 1;
    }
    Ok(report)
}
