//! State shared by the accept loop, connection threads, the job supervisor, and the event pump.
//!
//! Lock rules:
//! - `core` (the workspace) is held only for one core call or one query. Never across pipe I/O, a process
//!   start, or a wait. A PDF read runs between two core calls (`Workspace::dispatch_start`, task 34d).
//! - `pdf_reads` lets one PDF read run at a time host-wide, without `core`. Its own lock is held only to list a read or
//!   to take or give the turn, never across the read. A read is listed under `core` (`PdfReads::list`); `core` is never
//!   taken under it.
//! - `life` decides admission and idle exit under one lock, so a client cannot be admitted after the host
//!   has decided to exit.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, Weak};
use std::time::Duration;

use designllm_contracts::{CoreEvent, DltResult};
use designllm_core::{EventBatch, PdfCancel, Workspace};

use crate::jobs::JobSupervisor;
use crate::{HostMode, HostOptions};

/// Events requested from the core per batch.
pub(crate) const EVENT_BATCH: usize = 256;
/// Events queued for one subscriber before the queue is replaced by a resync marker.
const SUBSCRIBER_QUEUE_LIMIT: usize = 4096;

pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// How often a PDF read waiting for its turn checks its cancel.
const PDF_TURN_POLL: Duration = Duration::from_millis(50);

/// The host's PDF reads (task 34d). One runs at a time, as when each read ran under `core`: each worker may commit up to its
/// memory cap. Every read is listed with its cancel while it waits or runs, so a job's cancel or the host's stop reaches it.
#[derive(Default)]
pub(crate) struct PdfReads {
    state: Mutex<PdfReadState>,
    turn_freed: Condvar,
}

#[derive(Default)]
struct PdfReadState {
    running: bool,
    next_id: u64,
    reads: Vec<ListedRead>,
}

struct ListedRead {
    id: u64,
    job_id: Option<String>,
    cancel: PdfCancel,
}

impl PdfReads {
    /// Lists a read with its cancel: from now on a job's cancel or the host's stop reaches it. Run it with [`Listed::run`].
    pub(crate) fn list<'a>(&'a self, job_id: Option<&str>, cancel: &'a PdfCancel) -> Listed<'a> {
        let mut state = lock(&self.state);
        let id = state.next_id;
        state.next_id += 1;
        state.reads.push(ListedRead { id, job_id: job_id.map(str::to_string), cancel: cancel.clone() });
        Listed { reads: self, id, cancel, holds_turn: false }
    }

    /// Cancels the reads of one job, waiting or running.
    pub(crate) fn cancel_job(&self, job_id: &str) {
        self.cancel_where(|read| read.job_id.as_deref() == Some(job_id));
    }

    /// Cancels every read (the host stops).
    pub(crate) fn cancel_all(&self) {
        self.cancel_where(|_| true);
    }

    /// Cancels the matching reads and wakes the waiting ones, so a canceled read leaves the wait at once.
    fn cancel_where(&self, matches: impl Fn(&ListedRead) -> bool) {
        for read in lock(&self.state).reads.iter().filter(|read| matches(read)) {
            read.cancel.cancel();
        }
        self.turn_freed.notify_all();
    }

    #[cfg(test)]
    fn listed(&self) -> usize {
        lock(&self.state).reads.len()
    }
}

/// One read's place in [`PdfReads`]. Dropping it, also by a panic of the read, unlists the read and gives its turn back.
pub(crate) struct Listed<'a> {
    reads: &'a PdfReads,
    id: u64,
    cancel: &'a PdfCancel,
    holds_turn: bool,
}

impl Listed<'_> {
    /// Runs `read` in its turn. Returns `None`, without running it, when the read's cancel is set before the turn comes.
    pub(crate) fn run<T>(mut self, read: impl FnOnce() -> T) -> Option<T> {
        let reads = self.reads;
        {
            let mut state = lock(&reads.state);
            while state.running && !self.cancel.is_canceled() {
                state = match reads.turn_freed.wait_timeout(state, PDF_TURN_POLL) {
                    Ok((state, _)) => state,
                    Err(poisoned) => poisoned.into_inner().0,
                };
            }
            if self.cancel.is_canceled() {
                return None;
            }
            state.running = true;
            self.holds_turn = true;
        }
        Some(read())
    }
}

impl Drop for Listed<'_> {
    fn drop(&mut self) {
        let mut state = lock(&self.reads.state);
        state.reads.retain(|read| read.id != self.id);
        if self.holds_turn {
            state.running = false;
        }
        drop(state);
        self.reads.turn_freed.notify_all();
    }
}

/// Admission and idle state. Guarded by `Shared::life`.
#[derive(Debug, Default)]
pub(crate) struct Life {
    /// Connections past admission and not yet closed.
    pub clients: usize,
    /// Set when the host has decided to exit. No connection is admitted afterwards.
    pub closing: bool,
    /// At least one client has connected since start (selects startup or idle grace).
    pub ever_connected: bool,
}

/// One pushed-event subscription (a connection that sent `events.subscribe`).
pub(crate) struct Subscriber {
    state: Mutex<SubscriberState>,
}

struct SubscriberState {
    after: u64,
    queue: VecDeque<CoreEvent>,
}

impl Subscriber {
    pub(crate) fn new(after: u64) -> Arc<Self> {
        Arc::new(Subscriber { state: Mutex::new(SubscriberState { after, queue: VecDeque::new() }) })
    }

    pub(crate) fn after(&self) -> u64 {
        lock(&self.state).after
    }

    /// Queues `event` if it is newer than the subscriber's cursor. A full queue becomes one
    /// `resync_required` marker, so the client re-reads state instead of losing events silently.
    pub(crate) fn offer(&self, event: &CoreEvent) {
        let mut state = lock(&self.state);
        if event.event_seq <= state.after {
            return;
        }
        if state.queue.len() >= SUBSCRIBER_QUEUE_LIMIT {
            state.queue.clear();
            state.queue.push_back(designllm_bridge::protocol::event(
                event.event_seq,
                "resync_required",
                None,
                serde_json::json!({ "reason": "subscriber queue overflow" }),
            ));
        } else {
            state.queue.push_back(event.clone());
        }
        state.after = event.event_seq;
    }

    /// Queues a resync marker at `seq` (the core reported the cursor is outside its retained range).
    pub(crate) fn offer_resync(&self, seq: u64, artifacts: serde_json::Value) {
        let mut state = lock(&self.state);
        state.queue.push_back(designllm_bridge::protocol::event(
            seq,
            "resync_required",
            None,
            serde_json::json!({ "artifacts": artifacts }),
        ));
        state.after = state.after.max(seq);
    }

    pub(crate) fn drain(&self) -> Vec<CoreEvent> {
        lock(&self.state).queue.drain(..).collect()
    }
}

/// Everything the host threads share.
pub(crate) struct Shared {
    pub core: Mutex<Workspace>,
    pub pdf_reads: PdfReads,
    pub root: PathBuf,
    pub pipe_name: String,
    pub mode: HostMode,
    pub options: HostOptions,
    pub pid: u32,
    pub life: Mutex<Life>,
    pub stopping: AtomicBool,
    pub subscribers: Mutex<Vec<Weak<Subscriber>>>,
    pub pump_seq: AtomicU64,
    pub jobs: JobSupervisor,
}

impl Shared {
    pub(crate) fn new(workspace: Workspace, pipe_name: String, mode: HostMode, options: HostOptions) -> Self {
        let root = workspace.root().to_path_buf();
        Shared {
            core: Mutex::new(workspace),
            pdf_reads: PdfReads::default(),
            root,
            pipe_name,
            mode,
            options,
            pid: std::process::id(),
            life: Mutex::new(Life::default()),
            stopping: AtomicBool::new(false),
            subscribers: Mutex::new(Vec::new()),
            pump_seq: AtomicU64::new(0),
            jobs: JobSupervisor::new(),
        }
    }

    /// Runs one core operation under the workspace lock. The lock is released before the result is used.
    pub(crate) fn core_call<T>(&self, f: impl FnOnce(&mut Workspace) -> T) -> T {
        let mut ws = lock(&self.core);
        f(&mut ws)
    }

    /// Admits one connection. Returns false once the host is closing.
    pub(crate) fn admit(&self) -> bool {
        let mut life = lock(&self.life);
        if life.closing {
            return false;
        }
        life.clients += 1;
        life.ever_connected = true;
        true
    }

    pub(crate) fn release(&self) {
        let mut life = lock(&self.life);
        life.clients = life.clients.saturating_sub(1);
    }

    pub(crate) fn is_stopping(&self) -> bool {
        self.stopping.load(Ordering::SeqCst)
    }

    /// Stops admission and the accept loop. Existing connections finish their current frame and close.
    pub(crate) fn begin_shutdown(&self) {
        lock(&self.life).closing = true;
        self.stopping.store(true, Ordering::SeqCst);
    }

    pub(crate) fn clients(&self) -> usize {
        lock(&self.life).clients
    }

    pub(crate) fn register_subscriber(&self, sub: &Arc<Subscriber>) {
        let mut subs = lock(&self.subscribers);
        subs.retain(|w| w.strong_count() > 0);
        subs.push(Arc::downgrade(sub));
    }

    /// Sends every event newer than the subscriber's cursor from the core to all live subscribers.
    /// Without subscribers nothing is read: a later subscriber's backfill starts from its own cursor.
    pub(crate) fn pump_events(&self) {
        if lock(&self.subscribers).iter().all(|w| w.strong_count() == 0) {
            return;
        }
        loop {
            let last = self.pump_seq.load(Ordering::SeqCst);
            let batch: DltResult<EventBatch> = self.core_call(|ws| ws.events_after(last, EVENT_BATCH));
            let Ok(batch) = batch else { return };
            if batch.resync_required {
                let marker = designllm_bridge::protocol::event(
                    batch.last_seq,
                    "resync_required",
                    None,
                    serde_json::json!({ "artifacts": batch.artifacts }),
                );
                self.broadcast(&marker);
            }
            for event in &batch.events {
                self.broadcast(event);
            }
            let advanced = batch.last_seq > last;
            if advanced {
                self.pump_seq.store(batch.last_seq, Ordering::SeqCst);
            }
            if !advanced || batch.events.len() < EVENT_BATCH {
                return;
            }
        }
    }

    fn broadcast(&self, event: &CoreEvent) {
        let mut subs = lock(&self.subscribers);
        subs.retain(|w| match w.upgrade() {
            Some(sub) => {
                sub.offer(event);
                true
            }
            None => false,
        });
    }

    /// Brings a new subscriber up to date from the core. It is registered first, so no event is missed.
    pub(crate) fn backfill(&self, sub: &Subscriber) {
        loop {
            let after = sub.after();
            let batch = self.core_call(|ws| ws.events_after(after, EVENT_BATCH));
            let Ok(batch) = batch else { return };
            if batch.resync_required {
                sub.offer_resync(batch.last_seq, serde_json::json!(batch.artifacts));
            }
            for event in &batch.events {
                sub.offer(event);
            }
            if batch.events.len() < EVENT_BATCH || batch.last_seq <= after {
                return;
            }
        }
    }

}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use std::time::Instant;

    /// Starts a read of job `job` on its own thread. It holds the turn until `release` is set; `None` if it never got the turn.
    fn read_until(reads: &Arc<PdfReads>, job: &str, cancel: &PdfCancel, release: &PdfCancel) -> thread::JoinHandle<Option<()>> {
        let (reads, job, cancel, release) = (Arc::clone(reads), job.to_string(), cancel.clone(), release.clone());
        thread::spawn(move || {
            reads.list(Some(&job), &cancel).run(|| {
                while !release.is_canceled() {
                    thread::sleep(Duration::from_millis(5));
                }
            })
        })
    }

    fn wait_until(what: &str, condition: impl Fn() -> bool) {
        let started = Instant::now();
        while !condition() {
            assert!(started.elapsed() < Duration::from_secs(5), "{what}");
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn a_canceled_job_leaves_the_wait_and_its_running_read_is_told_to_stop() {
        let reads = Arc::new(PdfReads::default());
        let (first_cancel, release) = (PdfCancel::new(), PdfCancel::new());
        let first = read_until(&reads, "job-a", &first_cancel, &release);
        wait_until("the first read runs", || lock(&reads.state).running);
        let waiting_cancel = PdfCancel::new();
        let waiting = read_until(&reads, "job-b", &waiting_cancel, &PdfCancel::new());
        wait_until("the second read is listed", || reads.listed() == 2);

        reads.cancel_job("job-b");
        assert_eq!(waiting.join().expect("waiting read"), None, "the canceled read never ran");
        assert!(!first_cancel.is_canceled(), "another job's read is not canceled");
        reads.cancel_job("job-a");
        assert!(first_cancel.is_canceled(), "the running read is told to stop");
        release.cancel();
        assert_eq!(first.join().expect("first read"), Some(()));
        assert_eq!(reads.listed(), 0);
        assert!(!lock(&reads.state).running);
    }

    #[test]
    fn a_read_that_panics_gives_its_turn_back() {
        let reads = Arc::new(PdfReads::default());
        let panicking = Arc::clone(&reads);
        let outcome = thread::spawn(move || panicking.list(None, &PdfCancel::new()).run(|| panic!("reader panics"))).join();
        assert!(outcome.is_err());
        assert_eq!(reads.listed(), 0);
        assert_eq!(reads.list(None, &PdfCancel::new()).run(|| 7), Some(7), "the next read gets the turn");
    }
}
