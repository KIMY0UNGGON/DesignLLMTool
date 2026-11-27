//! The `source.extract_pdf` reader in a worker process (task 34c, `05-decisions/pdf.md` resource limits).
//!
//! The host does not parse a PDF itself. It starts a child process of the same executable (`DesignLLMTool --pdf-worker`)
//! suspended and resumes it inside a kill-on-close job object that caps the child's committed memory. The host waits up to
//! [`TIME_LIMIT`]. Once the worker is gone or the time is up, the host ends the whole job, so nothing the worker started can keep
//! the pipes open. A crash of the reader (stack overflow, abort, failed allocation) ends the child only, and the caller gets one
//! diagnostic.
//!
//! Protocol. Both directions use the bridge frame (`designllm_bridge::frame`: a 4-byte little-endian length, then JSON).
//! - Host to worker (stdin): one frame `{"protocol":1,"asset":AssetRef,"pages":[u32]}`, then the PDF bytes up to the end of the
//!   input. The worker opens no file and no workspace.
//! - Worker to host (stdout): one frame holding `Result<PdfReading, DltError>`, then exit code 0. Nothing else goes to stdout.
//!   Stderr is drained and discarded, since reader messages can name document text.
//!
//! The worker runs the same `read_pdf_in_process` as the in-process reader, so a read that completes gives the same value. The host
//! does not trust what the worker sends back: an error from the worker is withheld again (`withheld_worker_error`). [`verdict`] is
//! the whole outcome table.

use crate::{read_pdf_in_process, withheld_request_error, withheld_worker_error};
use designllm_bridge::frame::{read_frame, write_frame};
use designllm_contracts::{AssetRef, AssetStore, Diagnostic, DltError, DltResult, ErrorCode, MAX_FRAME_BYTES};
use designllm_core::{PdfCancel, PdfReading};
use designllm_runtimes::job as win;
use designllm_sources::MAX_PDF_BYTES;
use serde::Deserialize;
use std::io::{Cursor, Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

/// The hidden mode that makes the executable a PDF reader worker.
pub const WORKER_ARG: &str = "--pdf-worker";
/// Committed memory one worker may hold: 3 GiB, confirmed by the task 34 measurement window (2026-11-18).
pub const MEMORY_LIMIT_BYTES: usize = 3 * 1024 * 1024 * 1024;
/// Wall-clock limit of one read. The reader's own time check runs only between its steps, so the host ends the worker.
pub const TIME_LIMIT: Duration = Duration::from_secs(90);

/// Diagnostic of a worker that was ended at the memory cap (`UnsupportedOperation`). Added to docs/CONTRACTS.md.
pub const DIAGNOSTIC_MEMORY_LIMIT: &str = designllm_contracts::PDF_MEMORY_LIMIT;
/// Diagnostic of a worker that was ended at the time limit (`UnsupportedOperation`), as the in-process reader names it.
pub const DIAGNOSTIC_REQUEST_BUDGET: &str = designllm_contracts::PDF_REQUEST_BUDGET;
/// Diagnostic of a worker that ended at its stack limit (`UnsupportedOperation`), a resource limit like the memory cap (task 34d).
pub const DIAGNOSTIC_STACK_LIMIT: &str = designllm_contracts::PDF_STACK_LIMIT;
/// Diagnostic of a worker that crashed otherwise, or whose reply was missing or broken (`InvalidArgument`).
pub const DIAGNOSTIC_INTERNAL_PANIC: &str = designllm_contracts::PDF_INTERNAL_PANIC;
/// The Windows exit status of a process that overflowed a thread's stack (`STATUS_STACK_OVERFLOW`).
const STATUS_STACK_OVERFLOW: u32 = 0xC000_00FD;
/// Diagnostic of an asset whose declared size is over the PDF limit (`InvalidArgument`), as the in-process reader names it.
const DIAGNOSTIC_TOO_LARGE: &str = designllm_contracts::PDF_TOO_LARGE;

/// Protocol version in the request header.
const PROTOCOL: u32 = 1;
/// How often the host checks whether the worker has exited.
const POLL_INTERVAL: Duration = Duration::from_millis(5);

/// Limits of one worker run. The product uses [`PRODUCT_LIMITS`]; tests lower them through [`read_in_worker_for_tests`].
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub memory: usize,
    pub time: Duration,
}

#[doc(hidden)]
pub const PRODUCT_LIMITS: Limits = Limits { memory: MEMORY_LIMIT_BYTES, time: TIME_LIMIT };

/// The worker program of this host. Set once at host start (see `register_current_exe`).
static WORKER: OnceLock<PathBuf> = OnceLock::new();

/// Host start: the running executable is the worker. Only a host registers one, so a process that is not a host refuses PDF reads.
pub fn register_current_exe() {
    if let Ok(exe) = std::env::current_exe() {
        let _ = WORKER.set(exe);
    }
}

/// Test-only: names the worker program of this process. It must be an absolute path, because a relative name would be searched on
/// PATH. The first registration wins: the same path again is accepted, another path is refused.
#[doc(hidden)]
pub fn register_worker_for_tests(program: PathBuf) -> DltResult<()> {
    if !program.is_absolute() {
        return Err(DltError::invalid("the PDF reader worker must be named by an absolute path"));
    }
    match WORKER.set(program) {
        Ok(()) => Ok(()),
        Err(program) if WORKER.get() == Some(&program) => Ok(()),
        Err(_) => Err(DltError::invalid("another PDF reader worker is already registered in this process")),
    }
}

/// The product's `source.extract_pdf` read: through the registered worker. Without a registered worker the read is refused
/// (fail-closed), so the reader never runs in the host process.
pub(crate) fn read_with_registered_worker(asset: &AssetRef, pages: &[u32], assets: &dyn AssetStore) -> DltResult<PdfReading> {
    match WORKER.get() {
        Some(program) => read_in_worker(program, &[WORKER_ARG], asset, pages, assets, PRODUCT_LIMITS),
        None => Err(withheld_request_error(DltError::new(ErrorCode::Internal, "the PDF reader worker is not registered"))),
    }
}

/// One read through one worker run of `program args`. In the product `args` is `[WORKER_ARG]`. Every error is withheld.
fn read_in_worker(
    program: &Path,
    args: &[&str],
    asset: &AssetRef,
    pages: &[u32],
    assets: &dyn AssetStore,
    limits: Limits,
) -> DltResult<PdfReading> {
    // Refused before the bytes are read, so an oversized reference costs no read.
    if asset.bytes > MAX_PDF_BYTES {
        return Err(withheld_request_error(diagnosed(ErrorCode::InvalidArgument, DIAGNOSTIC_TOO_LARGE, "PDF exceeds the supported size")));
    }
    let body = assets.read(asset).map_err(withheld_request_error)?;
    let header = request_header(asset, pages).map_err(withheld_request_error)?;
    verdict(run(program, args, header, body, limits))
}

/// Test-only: [`read_in_worker`] with any program and any limits, so that a test reaches each outcome with a real process.
#[doc(hidden)]
pub fn read_in_worker_for_tests(
    program: &Path,
    args: &[&str],
    asset: &AssetRef,
    pages: &[u32],
    assets: &dyn AssetStore,
    limits: Limits,
) -> DltResult<PdfReading> {
    read_in_worker(program, args, asset, pages, assets, limits)
}

/// The request header frame: protocol version, the asset reference and the requested pages (empty means every page).
fn request_header(asset: &AssetRef, pages: &[u32]) -> DltResult<Vec<u8>> {
    serde_json::to_vec(&serde_json::json!({ "protocol": PROTOCOL, "asset": asset, "pages": pages }))
        .map_err(|error| DltError::new(ErrorCode::Internal, error.to_string()))
}

/// What the worker's stdout held.
enum Reply {
    /// One whole frame: the payload of the reply.
    Frame(Vec<u8>),
    /// A frame header above the bridge's frame limit. The payload is not read.
    Oversized,
    /// No whole frame: the stream ended first, or the frame was broken.
    Missing,
}

/// What one worker run came to. [`verdict`] turns it into the result.
enum Finish {
    /// The program could not be started, or its state could not be read: an environment failure.
    Unavailable,
    /// The time limit passed, and the job was ended.
    TimedOut,
    /// The host gave the read up (`PdfCancel`), and the job was ended.
    Canceled,
    /// The worker exited. `memory_limit` is the job's report that the worker exceeded its committed memory cap. `stack_overflow` is
    /// an exit status of [`STATUS_STACK_OVERFLOW`].
    Exited { success: bool, memory_limit: bool, stack_overflow: bool, reply: Reply },
}

/// How the wait for the worker ended.
enum Watch {
    Exited(ExitStatus),
    TimedOut,
    Canceled,
    Unwatchable(std::io::Error),
}

fn run(program: &Path, args: &[&str], header: Vec<u8>, body: Vec<u8>, limits: Limits) -> Finish {
    let started = Instant::now();
    let args: Vec<String> = args.iter().map(|arg| arg.to_string()).collect();
    let cwd = match program.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir,
        _ => Path::new("."),
    };
    let Ok((mut child, job)) = win::spawn_suspended_in_limited_job(program, &args, cwd, Some(limits.memory)) else {
        return Finish::Unavailable;
    };
    if win::resume_child(&child).is_err() {
        let _ = job.terminate(1);
        let _ = child.kill();
        let _ = child.wait();
        return Finish::Unavailable;
    }
    let (stdin, stdout, stderr) = (child.stdin.take(), child.stdout.take(), child.stderr.take());
    // Each pipe has its own thread, so the wait below never blocks on one of them.
    let writer = thread::spawn(move || {
        if let Some(mut stdin) = stdin {
            // A failed write means the worker is gone; the exit status decides the outcome.
            if write_frame(&mut stdin, &header).is_ok() {
                let _ = stdin.write_all(&body);
            }
        }
    });
    let reader = thread::spawn(move || -> Reply {
        let Some(mut stdout) = stdout else {
            return Reply::Missing;
        };
        let mut length = [0u8; 4];
        if stdout.read_exact(&mut length).is_err() {
            return Reply::Missing;
        }
        if u32::from_le_bytes(length) as usize > MAX_FRAME_BYTES {
            return Reply::Oversized;
        }
        // read_frame reads the length again, checks it, and reads the payload.
        match read_frame(&mut Cursor::new(length).chain(stdout)) {
            Ok(Some(payload)) => Reply::Frame(payload),
            _ => Reply::Missing,
        }
    });
    let drain = thread::spawn(move || {
        if let Some(mut stderr) = stderr {
            let _ = std::io::copy(&mut stderr, &mut std::io::sink());
        }
    });
    let deadline = started + limits.time;
    // Set when the host gives up the read (its connection closed, its job was canceled, the host stops): the job ends now.
    let cancel = designllm_core::current_pdf_read_cancel();
    let watch = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Watch::Exited(status),
            Ok(None) => {}
            Err(error) => break Watch::Unwatchable(error),
        }
        if Instant::now() >= deadline {
            break Watch::TimedOut;
        }
        if cancel.as_ref().is_some_and(PdfCancel::is_canceled) {
            break Watch::Canceled;
        }
        thread::sleep(POLL_INTERVAL);
    };
    // Whatever the wait found, the job is ended before the pipes are read to their end. A grandchild that inherited a pipe would
    // otherwise keep the reader and the writer blocked for as long as it lives. The worker itself is reaped here as well.
    let _ = job.terminate(1);
    let _ = child.wait();
    let reply = reader.join().unwrap_or(Reply::Missing);
    let _ = writer.join();
    let _ = drain.join();
    match watch {
        Watch::TimedOut => Finish::TimedOut,
        Watch::Canceled => Finish::Canceled,
        Watch::Unwatchable(error) => {
            eprintln!("PDF reader worker could not be watched: {error}");
            Finish::Unavailable
        }
        Watch::Exited(status) => {
            let success = status.success();
            let memory_limit = job.memory_limit_reported();
            let peak = job.peak_process_memory().unwrap_or(0);
            if !success {
                // Host log only. The worker's own stderr is never copied: it can name document text.
                eprintln!("PDF reader worker ended abnormally: exit {status:?}, peak committed {peak} bytes, memory limit reported {memory_limit}");
            }
            // The status is a 32-bit NTSTATUS that `code` reports as i32.
            let stack_overflow = status.code().map(|code| code as u32) == Some(STATUS_STACK_OVERFLOW);
            Finish::Exited { success, memory_limit, stack_overflow, reply }
        }
    }
}

/// The outcome table of task 34c. A reader error from the worker is withheld again, so the host shows the same fixed text that the
/// in-process reader shows.
fn verdict(finish: Finish) -> DltResult<PdfReading> {
    match finish {
        Finish::Unavailable => Err(withheld_request_error(DltError::new(ErrorCode::Internal, "the PDF reader could not be run"))),
        // Nothing about the document: the caller gave the read up.
        Finish::Canceled => Err(DltError::new(ErrorCode::Canceled, "the PDF read was canceled")),
        Finish::TimedOut => Err(withheld_request_error(diagnosed(
            ErrorCode::UnsupportedOperation,
            DIAGNOSTIC_REQUEST_BUDGET,
            "the request runs longer than the supported time",
        ))),
        // Decided before the exit status and the reply. A worker can catch a failed allocation and still exit with success and a
        // reply; that reply would be a reading with less evidence than the document has.
        Finish::Exited { memory_limit: true, .. } => Err(withheld_request_error(diagnosed(
            ErrorCode::UnsupportedOperation,
            DIAGNOSTIC_MEMORY_LIMIT,
            "the PDF reader needs more memory than its limit allows",
        ))),
        // A stack overflow ends only the worker. It is a limit of the reader (deep recursion on the input), not a broken reply.
        Finish::Exited { stack_overflow: true, .. } => Err(withheld_request_error(diagnosed(
            ErrorCode::UnsupportedOperation,
            DIAGNOSTIC_STACK_LIMIT,
            "the PDF reader needs more stack than its limit allows",
        ))),
        // A frame above the bridge's limit is a limit of the host, not a fact about the document: a fixed error, no diagnostic.
        Finish::Exited { reply: Reply::Oversized, .. } => {
            Err(withheld_request_error(DltError::new(ErrorCode::Internal, "the PDF reader result is larger than one frame")))
        }
        Finish::Exited { success: true, reply: Reply::Frame(payload), .. } => {
            match serde_json::from_slice::<DltResult<PdfReading>>(&payload) {
                Ok(Ok(reading)) => Ok(reading),
                Ok(Err(error)) => Err(withheld_worker_error(error)),
                Err(_) => Err(withheld_request_error(internal_panic())),
            }
        }
        Finish::Exited { .. } => Err(withheld_request_error(internal_panic())),
    }
}

fn internal_panic() -> DltError {
    diagnosed(ErrorCode::InvalidArgument, DIAGNOSTIC_INTERNAL_PANIC, "the PDF reader stopped unexpectedly")
}

/// An error with one blocking diagnostic, as the in-process reader builds its own.
fn diagnosed(code: ErrorCode, diagnostic: &str, message: &str) -> DltError {
    let mut error = DltError::new(code, message);
    error.diagnostics.push(Diagnostic {
        code: diagnostic.into(),
        message: message.into(),
        node_id: None,
        blocking: true,
        params: Default::default(),
    });
    error
}

/// The worker mode (`--pdf-worker`): one request on stdin, one reply frame on stdout.
pub fn run_worker() -> DltResult<()> {
    let reply = answer(&mut std::io::stdin().lock());
    let mut payload = serde_json::to_vec(&reply).map_err(|error| DltError::new(ErrorCode::Internal, error.to_string()))?;
    if payload.len() > MAX_FRAME_BYTES {
        // The host reads one frame of at most MAX_FRAME_BYTES, so a larger result becomes a fixed error.
        let too_big: DltResult<PdfReading> = Err(DltError::new(ErrorCode::Internal, "the PDF reader result is larger than one frame"));
        payload = serde_json::to_vec(&too_big).map_err(|error| DltError::new(ErrorCode::Internal, error.to_string()))?;
    }
    write_frame(&mut std::io::stdout().lock(), &payload)
}

#[derive(Deserialize)]
struct Request {
    protocol: u32,
    asset: AssetRef,
    pages: Vec<u32>,
}

/// Reads one request and the PDF it carries. Generic over the input, so the protocol is testable without a process.
fn answer<R: Read>(input: &mut R) -> DltResult<PdfReading> {
    let header = read_frame(input)?.ok_or_else(|| DltError::new(ErrorCode::Internal, "the PDF reader received no request"))?;
    let request: Request =
        serde_json::from_slice(&header).map_err(|_| DltError::new(ErrorCode::Internal, "the PDF reader request is malformed"))?;
    if request.protocol != PROTOCOL {
        return Err(DltError::new(ErrorCode::Internal, "the PDF reader protocol does not match"));
    }
    let mut bytes = Vec::new();
    // One byte past the limit is read, so an oversized body reaches the reader and is refused there as PDF_TOO_LARGE.
    input.take(MAX_PDF_BYTES + 1).read_to_end(&mut bytes)?;
    let Request { asset, pages, .. } = request;
    let store = WorkerStore { asset, bytes: Mutex::new(Some(bytes)) };
    read_pdf_in_process(&store.asset, &pages, &store)
}

/// The one asset of a worker run. Its bytes are handed out once, so the reader does not hold a second copy.
struct WorkerStore {
    asset: AssetRef,
    bytes: Mutex<Option<Vec<u8>>>,
}

impl AssetStore for WorkerStore {
    fn put(&self, _bytes: &[u8], _mime: &str) -> DltResult<AssetRef> {
        Err(DltError::new(ErrorCode::Internal, "the PDF reader worker stores no assets"))
    }

    fn read(&self, asset: &AssetRef) -> DltResult<Vec<u8>> {
        if asset != &self.asset {
            return Err(DltError::new(ErrorCode::NotFound, "asset not found"));
        }
        self.bytes
            .lock()
            .ok()
            .and_then(|mut bytes| bytes.take())
            .ok_or_else(|| DltError::new(ErrorCode::Internal, "the PDF reader read its asset twice"))
    }

    fn lookup(&self, _hash: &str) -> DltResult<AssetRef> {
        Err(DltError::new(ErrorCode::NotFound, "the PDF reader worker looks up no assets"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::withheld_read_failure;
    use designllm_core::PdfPageReport;

    const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../fixtures/pdf/evidence-p12.pdf");
    const LIMITS: Limits = PRODUCT_LIMITS;
    const FIXED_PAGE_TEXT: &str = "the PDF could not be read; the reader's detail is withheld";

    fn reading() -> PdfReading {
        PdfReading {
            evidence: vec![],
            read_pages: vec![0],
            page_reports: vec![PdfPageReport { physical_page_index: 0, read: true, evidence_count: 0, diagnostics: vec![] }],
            page_count: Some(1),
        }
    }

    fn exited(success: bool, memory_limit: bool, reply: Reply) -> Finish {
        Finish::Exited { success, memory_limit, stack_overflow: false, reply }
    }

    fn reply_of(result: &DltResult<PdfReading>) -> Reply {
        Reply::Frame(serde_json::to_vec(result).unwrap())
    }

    fn code_of(result: DltResult<PdfReading>) -> (ErrorCode, Vec<String>, String) {
        let error = result.expect_err("an error");
        let codes = error.diagnostics.iter().map(|diagnostic| diagnostic.code.clone()).collect();
        (error.code, codes, error.message)
    }

    #[test]
    fn a_reply_from_a_worker_that_exited_with_success_is_the_result() {
        let out = verdict(exited(true, false, reply_of(&Ok(reading())))).expect("the reading");
        assert_eq!(out.read_pages, vec![0]);
        assert_eq!(out.page_count, Some(1));
    }

    #[test]
    fn a_reader_error_in_the_reply_is_withheld_to_fixed_text_and_keeps_its_code() {
        let mut error = DltError::new(ErrorCode::InvalidArgument, "C:\\secret\\evidence.pdf: unexpected token");
        error.diagnostics.push(Diagnostic {
            code: "PDF_PARSE_ERROR".into(),
            message: "The system cannot find the path specified. (os error 3)".into(),
            node_id: None,
            blocking: true,
            params: Default::default(),
        });
        let (code, codes, message) = code_of(verdict(exited(true, false, reply_of(&Err(error)))));
        assert_eq!(code, ErrorCode::InvalidArgument);
        assert_eq!(codes, vec!["PDF_PARSE_ERROR".to_string()]);
        assert_eq!(message, FIXED_PAGE_TEXT);
    }

    #[test]
    fn a_page_message_in_a_reply_survives_only_when_it_is_the_fixed_page_text() {
        let page = |message: &str| {
            let mut error = DltError::new(ErrorCode::UnsupportedOperation, message);
            error.affected_ids = vec!["3".into()];
            error
        };
        let (_, _, fixed) = code_of(verdict(exited(true, false, reply_of(&Err(page("physical page 3 could not be read; the reader's detail is withheld"))))));
        assert_eq!(fixed, "physical page 3 could not be read; the reader's detail is withheld");
        let (_, _, altered) = code_of(verdict(exited(true, false, reply_of(&Err(page("physical page 3 could not be read: C:\\x"))))));
        assert_eq!(altered, FIXED_PAGE_TEXT);
    }

    #[test]
    fn a_refusal_the_worker_withheld_comes_back_with_the_same_bytes_from_the_host_step() {
        // The in-process refusals of a request that names a page out of range, and of a page that failed to read.
        let mut out_of_range = DltError::new(ErrorCode::InvalidArgument, "physical page index 999 is out of range; the document has 1 pages");
        out_of_range.affected_ids = vec!["999".into()];
        out_of_range.diagnostics.push(Diagnostic {
            code: "PDF_PAGE_OUT_OF_RANGE".into(),
            message: "physical page index 999 is out of range".into(),
            node_id: None,
            blocking: true,
            params: Default::default(),
        });
        let once = withheld_request_error(out_of_range);
        let twice = withheld_worker_error(once.clone());
        assert_eq!(serde_json::to_vec(&once).unwrap(), serde_json::to_vec(&twice).unwrap());

        let mut failure = DltError::new(ErrorCode::UnsupportedOperation, "font F9 is missing");
        failure.affected_ids = vec!["3".into()];
        failure.diagnostics.push(Diagnostic {
            code: "PDF_FONT_MISSING".into(),
            message: "font F9 is missing".into(),
            node_id: None,
            blocking: true,
            params: Default::default(),
        });
        let once = withheld_read_failure(failure);
        let twice = withheld_worker_error(once.clone());
        assert_eq!(serde_json::to_vec(&once).unwrap(), serde_json::to_vec(&twice).unwrap());
    }

    #[test]
    fn a_success_without_a_frame_is_an_internal_panic() {
        let (code, codes, message) = code_of(verdict(exited(true, false, Reply::Missing)));
        assert_eq!(code, ErrorCode::InvalidArgument);
        assert_eq!(codes, vec![DIAGNOSTIC_INTERNAL_PANIC.to_string()]);
        assert_eq!(message, FIXED_PAGE_TEXT);
    }

    #[test]
    fn a_frame_that_is_not_a_result_is_an_internal_panic() {
        let (code, codes, _) = code_of(verdict(exited(true, false, Reply::Frame(b"{}".to_vec()))));
        assert_eq!(code, ErrorCode::InvalidArgument);
        assert_eq!(codes, vec![DIAGNOSTIC_INTERNAL_PANIC.to_string()]);
    }

    #[test]
    fn an_oversized_reply_is_an_internal_error_without_a_diagnostic() {
        let (code, codes, _) = code_of(verdict(exited(true, false, Reply::Oversized)));
        assert_eq!(code, ErrorCode::Internal);
        assert!(codes.is_empty());
    }

    #[test]
    fn a_failed_exit_with_the_job_memory_report_is_the_memory_limit() {
        let (code, codes, _) = code_of(verdict(exited(false, true, Reply::Missing)));
        assert_eq!(code, ErrorCode::UnsupportedOperation);
        assert_eq!(codes, vec![DIAGNOSTIC_MEMORY_LIMIT.to_string()]);
    }

    #[test]
    fn a_memory_report_wins_over_a_successful_exit_and_its_reply() {
        let (code, codes, _) = code_of(verdict(exited(true, true, reply_of(&Ok(reading())))));
        assert_eq!(code, ErrorCode::UnsupportedOperation);
        assert_eq!(codes, vec![DIAGNOSTIC_MEMORY_LIMIT.to_string()]);
    }

    #[test]
    fn a_failed_exit_without_the_memory_report_is_an_internal_panic() {
        let (code, codes, _) = code_of(verdict(exited(false, false, Reply::Missing)));
        assert_eq!(code, ErrorCode::InvalidArgument);
        assert_eq!(codes, vec![DIAGNOSTIC_INTERNAL_PANIC.to_string()]);
    }

    #[test]
    fn a_failed_exit_with_a_reply_and_no_memory_report_is_an_internal_panic() {
        let (_, codes, _) = code_of(verdict(exited(false, false, Reply::Frame(vec![1, 2]))));
        assert_eq!(codes, vec![DIAGNOSTIC_INTERNAL_PANIC.to_string()]);
    }

    #[test]
    fn a_stack_overflow_exit_is_the_stack_limit_and_not_an_internal_panic() {
        let finish = Finish::Exited { success: false, memory_limit: false, stack_overflow: true, reply: Reply::Missing };
        let (code, codes, message) = code_of(verdict(finish));
        assert_eq!(code, ErrorCode::UnsupportedOperation);
        assert_eq!(codes, vec![DIAGNOSTIC_STACK_LIMIT.to_string()]);
        assert_eq!(message, FIXED_PAGE_TEXT);
    }

    #[test]
    fn a_memory_report_wins_over_a_stack_overflow_exit() {
        let finish = Finish::Exited { success: false, memory_limit: true, stack_overflow: true, reply: Reply::Missing };
        let (_, codes, _) = code_of(verdict(finish));
        assert_eq!(codes, vec![DIAGNOSTIC_MEMORY_LIMIT.to_string()]);
    }

    #[test]
    fn a_canceled_run_is_canceled_without_a_diagnostic() {
        let (code, codes, _) = code_of(verdict(Finish::Canceled));
        assert_eq!(code, ErrorCode::Canceled);
        assert!(codes.is_empty());
    }

    #[test]
    fn a_run_past_the_time_limit_is_the_request_budget() {
        let (code, codes, _) = code_of(verdict(Finish::TimedOut));
        assert_eq!(code, ErrorCode::UnsupportedOperation);
        assert_eq!(codes, vec![DIAGNOSTIC_REQUEST_BUDGET.to_string()]);
    }

    #[test]
    fn a_worker_that_could_not_be_run_is_internal_without_a_diagnostic() {
        let (code, codes, _) = code_of(verdict(Finish::Unavailable));
        assert_eq!(code, ErrorCode::Internal);
        assert!(codes.is_empty());
    }

    #[test]
    fn an_asset_over_the_pdf_limit_is_refused_before_it_is_read() {
        struct NeverRead;
        impl AssetStore for NeverRead {
            fn put(&self, _bytes: &[u8], _mime: &str) -> DltResult<AssetRef> {
                unreachable!("no store call for an oversized reference")
            }
            fn read(&self, _asset: &AssetRef) -> DltResult<Vec<u8>> {
                unreachable!("the bytes of an oversized reference are not read")
            }
            fn lookup(&self, _hash: &str) -> DltResult<AssetRef> {
                unreachable!("no lookup")
            }
        }
        let asset = AssetRef { sha256: "0".repeat(64), mime: "application/pdf".into(), bytes: MAX_PDF_BYTES + 1 };
        let (code, codes, _) = code_of(read_in_worker(Path::new("unused.exe"), &[], &asset, &[], &NeverRead, LIMITS));
        assert_eq!(code, ErrorCode::InvalidArgument);
        assert_eq!(codes, vec![DIAGNOSTIC_TOO_LARGE.to_string()]);
    }

    #[test]
    fn a_host_that_registered_no_worker_refuses_the_read() {
        struct OneAsset;
        impl AssetStore for OneAsset {
            fn put(&self, _bytes: &[u8], _mime: &str) -> DltResult<AssetRef> {
                Err(DltError::new(ErrorCode::Io, "read-only test store"))
            }
            fn read(&self, _asset: &AssetRef) -> DltResult<Vec<u8>> {
                Ok(std::fs::read(FIXTURE).unwrap())
            }
            fn lookup(&self, _hash: &str) -> DltResult<AssetRef> {
                Err(DltError::new(ErrorCode::NotFound, "read-only test store"))
            }
        }
        let bytes = std::fs::read(FIXTURE).unwrap();
        let asset = AssetRef { sha256: designllm_contracts::sha256(&bytes), mime: "application/pdf".into(), bytes: bytes.len() as u64 };
        let (code, codes, _) = code_of(read_with_registered_worker(&asset, &[], &OneAsset));
        assert_eq!(code, ErrorCode::Internal);
        assert!(codes.is_empty());
    }

    #[test]
    fn a_registration_must_name_an_absolute_path() {
        assert!(register_worker_for_tests(PathBuf::from("DesignLLMTool.exe")).is_err());
        assert!(WORKER.get().is_none(), "a refused registration registers nothing");
    }

    #[test]
    fn the_worker_side_answers_a_request_with_the_in_process_reading() {
        let bytes = std::fs::read(FIXTURE).unwrap();
        struct Bytes(Vec<u8>);
        impl AssetStore for Bytes {
            fn put(&self, _bytes: &[u8], _mime: &str) -> DltResult<AssetRef> {
                Err(DltError::new(ErrorCode::Io, "read-only test store"))
            }
            fn read(&self, _asset: &AssetRef) -> DltResult<Vec<u8>> {
                Ok(self.0.clone())
            }
            fn lookup(&self, _hash: &str) -> DltResult<AssetRef> {
                Err(DltError::new(ErrorCode::NotFound, "read-only test store"))
            }
        }
        let asset = AssetRef { sha256: designllm_contracts::sha256(&bytes), mime: "application/pdf".into(), bytes: bytes.len() as u64 };
        let pages = [0u32];
        let in_process = read_pdf_in_process(&asset, &pages, &Bytes(bytes.clone())).expect("in-process read");

        let mut wire = Vec::new();
        write_frame(&mut wire, &request_header(&asset, &pages).unwrap()).unwrap();
        wire.extend_from_slice(&bytes);
        let via_worker = answer(&mut Cursor::new(wire)).expect("worker-side read");

        assert_eq!(serde_json::to_vec(&in_process).unwrap(), serde_json::to_vec(&via_worker).unwrap());
    }

    #[test]
    fn a_request_of_another_protocol_is_refused_by_the_worker() {
        let asset = AssetRef { sha256: "0".repeat(64), mime: "application/pdf".into(), bytes: 0 };
        let header = serde_json::to_vec(&serde_json::json!({ "protocol": 2, "asset": asset, "pages": [] })).unwrap();
        let mut wire = Vec::new();
        write_frame(&mut wire, &header).unwrap();
        let error = answer(&mut Cursor::new(wire)).expect_err("refused");
        assert_eq!(error.code, ErrorCode::Internal);
    }
}
