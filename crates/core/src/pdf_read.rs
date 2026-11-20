//! `source.extract_pdf` in three steps, so that a host reads a PDF without holding the workspace (task 34d).
//!
//! The registered PDF reader ([`Workspace::set_pdf_read_handler`]) can run for a long time: the product reader is a worker process
//! with a 90 s limit. [`Workspace::dispatch_start`] answers every other request as [`Workspace::dispatch_bound`] does. For a read
//! through that reader it checks the request and returns a [`PdfRead`]. The caller releases the workspace, runs [`PdfRead::run`],
//! and hands the outcome to [`Workspace::finish_pdf_read`], which checks the asset scope again and builds the response that
//! `dispatch_bound` builds for the same read.
//!
//! The read changes no workspace state. It reads one content-addressed asset through its own staging store, so an edit, another
//! read or any other request may run meanwhile. The response carries no revision: a caller that applies the evidence to a document
//! names its base revision in that change, as for any other change.

use crate::blobs::StagingAssets;
use crate::routes::PdfParams;
use crate::{object_params, parse, respond, to_value, PdfReadHandler, PdfReading, Workspace};
use designllm_contracts::{AssetRef, DltResult, JobRef, RpcRequest, RpcResponse, PROTOCOL_VERSION};
use serde_json::{json, Value};
use std::cell::RefCell;
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

const METHOD: &str = "source.extract_pdf";

/// What [`Workspace::dispatch_start`] did with one request.
pub enum Dispatch {
    /// The request was answered under the workspace, as `dispatch_bound` answers it.
    Answered(RpcResponse),
    /// A checked PDF read. Run it without the workspace, then call [`Workspace::finish_pdf_read`].
    ReadPdf(PdfRead),
}

/// One checked `source.extract_pdf` request. It holds the asset reference and its own staging store, not the workspace.
pub struct PdfRead {
    id: String,
    asset: AssetRef,
    pages: Vec<u32>,
    job: Option<JobRef>,
    reader: PdfReadHandler,
    staging: StagingAssets,
}

impl PdfRead {
    /// Runs the registered reader. It may take as long as the reader's own limit, unless the reader stops at `cancel`: while it
    /// runs, [`current_pdf_read_cancel`] on this thread returns `cancel`. A reader that ignores it runs to its end.
    pub fn run(&self, cancel: &PdfCancel) -> DltResult<PdfReading> {
        let _current = CurrentCancel::set(cancel);
        (self.reader)(&self.asset, &self.pages, &self.staging)
    }

    /// The job of a job-bound read.
    pub fn job_id(&self) -> Option<&str> {
        self.job.as_ref().map(|job| job.job_id.as_str())
    }
}

/// A request to stop a PDF read early: its connection closed, its job was canceled, or the host is stopping.
#[derive(Clone, Default)]
pub struct PdfCancel(Arc<AtomicBool>);

impl PdfCancel {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub fn is_canceled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

thread_local! {
    /// The cancel of the read that [`PdfRead::run`] runs on this thread. The reader is a plain function
    /// ([`PdfReadHandler`]), so the cancel reaches it here instead of through an argument.
    static CURRENT: RefCell<Option<PdfCancel>> = const { RefCell::new(None) };
}

/// The cancel of the PDF read running on this thread, for a reader that can stop early (the worker reader ends its process).
pub fn current_pdf_read_cancel() -> Option<PdfCancel> {
    CURRENT.with(|current| current.borrow().clone())
}

/// Sets [`CURRENT`] for one run, and restores what was there when the run ends, also by a panic.
struct CurrentCancel(Option<PdfCancel>);

impl CurrentCancel {
    fn set(cancel: &PdfCancel) -> Self {
        CurrentCancel(CURRENT.with(|current| current.replace(Some(cancel.clone()))))
    }
}

impl Drop for CurrentCancel {
    fn drop(&mut self) {
        let previous = self.0.take();
        CURRENT.with(|current| *current.borrow_mut() = previous);
    }
}

impl Workspace {
    /// Like [`Workspace::dispatch_bound`], except that a `source.extract_pdf` read through the registered reader is returned
    /// checked but not yet run.
    pub fn dispatch_start(&mut self, request: RpcRequest, bound_job: Option<JobRef>) -> Dispatch {
        let split = request.method == METHOD && request.protocol_version == PROTOCOL_VERSION;
        let Some(reader) = self.pdf_read.filter(|_| split) else {
            return Dispatch::Answered(self.dispatch_bound(request, bound_job));
        };
        let RpcRequest { id, params, .. } = request;
        match self.start_pdf_read(object_params(params), bound_job.as_ref()) {
            Ok((asset, pages)) => Dispatch::ReadPdf(PdfRead { id, asset, pages, job: bound_job, reader, staging: self.staging() }),
            Err(error) => Dispatch::Answered(RpcResponse::failure(id, error)),
        }
    }

    /// The checks of `dispatch_bound` and `source_extract_pdf` before the reader runs.
    fn start_pdf_read(&mut self, params: Value, job: Option<&JobRef>) -> DltResult<(AssetRef, Vec<u32>)> {
        let params = match job {
            Some(job) => self.bound_scope(METHOD, params, job)?,
            None => params,
        };
        let p: PdfParams = parse(params)?;
        Ok((self.source_asset(&p.sha256, job)?, p.pages))
    }

    /// The response to a read run by [`PdfRead::run`]. The asset must still be one the caller may read: a job's asset scope follows
    /// its artifact, which may have changed during the read.
    pub fn finish_pdf_read(&mut self, read: PdfRead, reading: DltResult<PdfReading>) -> RpcResponse {
        let PdfRead { id, asset, pages, job, .. } = read;
        let result = self.source_asset(&asset.sha256, job.as_ref()).and_then(|_| reading_value(&asset, &pages, reading));
        respond(id, METHOD, result)
    }
}

/// The `source.extract_pdf` result of a reader outcome, after [`PdfReading::validate`]. Shared by the route and the split read.
pub(crate) fn reading_value(asset: &AssetRef, pages: &[u32], reading: DltResult<PdfReading>) -> DltResult<Value> {
    let mut reading = reading?;
    // Validation also rewrites the page diagnostics into the form the caller gets (see `PdfReading::sanitize_page_diagnostics`).
    reading.validate(&asset.sha256, pages)?;
    // Pages that produced evidence. A page read without text appears only in `read_pages` and `page_reports`.
    let pages_with_evidence: BTreeSet<u32> = reading.evidence.iter().filter_map(|e| e.physical_page_index).collect();
    let page_reports = to_value(&reading.page_reports)?;
    // `document_complete` is true only when the reader verified the page count and read every page of the document.
    Ok(json!({ "asset": asset, "requested_pages": pages, "pages_with_evidence": pages_with_evidence, "evidence": reading.evidence, "read_pages": reading.read_pages, "page_reports": page_reports, "page_count": reading.page_count, "document_complete": reading.document_complete() }))
}
