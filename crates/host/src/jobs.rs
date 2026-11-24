//! AI job supervision.
//!
//! The core owns job state (queued → running → validating → completed, or failed/conflict/canceled).
//! This module owns the processes. Rules:
//! - A runner thread starts a runtime for a queued attempt. It holds no core lock while the runtime starts
//!   or runs. Core calls take the lock for one operation each.
//! - Cancellation is decided by the core. The sweeper ends the process tree of any attempt that is no
//!   longer active in the core, so the kill follows the core's decision and never guesses.
//! - `host.stop` cancels every active job in the core and then ends every owned tree.
//! - When a host starts, the processes of its earlier jobs are gone: their job objects closed with the host. Every
//!   queued, running or validating job is failed with reason `host_restarted`, which also releases its target
//!   reservations. A new attempt starts only through `job.resume`.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Condvar, Mutex};
use std::thread;

use designllm_contracts::{ArtifactKind, DltError, DltResult, Diagnostic, ErrorCode, Job, JobRef, JobStartRequest, JobStatus};
use designllm_runtimes::{detect, start_with_transcript_watched, Attempt, JobWatch, LaunchSpec, McpCommand, ProgressBuffer, RuntimeKind, WaitOutcome};

use crate::shared::{lock, Shared};

/// Stderr lines attached to a failed job's diagnostics.
const STDERR_TAIL_LINES: usize = 20;

#[derive(Clone)]
struct Running {
    attempt_id: String,
    attempt: Attempt,
}

/// Process-level state for jobs. Core state is not stored here.
pub(crate) struct JobSupervisor {
    running: Mutex<HashMap<String, Running>>,
    launched: Mutex<HashSet<String>>,
    progress: Mutex<HashMap<String, std::sync::Arc<ProgressBuffer>>>,
    slots_used: Mutex<usize>,
    slots_freed: Condvar,
    /// Attempts whose processes may still run, from their launch until the launch returns (see `keep_alive`).
    live: Mutex<Vec<(u64, JobWatch)>>,
    next_live: AtomicU64,
}

impl JobSupervisor {
    pub(crate) fn new() -> Self {
        JobSupervisor {
            running: Mutex::new(HashMap::new()),
            launched: Mutex::new(HashSet::new()),
            progress: Mutex::new(HashMap::new()),
            slots_used: Mutex::new(0),
            slots_freed: Condvar::new(),
            live: Mutex::new(Vec::new()),
            next_live: AtomicU64::new(0),
        }
    }

    fn acquire_slot(&self, max: usize) {
        let mut used = lock(&self.slots_used);
        while *used >= max {
            used = self.slots_freed.wait(used).unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        *used += 1;
    }

    fn release_slot(&self) {
        *lock(&self.slots_used) -= 1;
        self.slots_freed.notify_one();
    }

    /// True the first time an attempt is seen, so each queued attempt gets exactly one runner.
    fn mark_launched(&self, job: &JobRef) -> bool {
        lock(&self.launched).insert(format!("{}/{}", job.job_id, job.attempt_id))
    }

    fn register(&self, job: &JobRef, attempt: &Attempt) {
        lock(&self.progress).insert(job.job_id.clone(), attempt.progress());
        lock(&self.running).insert(
            job.job_id.clone(),
            Running { attempt_id: job.attempt_id.clone(), attempt: attempt.clone() },
        );
    }

    fn unregister(&self, job: &JobRef) {
        let mut running = lock(&self.running);
        if running.get(&job.job_id).is_some_and(|r| r.attempt_id == job.attempt_id) {
            running.remove(&job.job_id);
        }
    }

    /// Snapshot of (job id, attempt id, handle) for every attempt with a live process.
    pub(crate) fn running_snapshot(&self) -> Vec<(String, String, Attempt)> {
        lock(&self.running).iter().map(|(job, r)| (job.clone(), r.attempt_id.clone(), r.attempt.clone())).collect()
    }

    /// Ends the process tree of the job's current attempt, if one runs. Called after a core cancel commit.
    pub(crate) fn kill_job(&self, job_id: &str) {
        let attempt = lock(&self.running).get(job_id).map(|r| r.attempt.clone());
        if let Some(attempt) = attempt {
            let _ = attempt.kill_tree();
        }
    }

    /// Retained progress lines of the job's latest attempt.
    pub(crate) fn progress_of(&self, job_id: &str) -> Option<Vec<String>> {
        lock(&self.progress).get(job_id).map(|p| p.snapshot())
    }

    /// Number of attempts with a live process.
    pub(crate) fn running_count(&self) -> usize {
        lock(&self.running).len()
    }
}

impl JobSupervisor {
    /// Lists the attempt's job in `owns_pid` until the guard drops. The launch passes the job before the runtime runs,
    /// and the list keeps it until the launch returns, after `settle` and the audit have ended the attempt's processes.
    pub(crate) fn keep_alive(&self, watch: &JobWatch) -> LiveAttempt<'_> {
        let id = self.next_live.fetch_add(1, Ordering::Relaxed);
        lock(&self.live).push((id, watch.clone()));
        LiveAttempt { jobs: self, id }
    }

    /// True when `pid` is the runtime of a live attempt or one of the processes it started. Both are in the attempt's
    /// job object. A failed query is an error, which the caller treats as a refusal.
    pub(crate) fn owns_pid(&self, pid: u32) -> DltResult<bool> {
        let watches: Vec<JobWatch> = lock(&self.live).iter().map(|(_, watch)| watch.clone()).collect();
        for watch in watches {
            if watch.process_ids()?.contains(&pid) {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

/// Ends one [`JobSupervisor::keep_alive`] entry when dropped.
pub(crate) struct LiveAttempt<'a> {
    jobs: &'a JobSupervisor,
    id: u64,
}

impl Drop for LiveAttempt<'_> {
    fn drop(&mut self) {
        lock(&self.jobs.live).retain(|(id, _)| *id != self.id);
    }
}

/// Startup recovery, run before the host accepts clients. The host that ran these jobs has stopped, and its job
/// objects closed with it, so no runtime process of this workspace is left. Every queued, running or validating job
/// is failed with `host_restarted`. The core releases a failed job's target reservations.
pub(crate) fn recover(shared: &std::sync::Arc<Shared>) {
    let active = active_jobs(shared).unwrap_or_default();
    for job in active {
        let job_ref = job_ref(&job);
        let error = DltError::new(
            ErrorCode::Internal,
            "host_restarted: the host stopped before this job finished. Use job.resume to start a new attempt.",
        );
        let _ = shared.core_call(|ws| ws.set_job_status(&job_ref, JobStatus::Failed, Some(error)));
    }
}

/// One supervision tick: start runners for newly queued attempts, and end processes that the core no
/// longer lists as active for their job.
pub(crate) fn sweep(shared: &std::sync::Arc<Shared>) {
    let Ok(active) = active_jobs(shared) else { return };
    for job in active.iter().filter(|j| j.status == JobStatus::Queued) {
        let job_ref = job_ref(job);
        if shared.jobs.mark_launched(&job_ref) {
            spawn_runner(shared, job_ref);
        }
    }
    for (job_id, attempt_id, attempt) in shared.jobs.running_snapshot() {
        let still_active = active.iter().any(|j| j.id == job_id && j.attempt_id == attempt_id);
        if !still_active && !attempt.was_killed() {
            let _ = attempt.kill_tree();
        }
    }
}

/// host.stop: cancels every active job in the core, then ends every owned process tree. Returns the
/// canceled job ids.
pub(crate) fn stop_all(shared: &std::sync::Arc<Shared>) -> Vec<String> {
    let mut canceled = Vec::new();
    for job in active_jobs(shared).unwrap_or_default() {
        if shared.core_call(|ws| ws.cancel_job(&job.id)).is_ok() {
            canceled.push(job.id.clone());
        }
    }
    for (_, _, attempt) in shared.jobs.running_snapshot() {
        let _ = attempt.kill_tree();
    }
    canceled
}

/// Core job list. An unsupported query means the core has no job model yet, so no job can be active.
/// Any other failure makes the caller treat the state as unknown.
fn active_jobs(shared: &std::sync::Arc<Shared>) -> DltResult<Vec<Job>> {
    match shared.core_call(|ws| ws.active_jobs()) {
        Ok(jobs) => Ok(jobs),
        Err(e) if e.code == ErrorCode::UnsupportedOperation => Ok(Vec::new()),
        Err(e) => Err(e),
    }
}

fn job_ref(job: &Job) -> JobRef {
    JobRef { job_id: job.id.clone(), attempt_id: job.attempt_id.clone() }
}

fn spawn_runner(shared: &std::sync::Arc<Shared>, job: JobRef) {
    let shared_for_thread = std::sync::Arc::clone(shared);
    let job_for_thread = job.clone();
    let spawned = thread::Builder::new()
        .name(format!("designllm-job-{}", job.job_id))
        .spawn(move || run_attempt(shared_for_thread, job_for_thread));
    if let Err(e) = spawned {
        let error = DltError::new(ErrorCode::Io, format!("cannot start the job runner thread: {e}"));
        let _ = shared.core_call(|ws| ws.set_job_status(&job, JobStatus::Failed, Some(error)));
    }
}

/// Runner thread: waits for a parallel slot, then runs the attempt.
fn run_attempt(shared: std::sync::Arc<Shared>, job: JobRef) {
    shared.jobs.acquire_slot(shared.options.max_parallel_jobs.max(1));
    let outcome = launch_and_wait(&shared, &job);
    shared.jobs.release_slot();
    if let Err(error) = outcome {
        let _ = shared.core_call(|ws| ws.set_job_status(&job, JobStatus::Failed, Some(error)));
    }
}

/// Starts the runtime for one attempt and reports its lifecycle to the core. Returns the error that should
/// fail the job. A canceled or superseded attempt returns Ok: the core already holds the final state.
fn launch_and_wait(shared: &std::sync::Arc<Shared>, job: &JobRef) -> Result<(), DltError> {
    let job_context = shared.core_call(|ws| -> DltResult<Option<(JobStartRequest, ArtifactKind)>> {
        let current = ws.job(&job.job_id)?;
        if current.attempt_id != job.attempt_id || current.status != JobStatus::Queued {
            return Ok(None);
        }
        let request = ws.job_start_request(&job.job_id)?;
        let artifact_kind = ws.artifact_kind(&request.artifact_id)?;
        Ok(Some((request, artifact_kind)))
    })?;
    let Some((request, artifact_kind)) = job_context else {
        return Ok(());
    };
    let kind = RuntimeKind::parse(&request.runtime)?;
    // The OS lock spans workspaces and processes of this user, and is released on crash.
    let _provider_lease = if kind == RuntimeKind::Claude {
        let root = std::env::var_os("LOCALAPPDATA").ok_or_else(|| DltError::invalid("LOCALAPPDATA is required for Claude serialization"))?;
        let path = std::path::PathBuf::from(root).join("DesignLLMTool/runtime-locks/claude.lock");
        let lease = crate::provider::acquire(&path, || shared.core_call(|ws| {
            let current = ws.job(&job.job_id)?;
            Ok(current.attempt_id == job.attempt_id && current.status == JobStatus::Queued)
        }))?;
        let Some(lease) = lease else { return Ok(()); };
        Some(lease)
    } else { None };
    let info = detect(kind, &shared.options.runtime)?;
    let program = match &shared.options.host_program {
        Some(path) => path.clone(),
        None => std::env::current_exe()
            .map_err(|e| DltError::new(ErrorCode::Io, format!("cannot locate the application executable: {e}")))?,
    };
    let spec = LaunchSpec {
        job: job.clone(),
        request,
        workspace_root: shared.root.clone(),
        mcp: McpCommand::for_job(&program, &shared.root, job),
        design_guidance: designllm_bridge::manual::job_design_guidance(artifact_kind),
    };
    let transcript = transcript_path(&shared.root, job)?;
    // The job is known before the runtime runs: the start hands it to the callback and resumes the process after it.
    let mut _live = None;
    let attempt = start_with_transcript_watched(&info, &spec, Some(&transcript), |watch| {
        _live = Some(shared.jobs.keep_alive(&watch));
    })?;
    shared.jobs.register(job, &attempt);

    if shared.core_call(|ws| ws.set_job_status(job, JobStatus::Running, None)).is_err() {
        // Canceled or superseded while starting: the core refused the transition, so the process must not run.
        let _ = attempt.kill_tree();
        shared.jobs.unregister(job);
        let _ = attempt.wait_timeout(std::time::Duration::from_secs(10));
        attempt.finish_audit();
        return Ok(());
    }

    let outcome = attempt.wait_timeout(shared.options.max_job_duration);
    shared.jobs.unregister(job);
    let result = match outcome {
        Ok(outcome) => settle(&attempt, outcome, shared, job),
        Err(error) => Err(error),
    };
    attempt.finish_audit();
    result
}

/// Decides the job outcome from the wait result and the attempt's audit. A failed model check fails the job with its
/// own error, also when the attempt was stopped because of it. A canceled attempt is not a failure.
fn settle(attempt: &Attempt, outcome: WaitOutcome, shared: &std::sync::Arc<Shared>, job: &JobRef) -> Result<(), DltError> {
    let violation = attempt.violation();
    match outcome {
        WaitOutcome::TimedOut => {
            let _ = attempt.kill_tree();
            if let Some(violation) = violation {
                return Err(violation);
            }
            Err(DltError::new(
                ErrorCode::Internal,
                format!("the runtime exceeded the maximum job duration of {} s", shared.options.max_job_duration.as_secs()),
            ))
        }
        WaitOutcome::Exited(code) => {
            if let Some(violation) = violation {
                if attempt.was_killed() || code == 0 {
                    return Err(violation);
                }
            }
            if attempt.was_killed() {
                return Ok(());
            }
            if code == 0 {
                shared.core_call(|ws| ws.set_job_status(job, JobStatus::Validating, None))?;
                shared.core_call(|ws| ws.set_job_status(job, JobStatus::Completed, None))?;
                return Ok(());
            }
            let tail: Vec<String> = {
                let lines = attempt.stderr().snapshot();
                let start = lines.len().saturating_sub(STDERR_TAIL_LINES);
                lines[start..].to_vec()
            };
            let mut error = DltError::new(ErrorCode::Internal, format!("the runtime exited with code {code}"));
            if !tail.is_empty() {
                error.diagnostics.push(Diagnostic {
                    code: "runtime_stderr_tail".into(),
                    message: tail.join("\n"),
                    node_id: None,
                    blocking: false,
                    params: Default::default(),
                });
            }
            Err(error)
        }
    }
}

/// `<root>/logs/jobs/<job>/<attempt>.jsonl`. The ids come from the core; anything other than letters, digits, `-` and
/// `_` is refused, so a transcript path cannot leave its folder.
fn transcript_path(root: &std::path::Path, job: &JobRef) -> Result<std::path::PathBuf, DltError> {
    let plain = |id: &str| {
        !id.is_empty() && id.len() <= 128 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    };
    if !plain(&job.job_id) || !plain(&job.attempt_id) {
        return Err(DltError::invalid(
            "job and attempt ids must be 1 to 128 letters, digits, '-' or '_' to name a transcript",
        ));
    }
    Ok(root.join("logs").join("jobs").join(&job.job_id).join(format!("{}.jsonl", job.attempt_id)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mark_launched_is_true_once_per_attempt() {
        let sup = JobSupervisor::new();
        let job = JobRef { job_id: "j".into(), attempt_id: "a1".into() };
        assert!(sup.mark_launched(&job));
        assert!(!sup.mark_launched(&job));
        let resumed = JobRef { job_id: "j".into(), attempt_id: "a2".into() };
        assert!(sup.mark_launched(&resumed), "a resumed attempt is a new launch");
    }

    #[test]
    fn kill_job_on_an_unknown_job_is_a_no_op() {
        JobSupervisor::new().kill_job("missing");
    }
}
