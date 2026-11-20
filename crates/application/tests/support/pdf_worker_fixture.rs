//! Test-only stand-in for the PDF reader worker (task 34c). It is not a product binary: the host never starts it. A test starts it by
//! name (`CARGO_BIN_EXE_designllm-pdf-worker-fixture`) with one mode argument. Each mode is one way a worker can misbehave, so each
//! branch of the host's outcome table runs against a real process:
//! - `reply-error`: a whole reply frame whose reader error carries operating-system text and a document path.
//! - `reply-truncated`: a frame whose payload is cut short, then exit 0.
//! - `reply-oversized`: a frame header above the bridge's frame limit, then exit 0.
//! - `grandchild`: exit 0 with no reply, while a grandchild that inherited the pipes is still running.
//! - `oom-then-reply`: exhausts the committed-memory cap, catches the failed allocations, and then replies with a result.

use designllm_bridge::frame::write_frame;
use designllm_contracts::{Diagnostic, DltError, DltResult, ErrorCode, MAX_FRAME_BYTES};
use designllm_core::{PdfPageReport, PdfReading};
use std::io::{Read, Write};
use std::process::Command;

/// The unit of `oom-then-reply`. Each chunk is committed in full, so the cap is reached after a few of them.
const CHUNK_BYTES: usize = 16 * 1024 * 1024;

fn main() {
    let mode = std::env::args().nth(1).unwrap_or_default();
    // Like the product worker, read the whole request first: the host writes it and then closes stdin.
    let mut request = Vec::new();
    let _ = std::io::stdin().lock().read_to_end(&mut request);
    match mode.as_str() {
        "reply-error" => reply(&Err(reader_error())),
        "reply-truncated" => truncated(),
        "reply-oversized" => oversized(),
        "grandchild" => grandchild(),
        "oom-then-reply" => {
            let held = exhaust_memory();
            reply(&Ok(reading()));
            std::hint::black_box(held.len());
        }
        _ => std::process::exit(2),
    }
}

fn reply(result: &DltResult<PdfReading>) {
    let payload = serde_json::to_vec(result).expect("a reply serializes");
    let mut stdout = std::io::stdout().lock();
    write_frame(&mut stdout, &payload).expect("the frame is written");
}

fn truncated() {
    let mut stdout = std::io::stdout().lock();
    stdout.write_all(&100u32.to_le_bytes()).expect("the header is written");
    stdout.write_all(b"0123456789").expect("the partial payload is written");
    stdout.flush().expect("the output is flushed");
}

fn oversized() {
    let mut stdout = std::io::stdout().lock();
    stdout.write_all(&((MAX_FRAME_BYTES as u32) + 1).to_le_bytes()).expect("the header is written");
    stdout.flush().expect("the output is flushed");
}

/// `cmd` keeps the inherited pipes open while `ping` runs for about two minutes. `ping` writes to nul, so nothing else is written.
fn grandchild() {
    Command::new(r"C:\Windows\System32\cmd.exe").args(["/c", "ping -n 120 127.0.0.1 >nul"]).spawn().expect("the grandchild starts");
}

/// Commits chunks until one of them cannot be committed. The failure is caught here, so the process does not abort: the only trace
/// is the job's memory report.
fn exhaust_memory() -> Vec<Vec<u8>> {
    let mut held = Vec::new();
    while held.len() < 4096 {
        let mut chunk: Vec<u8> = Vec::new();
        if chunk.try_reserve_exact(CHUNK_BYTES).is_err() {
            break;
        }
        // Writing every byte commits the pages here, inside the reserved chunk.
        chunk.resize(CHUNK_BYTES, 1);
        held.push(chunk);
    }
    held
}

/// A reader error that carries what must not reach the user: an operating-system message and a document path.
fn reader_error() -> DltError {
    let mut error = DltError::new(ErrorCode::Io, "The system cannot find the path specified. (os error 3)");
    error.diagnostics.push(Diagnostic {
        code: "PDF_PARSE_ERROR".into(),
        message: r"C:\secret\evidence.pdf: unexpected token".into(),
        node_id: None,
        blocking: true,
        params: Default::default(),
    });
    error
}

fn reading() -> PdfReading {
    PdfReading {
        evidence: Vec::new(),
        read_pages: vec![0],
        page_reports: vec![PdfPageReport { physical_page_index: 0, read: true, evidence_count: 0, diagnostics: Vec::new() }],
        page_count: Some(1),
    }
}
