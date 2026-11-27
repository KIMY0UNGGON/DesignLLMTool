//! One running attempt: the runtime process tree, its job object, its bounded progress text, and its audit (the model
//! check and the transcript). A failed check ends the process tree at once.

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use designllm_contracts::{DltError, DltResult, ErrorCode};

use crate::agy::AgyProfile;
use crate::audit::{ModelCheck, RunAudit};
use crate::detect::{RuntimeInfo, RuntimeKind};
use crate::invocation::{build, LaunchSpec};
use crate::progress::ProgressBuffer;
use crate::job::{resume_child, spawn_suspended_in_job_with_env, JobObject};

/// Poll interval for the exit check.
const EXIT_POLL: Duration = Duration::from_millis(25);
/// Longest wait for the output readers after the runtime exits. They end when their pipes close.
const READER_JOIN: Duration = Duration::from_secs(10);
/// Longest stdout or stderr line read whole. A longer line keeps its first bytes and a marker.
pub const MAX_LINE_BYTES: usize = 1024 * 1024;

/// Result of waiting on an attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitOutcome {
    /// The runtime process exited with this code.
    Exited(i32),
    /// The deadline passed while the process was still running. Nothing was killed; call `kill_tree`.
    TimedOut,
}

/// Handle to one attempt. Clones share the same process tree. Dropping the last handle closes the job
/// object, which also ends the tree (kill-on-close).
#[derive(Clone)]
pub struct Attempt {
    inner: Arc<AttemptInner>,
}

struct AttemptInner {
    pid: u32,
    child: Mutex<Child>,
    job: Arc<JobObject>,
    progress: Arc<ProgressBuffer>,
    stderr: Arc<ProgressBuffer>,
    killed: AtomicBool,
    audit: Mutex<RunAudit>,
    readers: Mutex<Vec<JoinHandle<()>>>,
    exit_code: Mutex<Option<i32>>,
    _agy_profile: Option<Arc<AgyProfile>>,
}

/// The job object of one attempt, handed to the caller before the runtime runs. Holding it keeps the job open.
#[derive(Clone)]
pub struct JobWatch {
    job: Arc<JobObject>,
}

impl JobWatch {
    /// Process ids in the attempt's job: the runtime and every process it started.
    pub fn process_ids(&self) -> DltResult<Vec<u32>> {
        self.job.process_ids().map_err(|e| DltError::new(ErrorCode::Io, format!("cannot query the job: {e}")))
    }
}

/// Validates the spec, builds the invocation, and starts the runtime inside a new job object.
/// The process is resumed only after it is assigned to the job. No transcript is kept; the model check still runs.
pub fn start(info: &RuntimeInfo, spec: &LaunchSpec) -> DltResult<Attempt> {
    start_with_transcript(info, spec, None)
}

/// Like [`start`], and keeps the transcript at `transcript`. The file is created before the process starts, so an
/// attempt that cannot keep its transcript does not run.
pub fn start_with_transcript(info: &RuntimeInfo, spec: &LaunchSpec, transcript: Option<&Path>) -> DltResult<Attempt> {
    start_with_transcript_watched(info, spec, transcript, |_| {})
}

/// Like [`start_with_transcript`]. `on_assigned` receives the attempt's job before the runtime runs. The process is
/// suspended, assigned to the job, handed to the callback, and resumed only after the callback returns. A caller that
/// records the job in the callback knows every process of the attempt before the first one runs.
pub fn start_with_transcript_watched(
    info: &RuntimeInfo,
    spec: &LaunchSpec,
    transcript: Option<&Path>,
    on_assigned: impl FnOnce(JobWatch),
) -> DltResult<Attempt> {
    let mut invocation = build(info, spec)?;
    let agy_profile = if info.kind == RuntimeKind::Agy {
        let profile = AgyProfile::create()?;
        invocation.args.extend(profile.args());
        Some(Arc::new(profile))
    } else { None };
    let mut audit = RunAudit::open(info.kind, &spec.request.model, &spec.request.effort, &invocation.program, transcript)?;
    let cwd = agy_profile.as_ref().map_or(invocation.cwd.as_path(), |profile| profile.directory.as_path());
    let started = spawn_suspended_in_job_with_env(&invocation.program, &invocation.args, cwd, &invocation.env);
    let (mut child, job) = match started {
        Ok(started) => started,
        Err(e) => {
            audit.finish(None, false, &[]);
            return Err(DltError::new(
                ErrorCode::UnsupportedOperation,
                format!("cannot start {}: {e}", invocation.program.display()),
            ));
        }
    };
    let job = Arc::new(job);
    on_assigned(JobWatch { job: Arc::clone(&job) });
    if let Err(e) = resume_child(&child) {
        let _ = job.terminate(1);
        let _ = child.kill();
        let _ = child.wait();
        audit.finish(None, false, &[]);
        return Err(DltError::new(
            ErrorCode::UnsupportedOperation,
            format!("cannot start {}: {e}", invocation.program.display()),
        ));
    }
    let pid = child.id();
    let progress = Arc::new(ProgressBuffer::new());
    let stderr = Arc::new(ProgressBuffer::new());
    let stdin = child.stdin.take();
    let out = child.stdout.take();
    let err = child.stderr.take();
    let inner = Arc::new(AttemptInner {
        pid,
        child: Mutex::new(child),
        job,
        progress,
        stderr,
        killed: AtomicBool::new(false),
        audit: Mutex::new(audit),
        readers: Mutex::new(Vec::new()),
        exit_code: Mutex::new(None),
        _agy_profile: agy_profile.clone(),
    });
    let mut readers = Vec::new();
    if let Some(profile) = agy_profile {
        let (Some(stdin), Some(out)) = (stdin, out) else {
            terminate(&inner);
            return Err(DltError::new(ErrorCode::Io, "AGY stdio is unavailable"));
        };
        readers.push(spawn_agy_turn(Arc::clone(&inner), profile, out, stdin, spec.clone(), invocation.stdin));
    } else {
        if let Some(mut stdin) = stdin {
            thread::spawn(move || { let _ = stdin.write_all(invocation.stdin.as_bytes()); });
        }
        if let Some(out) = out {
            readers.push(spawn_stdout_reader(Arc::clone(&inner), out));
        }
    }
    if let Some(err) = err {
        readers.push(spawn_stderr_reader(Arc::clone(&inner), err));
    }
    *lock(&inner.readers) = readers;
    Ok(Attempt { inner })
}

fn spawn_agy_turn(inner: Arc<AttemptInner>, profile: Arc<AgyProfile>, stdout: ChildStdout, stdin: ChildStdin, spec: LaunchSpec, prompt: String) -> JoinHandle<()> {
    thread::spawn(move || {
        let result = crate::agy::run_turn(BufReader::new(stdout), stdin, &spec, &profile, &prompt,
            |line| {
                inner.progress.push(line);
                lock(&inner.audit).observe_stdout(line);
            },
            |model| { lock(&inner.audit).judge(model); });
        if let Err(error) = result {
            if !inner.killed.load(Ordering::SeqCst) {
                lock(&inner.audit).fail(error);
                terminate(&inner);
            }
        }
    })
}

/// Reads stdout line by line. Each line feeds the progress text and the audit. A violation ends the process tree
/// at once, so the runtime cannot act after a failed check.
fn spawn_stdout_reader(inner: Arc<AttemptInner>, out: ChildStdout) -> JoinHandle<()> {
    thread::spawn(move || {
        let mut reader = BufReader::new(out);
        while let Ok(Some(line)) = read_bounded_line(&mut reader, MAX_LINE_BYTES) {
            inner.progress.push(&line);
            let stop = lock(&inner.audit).observe_stdout(&line);
            if stop {
                terminate(&inner);
            }
        }
        lock(&inner.audit).finish_stdout();
    })
}

fn spawn_stderr_reader(inner: Arc<AttemptInner>, err: ChildStderr) -> JoinHandle<()> {
    thread::spawn(move || {
        let mut reader = BufReader::new(err);
        while let Ok(Some(line)) = read_bounded_line(&mut reader, MAX_LINE_BYTES) {
            inner.stderr.push(&line);
        }
    })
}

/// Marks the attempt as killed and ends its job. Used for a failed check; `kill_tree` does the same for other reasons.
fn terminate(inner: &AttemptInner) {
    inner.killed.store(true, Ordering::SeqCst);
    let _ = inner.job.terminate(1);
}

/// Reads one line without its line break (a trailing `\r` is removed too). A line longer than `max` bytes keeps its
/// first `max` bytes and a marker, and the rest of it is skipped. `Ok(None)` means the stream ended.
pub(crate) fn read_bounded_line<R: BufRead>(reader: &mut R, max: usize) -> std::io::Result<Option<String>> {
    let mut line: Vec<u8> = Vec::new();
    let mut cut = false;
    let mut seen_any = false;
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            break;
        }
        seen_any = true;
        let (take, done) = match available.iter().position(|b| *b == b'\n') {
            Some(index) => (index, true),
            None => (available.len(), false),
        };
        let room = max.saturating_sub(line.len());
        if take > room {
            line.extend_from_slice(&available[..room]);
            cut = true;
        } else {
            line.extend_from_slice(&available[..take]);
        }
        let consumed = if done { take + 1 } else { take };
        reader.consume(consumed);
        if done {
            break;
        }
    }
    if !seen_any {
        return Ok(None);
    }
    if line.last() == Some(&b'\r') {
        line.pop();
    }
    let mut text = String::from_utf8_lossy(&line).into_owned();
    if cut {
        text.push_str(&format!(" [line cut after {max} bytes]"));
    }
    Ok(Some(text))
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Attempt {
    /// Process id of the runtime (the tree root).
    pub fn pid(&self) -> u32 {
        self.inner.pid
    }

    /// Stdout lines (progress text only).
    pub fn progress(&self) -> Arc<ProgressBuffer> {
        Arc::clone(&self.inner.progress)
    }

    /// Stderr lines (diagnostics).
    pub fn stderr(&self) -> Arc<ProgressBuffer> {
        Arc::clone(&self.inner.stderr)
    }

    /// True after `kill_tree` was requested on this attempt, or after a failed model check ended it.
    pub fn was_killed(&self) -> bool {
        self.inner.killed.load(Ordering::SeqCst)
    }

    /// The violation that fails this attempt's job: a failed model check, or a transcript that could not be written.
    pub fn violation(&self) -> Option<DltError> {
        lock(&self.inner.audit).violation()
    }

    /// The model check so far.
    pub fn model_check(&self) -> ModelCheck {
        lock(&self.inner.audit).check().clone()
    }

    /// Ends every process in this attempt's job: the runtime and any process it started. Other
    /// processes on the machine are not touched. Safe to call more than once.
    pub fn kill_tree(&self) -> DltResult<()> {
        self.inner.killed.store(true, Ordering::SeqCst);
        self.inner
            .job
            .terminate(1)
            .map_err(|e| DltError::new(ErrorCode::Io, format!("cannot terminate the job of attempt {}: {e}", self.pid())))
    }

    /// Process ids still in the job.
    pub fn process_ids(&self) -> DltResult<Vec<u32>> {
        self.inner
            .job
            .process_ids()
            .map_err(|e| DltError::new(ErrorCode::Io, format!("cannot query the job: {e}")))
    }

    /// Waits for the runtime to exit, up to `timeout`. After an exit, any process the runtime left behind is ended
    /// too, and the output readers are given up to `READER_JOIN` to reach the end of their pipes. So the model check
    /// is final when this returns an exit.
    pub fn wait_timeout(&self, timeout: Duration) -> DltResult<WaitOutcome> {
        let deadline = Instant::now() + timeout;
        loop {
            let status = {
                let mut child = lock(&self.inner.child);
                child.try_wait()?
            };
            if let Some(status) = status {
                let code = status.code().unwrap_or(-1);
                *lock(&self.inner.exit_code) = Some(code);
                let _ = self.inner.job.terminate(0);
                self.join_readers(READER_JOIN);
                return Ok(WaitOutcome::Exited(code));
            }
            if Instant::now() >= deadline {
                return Ok(WaitOutcome::TimedOut);
            }
            thread::sleep(EXIT_POLL);
        }
    }

    /// Ends the audit: waits for the output readers, then writes the stderr tail and the end record. Call it once per
    /// attempt, after the outcome is known. Later calls do nothing.
    pub fn finish_audit(&self) {
        self.join_readers(READER_JOIN);
        let stderr_lines = self.inner.stderr.snapshot();
        let exit_code = *lock(&self.inner.exit_code);
        let killed = self.was_killed();
        lock(&self.inner.audit).finish(exit_code, killed, &stderr_lines);
    }

    /// Waits up to `timeout` for each output reader to finish. A reader that is still running after that is left
    /// to finish on its own; the audit then keeps whatever it already wrote.
    fn join_readers(&self, timeout: Duration) {
        let handles: Vec<JoinHandle<()>> = std::mem::take(&mut *lock(&self.inner.readers));
        let deadline = Instant::now() + timeout;
        for handle in handles {
            while !handle.is_finished() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(5));
            }
            if handle.is_finished() {
                let _ = handle.join();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufReader;

    #[test]
    fn bounded_lines_keep_their_text_and_cut_the_long_ones() {
        let data: &[u8] = b"first\r\nsecond\nthird-without-newline";
        let mut reader = BufReader::new(data);
        assert_eq!(read_bounded_line(&mut reader, 100).unwrap().as_deref(), Some("first"));
        assert_eq!(read_bounded_line(&mut reader, 100).unwrap().as_deref(), Some("second"));
        assert_eq!(read_bounded_line(&mut reader, 100).unwrap().as_deref(), Some("third-without-newline"));
        assert_eq!(read_bounded_line(&mut reader, 100).unwrap(), None);

        let mut long = vec![b'a'; 50];
        long.extend_from_slice(b"\nnext\n");
        let mut reader = BufReader::new(&long[..]);
        let cut = read_bounded_line(&mut reader, 10).unwrap().expect("a line");
        assert!(cut.starts_with("aaaaaaaaaa") && cut.contains("cut after 10 bytes"), "{cut}");
        assert_eq!(read_bounded_line(&mut reader, 10).unwrap().as_deref(), Some("next"), "the next line starts cleanly");
    }
}
