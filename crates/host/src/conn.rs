//! One client connection: handshake, routing, responses, and pushed events.
//!
//! Routing order for a request:
//! 1. `system.hello` is answered by the host. It merges the core's identity (workspace id, registered kinds).
//! 2. Host methods (`host.*`, `events.subscribe`) are answered by the host.
//! 3. Everything else goes to the core: `dispatch` for gui/cli, and `dispatch_bound` with the verified job
//!    binding for a job-bound bridge. Only the core lock is held, and only for that one call.
//!    The GUI-only methods are refused before routing unless the connection holds the gui role (see
//!    `gui_only_refusal`). A connection without the gui role may not open a review gate (`review_refusal`), and its actors are
//!    fixed to its identity before the core sees them (`identity`).
//!
//! Core requests run on the connection's core lane, one at a time in arrival order (task 34d). The connection
//! thread keeps reading frames meanwhile, so it answers host methods such as `host.heartbeat` while a long core
//! request runs. A host method's response can therefore come before that of an earlier core request: the host
//! methods only read host state (heartbeat, status, job progress), start a subscription whose cursor still covers
//! the queued requests' events, or stop the host, which is meant to overtake. Pushed events wait while a core
//! request runs, so an event of a change follows that change's response. A PDF read (`source.extract_pdf`) runs
//! between two core calls, holds neither the core lock nor the event turn while the reader runs, and stops when
//! its connection ends, its job is canceled, or the host stops. A render that is not cached (`render.get`, and the gui's
//! `render.pixels`, whose raw pixels go as binary frames right before its response) draws the same way (68i).

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, TryLockError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use designllm_bridge::frame::{binary_header, encode_frame};
use designllm_bridge::pipe::{Pipe, ReadOutcome};
use designllm_bridge::review_gate;
use designllm_bridge::operation_exposure;
use designllm_bridge::protocol::{
    self, CLIENT_ROLES, METHOD_EVENTS_SUBSCRIBE, METHOD_HEARTBEAT, METHOD_HELLO, METHOD_JOB_PROGRESS, METHOD_STATUS,
    METHOD_STOP,
};
use designllm_contracts::{
    new_id, CoreEvent, DltError, ErrorCode, IpcMessage, JobRef, JobStatus, RpcRequest, RpcResponse, UiStateView,
    MAX_FRAME_BYTES, PROTOCOL_VERSION, SCHEMA_VERSION,
};
use designllm_core::{ClientConnection, Dispatch, PdfCancel, PdfOpen, PdfRead, RenderJob, RenderedPage, Workspace};
use serde_json::{json, Value};

use crate::gui_identity;
use crate::identity::Identity;
use crate::jobs;
use crate::rasters::{permits_for, RasterPermits};
use crate::shared::{lock, Listed, Shared, Subscriber};
use crate::HostMode;

/// Poll slice for reads. Pushed events and shutdown are noticed at this period.
const POLL: Duration = Duration::from_millis(100);
/// Deadline for one frame write. A client that stops reading is dropped after this.
const WRITE_DEADLINE: Duration = Duration::from_secs(10);
/// A new connection must send `system.hello` within this time.
const HELLO_DEADLINE: Duration = Duration::from_secs(10);
/// Core requests one connection may have waiting on its core lane. At this count the host stops reading the
/// connection's frames until the lane catches up, as it did while it ran each core request itself.
const MAX_QUEUED_CORE_REQUESTS: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    /// The application's own process: `client_role: "gui"`, verified by `gui_identity`. Never job-bound, because
    /// `job_ref` needs `bridge`.
    Gui,
    /// MCP bridges, including the job-bound bridge of a runtime's attempt.
    Bridge,
    Cli,
}

impl Role {
    fn name(self) -> &'static str {
        match self {
            Role::Gui => "gui",
            Role::Bridge => "bridge",
            Role::Cli => "cli",
        }
    }
}

enum Flow {
    Continue,
    Close,
    Stop,
}

/// Where one request goes.
enum Routed {
    /// The host answered it.
    Answered(RpcResponse, Flow),
    /// The core answers it, on the connection's core lane.
    ToCore(RpcRequest),
}

/// Runs one connection to completion. The admission slot taken by `Shared::admit` is released here, after
/// the pipe is closed, so an idle managed host sees the client leave.
pub(crate) fn serve_connection(shared: Arc<Shared>, pipe: Pipe) {
    let mut conn = Connection::new(Arc::clone(&shared), pipe);
    conn.run();
    // The connection is over: its PDF reads stop now (a running worker is ended, a waiting read leaves the queue). Other queued
    // core requests still run, and the lane stops at its first response that cannot be written.
    conn.cancel.cancel();
    if let Some(lane) = conn.lane.take() {
        lane.close();
    }
    // The core lists a connection from its hello until its pipe closes.
    if let Some(record) = lock(&conn.reported).take() {
        shared.core_call(|ws| ws.client_disconnected(&record.connection_id));
    }
    // Nothing can reconnect as this connection: its reservations end now instead of after their ttl.
    if let Some(identity) = conn.identity.as_ref().filter(|identity| identity.per_connection()) {
        shared.core_call(|ws| ws.release_caller_reservations(identity.prefix()));
    }
    drop(conn);
    shared.release();
}

/// The pipe of one connection. The connection thread reads it. The connection thread and the core lane both write
/// to it, one whole frame at a time.
struct Link {
    pipe: Pipe,
    write: Mutex<()>,
    /// Held by the lane while a core request runs and its response is written, and by the connection thread while it
    /// writes pushed events. An event of a change therefore follows the change's response on this connection.
    turn: Mutex<()>,
}

impl Link {
    fn send(&self, message: &IpcMessage) -> Result<(), DltError> {
        let payload = protocol::encode(message)?;
        let frame = encode_frame(&payload)?;
        let _whole_frame = lock(&self.write);
        self.pipe
            .write_all(&frame, WRITE_DEADLINE)
            .map_err(|e| protocol::error(ErrorCode::Io, format!("write to client failed: {e}"), true))
    }

    /// `message` after `binary` as binary frames of at most `MAX_FRAME_BYTES` each, written under one hold of the write lock:
    /// no other frame comes between them, so the client hands the binary frames to that response (68i, `render.pixels`).
    fn send_after_binary(&self, message: &IpcMessage, binary: &[u8]) -> Result<(), DltError> {
        let payload = protocol::encode(message)?;
        let frame = encode_frame(&payload)?;
        let failed = |e: std::io::Error| protocol::error(ErrorCode::Io, format!("write to client failed: {e}"), true);
        let _whole_message = lock(&self.write);
        for chunk in binary.chunks(MAX_FRAME_BYTES) {
            self.pipe.write_all(&binary_header(chunk.len())?, WRITE_DEADLINE).map_err(failed)?;
            self.pipe.write_all(chunk, WRITE_DEADLINE).map_err(failed)?;
        }
        self.pipe.write_all(&frame, WRITE_DEADLINE).map_err(failed)
    }
}

/// What the core was last told about a connection. The connection thread sets it at the hello, the core lane at
/// `job.set_client`.
type Reported = Arc<Mutex<Option<ClientConnection>>>;

/// The thread that runs one connection's core requests in arrival order.
struct CoreLane {
    queue: Sender<QueuedRequest>,
    /// Requests sent to the lane whose responses are not written yet.
    waiting: Arc<AtomicUsize>,
    /// Set when the lane thread has ended: a response could not be written, or a request panicked.
    ended: Arc<AtomicBool>,
    thread: JoinHandle<()>,
}

struct QueuedRequest {
    request: RpcRequest,
    received: Instant,
}

impl CoreLane {
    fn start(lane: LaneContext) -> std::io::Result<CoreLane> {
        let (queue, requests) = mpsc::channel();
        let waiting = Arc::new(AtomicUsize::new(0));
        let ended = Arc::new(AtomicBool::new(false));
        let end = LaneEnd { waiting: Arc::clone(&waiting), ended: Arc::clone(&ended) };
        let thread = thread::Builder::new().name("designllm-host-core-lane".into()).spawn(move || {
            let end = end;
            lane.run(requests, &end.waiting);
        })?;
        Ok(CoreLane { queue, waiting, ended, thread })
    }

    fn waiting(&self) -> usize {
        self.waiting.load(Ordering::SeqCst)
    }

    fn has_ended(&self) -> bool {
        self.ended.load(Ordering::SeqCst)
    }

    /// Hands one request to the lane. False when the lane has ended.
    fn submit(&self, req: RpcRequest) -> bool {
        self.waiting.fetch_add(1, Ordering::SeqCst);
        if self.has_ended() || self.queue.send(QueuedRequest { request: req, received: Instant::now() }).is_err() {
            count_down(&self.waiting);
            return false;
        }
        true
    }

    /// No more requests: the lane answers those it holds, then ends.
    fn close(self) {
        drop(self.queue);
        let _ = self.thread.join();
    }
}

/// The end of a lane thread, also by a panic: the lane is marked ended and owes no response any more, so the
/// connection thread closes the connection instead of waiting for the requests still queued.
struct LaneEnd {
    waiting: Arc<AtomicUsize>,
    ended: Arc<AtomicBool>,
}

impl Drop for LaneEnd {
    fn drop(&mut self) {
        self.ended.store(true, Ordering::SeqCst);
        self.waiting.store(0, Ordering::SeqCst);
    }
}

/// What the core lane needs from its connection. The binding is fixed by the hello, before the first core request.
struct LaneContext {
    shared: Arc<Shared>,
    link: Arc<Link>,
    binding: Option<JobRef>,
    /// The connection holds the gui role (never job-bound): only it may open a review gate.
    gui: bool,
    /// The actor prefix and receipt fingerprint of a connection without the gui role (`identity`).
    caller: Option<(String, String)>,
    reported: Reported,
    /// Set when the connection ends: its PDF reads stop.
    cancel: PdfCancel,
}

impl LaneContext {
    fn run(self, requests: Receiver<QueuedRequest>, waiting: &AtomicUsize) {
        for req in requests {
            let sent = self.answer(req);
            // Counted down once the response is written, so the connection is not idle while it is written.
            count_down(waiting);
            if !sent {
                // The connection thread sees the broken pipe or the ended lane and closes. Requests still queued are not run.
                return;
            }
        }
    }

    /// Runs one core request and writes its response. False when the response could not be written.
    fn answer(&self, queued: QueuedRequest) -> bool {
        let QueuedRequest { request: req, received } = queued;
        let mut turn = lock(&self.link.turn);
        let method = req.method.clone();
        let job_id = req.params.get("job_id").and_then(Value::as_str).map(str::to_string);
        // job.set_client names this connection's MCP client; the listing shows the name from its success on.
        let named = (method == "job.set_client")
            .then(|| (clean_label(req.params.get("client_name")), clean_label(req.params.get("client_version"))));
        let binding = self.binding.clone();
        let gui = self.gui;
        let caller = self.caller.as_ref();
        let (reads, cancel) = (&self.shared.pdf_reads, &self.cancel);
        let start = self.shared.core_call(move |ws| {
            // The verified gui connection's file paths are the user's (core `dispatch_start_gui`); others are an agent's, judged
            // by the review gate first. The gate reads within the caller's scope, so its history previews see the same receipts.
            let dispatched = if gui {
                ws.dispatch_start_gui(req)
            } else {
                let gated = move |ws: &mut Workspace| match review_refusal(ws, &req, binding.as_ref()) {
                    Some(error) => Dispatch::Answered(RpcResponse::failure(req.id, error)),
                    None => ws.dispatch_start(req, binding),
                };
                match caller {
                    Some((caller, fingerprint)) => ws.with_caller(caller, fingerprint, gated),
                    None => gated(ws),
                }
            };
            match dispatched {
                Dispatch::Answered(response) => Started::Answered(response),
                Dispatch::ReadPdf(read) => {
                    // Listed under the core lock, where `job.cancel` commits: a cancel committed earlier is seen here, and a later
                    // one finds the read listed when it cancels the job's reads.
                    let listed = reads.list(read.job_id(), cancel);
                    if read.job_id().is_some_and(|job_id| ws.job(job_id).is_ok_and(|job| job.status == JobStatus::Canceled)) {
                        cancel.cancel();
                    }
                    Started::ReadPdf(read, listed)
                }
                Dispatch::OpenPdf { id, open } => Started::OpenPdf { id, open, listed: reads.list(None, cancel) },
                Dispatch::Render(job) => Started::Render(job),
            }
        });
        let (response, binary) = match start {
            Started::Answered(response) => (response, None),
            Started::Render(job) => {
                // A render draws without the core lock and without the turn, as a PDF read runs (68i): other connections are
                // served, and events committed meanwhile reach this connection.
                drop(turn);
                // Drawing renders take raster permits host-wide (`rasters.rs`). A wait ends when this connection closes or the host stops;
                // a job's cancel does not end it.
                let stop = || cancel.is_canceled() || self.shared.is_stopping();
                let drawn = draw(&job, &self.shared.rasters, stop);
                turn = lock(&self.link.turn);
                self.shared.core_call(|ws| ws.finish_render(job, drawn))
            }
            Started::ReadPdf(read, listed) => {
                // The reader runs without the core lock and without the turn: other connections are served, and events
                // committed meanwhile reach this connection.
                drop(turn);
                let reading = listed
                    .run(|| read.run(cancel))
                    .unwrap_or_else(|| Err(DltError::new(ErrorCode::Canceled, "the PDF read was canceled before it ran")));
                turn = lock(&self.link.turn);
                (self.shared.core_call(|ws| ws.finish_pdf_read(read, reading)), None)
            }
            Started::OpenPdf { id, open, listed } => {
                drop(turn);
                // Keep the queue wait plus a Word conversion within the default client request deadline.
                let deadline = received + Duration::from_secs(20);
                let converted = listed.run_before(Some(deadline), || open.run(cancel)).unwrap_or_else(|| {
                    let mut error = DltError::new(ErrorCode::TargetBusy, "PDF conversion waited too long to start");
                    error.retryable = true;
                    Err(error)
                });
                turn = lock(&self.link.turn);
                let result = self.shared.core_call(|ws| ws.finish_pdf_open(open, converted, cancel));
                let response = match result {
                    Ok(value) => RpcResponse::success(id, value),
                    Err(error) => RpcResponse::failure(id, error),
                };
                (response, None)
            }
        };
        // A committed cancel ends the attempt's process tree and its PDF reads now. The sweeper would end the tree within one tick.
        if method == "job.cancel" && response.error.is_none() {
            if let Some(job_id) = job_id {
                self.shared.jobs.kill_job(&job_id);
                self.shared.pdf_reads.cancel_job(&job_id);
            }
        }
        if let (Some((name, version)), true) = (named, response.error.is_none()) {
            let known = lock(&self.reported).clone();
            if let Some(mut record) = known {
                record.client_name = name.or(record.client_name);
                record.client_version = version.or(record.client_version);
                report(&self.shared, &self.reported, record);
            }
        }
        let message = IpcMessage::Response(response);
        let sent = match binary {
            Some(pixels) => self.link.send_after_binary(&message, &pixels.rgba_premultiplied),
            None => self.link.send(&message),
        }
        .is_ok();
        drop(turn);
        sent
    }
}

/// A core request after its first core call.
enum Started<'a> {
    Answered(RpcResponse),
    /// A PDF read to run without the core, already listed for cancels.
    ReadPdf(PdfRead, Listed<'a>),
    OpenPdf { id: String, open: PdfOpen, listed: Listed<'a> },
    /// A render to draw without the core.
    Render(Box<RenderJob>),
}

/// Counts one lane request down. It saturates: the lane's end sets the count to 0 while `CoreLane::submit` may still be taking
/// back the count of a request the ended lane did not take.
fn count_down(waiting: &AtomicUsize) {
    let _ = waiting.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| Some(n.saturating_sub(1)));
}

/// Tells the core about a connection: after its hello, and again when it learns the client's name.
fn report(shared: &Shared, reported: &Mutex<Option<ClientConnection>>, record: ClientConnection) {
    shared.core_call(|ws| ws.client_connected(record.clone()));
    *lock(reported) = Some(record);
}

/// Draws a render job outside the core. A job that draws nothing (its pixels are held, `RenderJob::draw_pixels` is None) takes no
/// raster permit, so it is answered while every permit is taken (68i). A job that draws takes permits by its size; the wait ends
/// when `stop` is true.
fn draw(job: &RenderJob, rasters: &RasterPermits, stop: impl Fn() -> bool) -> Result<RenderedPage, DltError> {
    // A request that does not fit its page is refused before any permit is waited for.
    let Some(pixels) = job.draw_pixels()? else {
        return job.run();
    };
    match rasters.acquire(permits_for(pixels), stop) {
        Some(_permit) => job.run(),
        None => Err(DltError::new(ErrorCode::Canceled, "the render was canceled before it ran: the connection or the host is closing")),
    }
}

struct Connection {
    shared: Arc<Shared>,
    link: Arc<Link>,
    decoder: designllm_bridge::frame::FrameDecoder,
    opened: std::time::Instant,
    last_frame: std::time::Instant,
    role: Option<Role>,
    binding: Option<JobRef>,
    /// Fixed by the hello for every role but gui (`identity`).
    identity: Option<Identity>,
    hello_result: Option<Value>,
    subscriber: Option<Arc<Subscriber>>,
    /// Identifies the connection to the core (`clients.list`).
    connection_id: String,
    /// What the core was last told about this connection: set by a hello, and by `job.set_client` after it.
    reported: Reported,
    /// Started by the first core request.
    lane: Option<CoreLane>,
    /// Set when the connection ends, for the PDF reads of its lane.
    cancel: PdfCancel,
}

impl Connection {
    fn new(shared: Arc<Shared>, pipe: Pipe) -> Self {
        let now = std::time::Instant::now();
        Connection {
            shared,
            link: Arc::new(Link { pipe, write: Mutex::new(()), turn: Mutex::new(()) }),
            decoder: designllm_bridge::frame::FrameDecoder::new(),
            opened: now,
            last_frame: now,
            role: None,
            binding: None,
            identity: None,
            hello_result: None,
            subscriber: None,
            connection_id: new_id(),
            reported: Arc::new(Mutex::new(None)),
            lane: None,
            cancel: PdfCancel::new(),
        }
    }

    fn run(&mut self) {
        let mut buffer = vec![0u8; 64 * 1024];
        loop {
            if !self.flush_events() {
                break;
            }
            // A full lane: the frames wait in the pipe until the lane has answered some.
            let read = if self.core_backlog() >= MAX_QUEUED_CORE_REQUESTS {
                thread::sleep(POLL);
                Ok(ReadOutcome::TimedOut)
            } else {
                self.link.pipe.read_some(&mut buffer, POLL)
            };
            match read {
                Ok(ReadOutcome::Data(0)) | Ok(ReadOutcome::TimedOut) => {}
                Ok(ReadOutcome::Data(n)) => {
                    self.last_frame = std::time::Instant::now();
                    let frames = match self.decoder.push(&buffer[..n]) {
                        Ok(frames) => frames,
                        Err(e) => {
                            // An oversized or malformed header cannot be resynchronised: answer once, then close.
                            let _ = self.write_response(protocol::response_err("", e));
                            break;
                        }
                    };
                    let mut close = false;
                    for frame in frames {
                        match self.handle_frame(&frame) {
                            Flow::Continue => {}
                            Flow::Close => {
                                close = true;
                                break;
                            }
                            Flow::Stop => {
                                self.shared.begin_shutdown();
                                close = true;
                                break;
                            }
                        }
                    }
                    if close {
                        break;
                    }
                }
                Ok(ReadOutcome::Eof) | Err(_) => break,
            }
            // An ended lane (a response could not be written, or a request panicked) answers nothing more: close as at EOF.
            if self.lane.as_ref().is_some_and(CoreLane::has_ended) {
                break;
            }
            if self.shared.is_stopping() {
                break;
            }
            if self.role.is_none() && self.opened.elapsed() > HELLO_DEADLINE {
                break;
            }
            // A connection is idle only while the host owes it no response.
            if self.core_backlog() == 0 && self.last_frame.elapsed() > self.shared.options.client_idle_timeout {
                break;
            }
        }
    }

    fn handle_frame(&mut self, payload: &[u8]) -> Flow {
        let value: Value = match serde_json::from_slice(payload) {
            Ok(value) => value,
            Err(e) => {
                let error = DltError::invalid(format!("invalid JSON frame: {e}"));
                let _ = self.write_response(protocol::response_err("", error));
                return Flow::Continue;
            }
        };
        let id = value.get("id").and_then(Value::as_str).unwrap_or("").to_string();
        if let Err(error) = protocol::check_version(&value) {
            let _ = self.write_response(protocol::response_err(&id, error));
            return Flow::Close;
        }
        if value.get("kind").and_then(Value::as_str) != Some("request") {
            let error = DltError::invalid("clients may only send requests");
            return self.reply(&id, error);
        }
        let request: RpcRequest = match serde_json::from_value(value) {
            Ok(request) => request,
            Err(e) => return self.reply(&id, DltError::invalid(format!("invalid request: {e}"))),
        };
        let (response, flow) = match self.route(request) {
            Routed::Answered(response, flow) => (response, flow),
            Routed::ToCore(request) => match self.submit_core(request) {
                None => return Flow::Continue,
                Some(response) => (response, Flow::Close),
            },
        };
        if self.write_response(response).is_err() {
            return Flow::Close;
        }
        flow
    }

    fn reply(&mut self, id: &str, error: DltError) -> Flow {
        match self.write_response(protocol::response_err(id, error)) {
            Ok(()) => Flow::Continue,
            Err(_) => Flow::Close,
        }
    }

    /// Queues a core request on the lane, which writes its response. Returns a response only when the request
    /// cannot be queued; the connection then closes.
    fn submit_core(&mut self, req: RpcRequest) -> Option<RpcResponse> {
        if self.lane.is_none() {
            let context = LaneContext {
                shared: Arc::clone(&self.shared),
                link: Arc::clone(&self.link),
                binding: self.binding.clone(),
                gui: self.role == Some(Role::Gui) && self.binding.is_none(),
                caller: self.identity.as_ref().map(|identity| (identity.prefix().to_string(), identity.fingerprint().to_string())),
                reported: Arc::clone(&self.reported),
                cancel: self.cancel.clone(),
            };
            match CoreLane::start(context) {
                Ok(lane) => self.lane = Some(lane),
                Err(e) => {
                    let error = protocol::error(ErrorCode::Io, format!("cannot start the core lane: {e}"), true);
                    return Some(protocol::response_err(&req.id, error));
                }
            }
        }
        let id = req.id.clone();
        match self.lane.as_ref().map(|lane| lane.submit(req)) {
            Some(true) => None,
            _ => Some(protocol::response_err(&id, protocol::error(ErrorCode::Io, "the connection is closing", true))),
        }
    }

    /// Core requests waiting on this connection's lane.
    fn core_backlog(&self) -> usize {
        self.lane.as_ref().map_or(0, CoreLane::waiting)
    }

    fn route(&mut self, req: RpcRequest) -> Routed {
        let id = req.id.clone();
        if req.method == METHOD_HELLO {
            let (response, flow) = self.hello(req);
            return Routed::Answered(response, flow);
        }
        let Some(role) = self.role else {
            let error = DltError::invalid("system.hello must be the first request on a connection");
            return Routed::Answered(protocol::response_err(&id, error), Flow::Continue);
        };
        if let Some(message) = gui_only_refusal(role, self.binding.is_some(), &req) {
            return Routed::Answered(self.forbidden(&id, &message), Flow::Continue);
        }
        match req.method.as_str() {
            METHOD_HEARTBEAT => Routed::Answered(protocol::response_ok(&id, self.heartbeat()), Flow::Continue),
            METHOD_STATUS => Routed::Answered(protocol::response_ok(&id, self.status()), Flow::Continue),
            METHOD_JOB_PROGRESS => {
                if role == Role::Bridge || self.binding.is_some() {
                    let message = "job progress is available to gui and cli connections";
                    return Routed::Answered(self.forbidden(&id, message), Flow::Continue);
                }
                Routed::Answered(self.job_progress(&id, &req.params), Flow::Continue)
            }
            METHOD_EVENTS_SUBSCRIBE => {
                if role == Role::Bridge || self.binding.is_some() {
                    let error = protocol::error(
                        ErrorCode::UnsupportedOperation,
                        "bridge connections poll events with events.poll",
                        false,
                    );
                    return Routed::Answered(protocol::response_err(&id, error), Flow::Continue);
                }
                let (response, flow) = self.subscribe(req);
                Routed::Answered(response, flow)
            }
            METHOD_STOP => {
                if role == Role::Bridge || self.binding.is_some() {
                    let message = "host.stop is not available to bridge connections";
                    return Routed::Answered(self.forbidden(&id, message), Flow::Continue);
                }
                let canceled = jobs::stop_all(&self.shared);
                self.shared.pdf_reads.cancel_all();
                let result = json!({ "stopped": true, "canceled_jobs": canceled });
                Routed::Answered(protocol::response_ok(&id, result), Flow::Stop)
            }
            _ => {
                let mut req = req;
                if let Some(identity) = &self.identity {
                    identity.pin(&req.method, &mut req.params);
                }
                Routed::ToCore(req)
            }
        }
    }

    fn forbidden(&self, id: &str, message: &str) -> RpcResponse {
        protocol::response_err(id, protocol::error(ErrorCode::InvalidArgument, message, false))
    }

    fn hello(&mut self, req: RpcRequest) -> (RpcResponse, Flow) {
        let id = req.id.clone();
        if self.role.is_some() {
            // The handshake is idempotent on its connection: a repeat returns the same result, so a caller that
            // already holds the handshake (for example a client asking for system.hello) is not refused.
            return match &self.hello_result {
                Some(cached) => (protocol::response_ok(&id, cached.clone()), Flow::Continue),
                None => {
                    let error = DltError::invalid("system.hello was already received on this connection");
                    (protocol::response_err(&id, error), Flow::Continue)
                }
            };
        }
        if req.protocol_version != PROTOCOL_VERSION {
            let error = protocol::mismatch(u64::from(req.protocol_version));
            return (protocol::response_err(&id, error), Flow::Close);
        }
        let role = match req.params.get("client_role").and_then(Value::as_str) {
            Some("gui") => Role::Gui,
            Some("bridge") => Role::Bridge,
            Some("cli") => Role::Cli,
            Some(other) => {
                let error = DltError::invalid(format!("client_role must be one of {CLIENT_ROLES:?}, got {other}"));
                return (protocol::response_err(&id, error), Flow::Close);
            }
            None => {
                let error = DltError::invalid("client_role is required");
                return (protocol::response_err(&id, error), Flow::Close);
            }
        };
        if self.shared.is_stopping() {
            let error = protocol::error(ErrorCode::HostNotRunning, "the host is stopping", true);
            return (protocol::response_err(&id, error), Flow::Close);
        }
        let binding = match req.params.get("job_ref") {
            None | Some(Value::Null) => None,
            Some(value) => {
                if role != Role::Bridge {
                    let error = DltError::invalid("job_ref is only accepted from bridge connections");
                    return (protocol::response_err(&id, error), Flow::Close);
                }
                let job = match parse_job_ref(value) {
                    Ok(job) => job,
                    Err(error) => return (protocol::response_err(&id, error), Flow::Close),
                };
                if let Err(error) = self.verify_binding(&job) {
                    return (protocol::response_err(&id, error), Flow::Close);
                }
                Some(job)
            }
        };
        if role == Role::Gui {
            if let Err(error) =
                gui_identity::verify_gui_client(&self.link.pipe, &self.shared.jobs, &self.shared.options.gui_executables)
            {
                return (protocol::response_err(&id, error), Flow::Close);
            }
        }
        let result = self.hello_result(role, binding.as_ref());
        self.role = Some(role);
        self.binding = binding;
        self.identity = match role {
            Role::Gui => None,
            Role::Bridge => Some(Identity::bridge(&self.connection_id, self.binding.as_ref(), self.link.pipe.client_process_start().ok())),
            Role::Cli => Some(Identity::cli()),
        };
        self.hello_result = Some(result.clone());
        let record = ClientConnection {
            connection_id: self.connection_id.clone(),
            role: role.name().to_string(),
            job: self.binding.clone(),
            client_name: clean_label(req.params.get("client_name")),
            client_version: clean_label(req.params.get("client_version")),
        };
        report(&self.shared, &self.reported, record);
        (protocol::response_ok(&id, result), Flow::Continue)
    }

    /// A bound connection must name the job's current attempt. A canceled attempt still binds: the core
    /// refuses its late writes with CANCELED, which is the rule under test.
    fn verify_binding(&self, job: &JobRef) -> Result<(), DltError> {
        let current = self.shared.core_call(|ws| ws.job(&job.job_id))?;
        if current.attempt_id != job.attempt_id {
            return Err(protocol::error(
                ErrorCode::JobNotActive,
                format!("attempt {} is not the current attempt of job {}", job.attempt_id, job.job_id),
                false,
            ));
        }
        Ok(())
    }

    fn hello_result(&self, role: Role, binding: Option<&JobRef>) -> Value {
        let (workspace_id, kinds) = self.shared.core_call(|ws| (ws.id().to_string(), ws.registered_kinds()));
        json!({
            "workspace_id": workspace_id,
            "protocol_version": PROTOCOL_VERSION,
            "schema_version": SCHEMA_VERSION,
            "registered_kinds": kinds,
            "host": {
                "pid": self.shared.pid,
                "mode": mode_name(self.shared.mode),
                "accepted_role": role.name(),
                "job_ref": binding.map(|j| protocol::job_ref_value(&j.job_id, &j.attempt_id)),
            },
        })
    }

    fn heartbeat(&self) -> Value {
        json!({
            "ok": true,
            "host_pid": self.shared.pid,
            "mode": mode_name(self.shared.mode),
            "clients": self.shared.clients(),
        })
    }

    fn status(&self) -> Value {
        let active = self.shared.core_call(|ws| ws.active_jobs()).ok();
        json!({
            "pid": self.shared.pid,
            "mode": mode_name(self.shared.mode),
            "pipe_name": self.shared.pipe_name,
            "workspace_root": self.shared.root.display().to_string(),
            "clients": self.shared.clients(),
            "stopping": self.shared.is_stopping(),
            "running_attempts": self.shared.jobs.running_count(),
            "active_jobs": active,
        })
    }

    fn job_progress(&self, id: &str, params: &Value) -> RpcResponse {
        let Some(job_id) = params.get("job_id").and_then(Value::as_str) else {
            return protocol::response_err(id, DltError::invalid("job_id is required"));
        };
        match self.shared.jobs.progress_of(job_id) {
            Some(lines) => protocol::response_ok(id, json!({ "job_id": job_id, "lines": lines })),
            None => protocol::response_err(id, protocol::error(ErrorCode::NotFound, "no progress for this job", false)),
        }
    }

    fn subscribe(&mut self, req: RpcRequest) -> (RpcResponse, Flow) {
        let after = req.params.get("after_seq").and_then(Value::as_u64).unwrap_or(0);
        let sub = Subscriber::new(after);
        // Registered before the backfill so an event pumped meanwhile is not lost.
        self.shared.register_subscriber(&sub);
        self.subscriber = Some(Arc::clone(&sub));
        self.shared.backfill(&sub);
        let result = json!({ "subscribed": true, "after_seq": after });
        (protocol::response_ok(&req.id, result), Flow::Continue)
    }

    /// Writes queued events for this connection. Returns false when the connection must close. While the lane holds
    /// the turn (a core request runs or its response is being written) the events wait for the next round.
    fn flush_events(&mut self) -> bool {
        let Some(sub) = self.subscriber.clone() else {
            return true;
        };
        let link = Arc::clone(&self.link);
        let _turn = match link.turn.try_lock() {
            Ok(turn) => turn,
            Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
            Err(TryLockError::WouldBlock) => return true,
        };
        for event in sub.drain() {
            if self.write_event(event).is_err() {
                return false;
            }
        }
        true
    }

    fn write_response(&self, response: RpcResponse) -> Result<(), DltError> {
        self.link.send(&IpcMessage::Response(response))
    }

    fn write_event(&self, event: CoreEvent) -> Result<(), DltError> {
        self.link.send(&IpcMessage::Event(event))
    }
}

/// Methods that only the desktop app's connection may call (docs/UI_CHANNEL.md). The core answers them to any
/// unbound connection, so the host decides by role.
const GUI_ONLY_METHODS: &[&str] = &["ui.state.publish", "ui.command.complete", "render.pixels"];

/// The refusal for a request only the gui role may make, or None when the request may proceed. `events.poll` is
/// open to every connection, except a poll of a view (`view_id` or `view_token` given).
fn gui_only_refusal(role: Role, bound: bool, req: &RpcRequest) -> Option<String> {
    let view_poll = req.method == "events.poll"
        && ["view_id", "view_token"].iter().any(|key| req.params.get(*key).is_some_and(|v| !v.is_null()));
    if !view_poll && !GUI_ONLY_METHODS.contains(&req.method.as_str()) {
        return None;
    }
    if role == Role::Gui && !bound {
        return None;
    }
    if view_poll {
        Some("events.poll with view_id or view_token is available only to the gui connection".to_string())
    } else {
        Some(format!("{} is available only to the gui connection", req.method))
    }
}

/// Why a request of a connection without the gui role is refused for opening a review gate, or None
/// (`designllm_bridge::review_gate`, 49w). Everything it reads is read under the same core lock as the dispatch that follows.
/// `changes.apply` and `changes.validate` are judged by their change set, `proposal.apply` by the proposal's stored change set,
/// `history.undo`, `history.redo` and `history.undo_change` by what they would restore, and a `ui.command.run` of `edit.undo`,
/// `edit.redo` or `ai.undo_change` by what the window's history request would restore. A proposal or a view that cannot be
/// read is left to the core.
/// An operation hidden from agents (`designllm_bridge::operation_exposure`) is refused the same way: in the change set of
/// `changes.*` and `proposal.create`, and in the stored change set of `proposal.apply`.
fn review_refusal(ws: &mut Workspace, req: &RpcRequest, binding: Option<&JobRef>) -> Option<DltError> {
    review_refusal_with(ws, req, binding, operation_exposure::is_hidden)
}

/// [`review_refusal`] with the hidden operation tags told by `hidden` (the tests pass their own).
fn review_refusal_with(ws: &mut Workspace, req: &RpcRequest, binding: Option<&JobRef>, hidden: fn(&str) -> bool) -> Option<DltError> {
    let history = |ws: &mut Workspace, method: &str, params: &Value, binding: Option<&JobRef>| match ws.history_preview(method, params, binding) {
        Ok(transitions) => review_gate::history_refusal(transitions),
        Err(error) => Some(error),
    };
    match req.method.as_str() {
        "proposal.apply" => {
            let proposal_id = req.params.get("proposal_id")?.as_str()?;
            let detail = ws.dispatch(core_request("proposal.get", json!({ "proposal_id": proposal_id })));
            let change_set = detail.result.as_ref()?.get("proposal")?.get("change_set")?;
            operation_exposure::change_set_refusal_with(change_set, hidden).or_else(|| review_gate::change_set_refusal(change_set))
        }
        "history.undo" | "history.redo" | "history.undo_change" => history(ws, &req.method, &req.params, binding),
        // A job-bound connection may not queue UI runs: the core refuses it without this reading the view.
        "ui.command.run" if binding.is_none() => {
            let view = ws.dispatch(core_request("ui.state.read", json!({ "view_id": req.params.get("view_id") })));
            let view: UiStateView = serde_json::from_value(view.result?).ok()?;
            let (method, params) = review_gate::ui_run_history_call(&req.params, &view.state, ws.id())?;
            // The window sends its history request on its own connection, which no job binds.
            history(ws, method, &params, None)
        }
        _ => operation_exposure::refusal_with(&req.method, &req.params, hidden)
            .or_else(|| review_gate::agent_refusal(&req.method, &req.params)),
    }
}

/// A core request the host itself makes.
fn core_request(method: &str, params: Value) -> RpcRequest {
    RpcRequest { protocol_version: PROTOCOL_VERSION, id: new_id(), method: method.into(), params }
}

/// A label the host reports for a connection (a client name or version): trimmed, 1 to 128 characters, no control
/// characters. Anything else is not reported.
fn clean_label(value: Option<&Value>) -> Option<String> {
    let text = value?.as_str()?.trim();
    let usable = !text.is_empty() && text.chars().count() <= 128 && !text.chars().any(char::is_control);
    usable.then(|| text.to_string())
}

fn parse_job_ref(value: &Value) -> Result<JobRef, DltError> {
    let job_id = value.get("job_id").and_then(Value::as_str);
    let attempt_id = value.get("attempt_id").and_then(Value::as_str);
    match (job_id, attempt_id) {
        (Some(job_id), Some(attempt_id)) if !job_id.is_empty() && !attempt_id.is_empty() => Ok(JobRef {
            job_id: job_id.to_string(),
            attempt_id: attempt_id.to_string(),
        }),
        _ => Err(DltError::invalid("job_ref needs non-empty job_id and attempt_id")),
    }
}

fn mode_name(mode: HostMode) -> &'static str {
    match mode {
        HostMode::Headless => "headless",
        HostMode::Managed => "managed",
    }
}

#[cfg(test)]
#[path = "conn_exposure_tests.rs"]
mod exposure_tests;

#[cfg(test)]
#[path = "conn_draw_tests.rs"]
mod draw_tests;
