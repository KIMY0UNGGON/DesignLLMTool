//! Attach-only protocol client.
//!
//! `Client` never starts a host. A call connects to the workspace's named pipe; if no instance exists
//! the call fails with HOST_NOT_RUNNING. After the first successful connection the client keeps one
//! persistent connection with a reader thread (responses are matched by `id`, events are delivered to
//! the receiver from `take_events`) and a heartbeat thread, so the host can count it as a live client.
//!
//! Failure rules:
//! - A request that could not be written is retried once on a fresh connection.
//! - A request that was written but got no response (connection lost, deadline) is NOT retried. The
//!   error is HOST_NOT_RUNNING with `retryable = true` for a lost connection. The caller must repeat the
//!   same `request_id`, which the core answers from its receipt, so a duplicate is not applied twice.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, PoisonError};
use std::thread;
use std::time::Duration;

use designllm_contracts::{CoreEvent, DltError, DltResult, ErrorCode, IpcMessage, JobRef, RpcResponse};
use serde_json::{json, Value};

use crate::frame::{encode_frame, Frame, FrameDecoder};
use crate::pipe::{self, normalize_root, ConnectError, Pipe, ReadOutcome};
use crate::protocol::{self, METHOD_EVENTS_SUBSCRIBE, METHOD_HEARTBEAT, METHOD_HELLO};

/// Default deadline for one request/response exchange.
pub const DEFAULT_CALL_TIMEOUT: Duration = Duration::from_secs(120);
/// Interval between heartbeats on a live connection.
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);

const READ_SLICE: Duration = Duration::from_millis(250);
const WRITE_DEADLINE: Duration = Duration::from_secs(10);
const HANDSHAKE_DEADLINE: Duration = Duration::from_secs(10);
const HEARTBEAT_DEADLINE: Duration = Duration::from_secs(10);
// Total wait for a busy host (connect_client waits up to this in all). A missing host fails at once with
// ERROR_FILE_NOT_FOUND, so this applies only while every instance is busy. 6 s matches the old worst case (3 tries x 2 s).
const CONNECT_WAIT: Duration = Duration::from_millis(6000);
/// The binary frames of one response (`render.pixels`) may hold at most this many bytes, as the core answers; more closes the
/// connection.
const MAX_ATTACHMENT_BYTES: usize = designllm_contracts::MAX_RASTER_PIXEL_BYTES;

/// A response and the binary frames that came right before it (68i).
type Reply = (RpcResponse, Vec<Vec<u8>>);

/// Who is connecting. The host applies different lifetime and method rules per role.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientRole {
    /// The desktop GUI. Counts as a client for hidden-host lifetime.
    Gui,
    /// An MCP bridge (external agent or job-bound runtime).
    Bridge,
    /// A command-line caller.
    Cli,
}

impl ClientRole {
    /// Wire value in `system.hello.client_role`.
    pub fn wire_name(self) -> &'static str {
        match self {
            ClientRole::Gui => "gui",
            ClientRole::Bridge => "bridge",
            ClientRole::Cli => "cli",
        }
    }
}

/// Client configuration.
#[derive(Debug, Clone)]
pub struct ClientOptions {
    pub role: ClientRole,
    /// Job binding. Only bridges started for an app job set it; the host verifies it.
    pub job: Option<JobRef>,
    /// Deadline for one request.
    pub call_timeout: Duration,
}

impl ClientOptions {
    pub fn for_role(role: ClientRole) -> Self {
        ClientOptions { role, job: None, call_timeout: DEFAULT_CALL_TIMEOUT }
    }
}

/// Handle to one workspace's host. Cheap to clone; all clones share one connection. Safe to call from
/// several threads. Do not call it on a UI thread for long operations.
#[derive(Clone)]
pub struct Client {
    inner: Arc<ClientInner>,
}

#[derive(Default)]
struct EventHub {
    sender: Mutex<Option<Sender<CoreEvent>>>,
    last_seq: std::sync::atomic::AtomicU64,
}

/// The MCP client's name and version, sent in `system.hello` of every connection this client opens.
#[derive(Clone)]
struct ClientIdentity {
    name: String,
    version: Option<String>,
}

struct ClientInner {
    root: PathBuf,
    options: ClientOptions,
    pipe_name: Result<String, DltError>,
    connection: Mutex<Option<Arc<Connection>>>,
    events: Arc<EventHub>,
    subscribe: AtomicBool,
    identity: Mutex<Option<ClientIdentity>>,
}

impl Drop for ClientInner {
    fn drop(&mut self) {
        if let Some(conn) = lock(&self.connection).take() {
            conn.shutdown();
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn not_running() -> DltError {
    protocol::error(
        ErrorCode::HostNotRunning,
        "no DesignLLMTool host is serving this workspace; start one with --host or --headless",
        true,
    )
}

fn connection_lost(detail: &str) -> DltError {
    protocol::error(
        ErrorCode::HostNotRunning,
        format!(
            "connection to the host was lost ({detail}). A request that was already sent may have been applied; \
             retry it with the same request_id"
        ),
        true,
    )
}

/// Why one exchange failed. The distinction decides whether a retry on a new connection is safe.
enum Failure {
    /// The request never reached the pipe.
    NotSent(DltError),
    /// The request may have reached the host. The caller must not blindly resend it.
    MaybeSent(DltError),
    /// The host answered with an error envelope.
    Remote(DltError),
}

impl Client {
    /// Client for `workspace` in the command-line and agent role (`cli`). Makes no connection until the first call.
    pub fn new(workspace: &Path) -> Self {
        Self::with_options(workspace, ClientOptions::for_role(ClientRole::Cli))
    }

    /// The desktop window's client (role `gui`). The host accepts this role only from the application executable that
    /// does not run inside a job. Any other process is refused at `system.hello`.
    pub fn gui(workspace: &Path) -> Self {
        Self::with_options(workspace, ClientOptions::for_role(ClientRole::Gui))
    }

    /// Client with an explicit role (`cli`, `bridge`, or `gui`).
    pub fn with_role(workspace: &Path, role: ClientRole) -> Self {
        Self::with_options(workspace, ClientOptions::for_role(role))
    }

    /// Bridge bound to an app job. Requests run as that job's attempt, scoped by the host.
    pub fn with_job(workspace: &Path, job: JobRef) -> Self {
        Self::with_options(
            workspace,
            ClientOptions { role: ClientRole::Bridge, job: Some(job), call_timeout: DEFAULT_CALL_TIMEOUT },
        )
    }

    pub fn with_options(workspace: &Path, options: ClientOptions) -> Self {
        let root = normalize_root(workspace);
        let pipe_name = pipe::pipe_name_for(&root);
        Client {
            inner: Arc::new(ClientInner {
                root,
                options,
                pipe_name,
                connection: Mutex::new(None),
                events: Arc::new(EventHub::default()),
                subscribe: AtomicBool::new(false),
                identity: Mutex::new(None),
            }),
        }
    }

    /// Names the MCP client for the connections this client opens from now on: `system.hello` carries the name, and
    /// the host lists the connection under it. An empty name is ignored.
    pub fn announce(&self, name: &str, version: Option<&str>) {
        let name = name.trim();
        if name.is_empty() {
            return;
        }
        *lock(&self.inner.identity) = Some(ClientIdentity { name: name.to_string(), version: version.map(str::to_string) });
    }

    /// Normalized workspace root used for the pipe name.
    pub fn workspace_root(&self) -> &Path {
        &self.inner.root
    }

    /// Pipe name this client connects to.
    pub fn pipe_name(&self) -> DltResult<String> {
        self.inner.pipe_name.clone()
    }

    /// True while a live connection exists (it may still be closed by the host).
    pub fn is_connected(&self) -> bool {
        lock(&self.inner.connection).as_ref().is_some_and(|c| c.is_alive())
    }

    /// Connects if needed and returns the `system.hello` result (workspace id, versions, host mode, pid).
    pub fn hello(&self) -> DltResult<Value> {
        let conn = self.inner.connection()?;
        Ok(conn.hello())
    }

    /// Sends one request and returns its `result`.
    ///
    /// Errors are `DltError`s from the host or transport. Attach-only: no host is started.
    pub fn call(&self, method: &str, params: Value) -> DltResult<Value> {
        if method == METHOD_HELLO {
            // The handshake already happened on this connection; answer with its result (no second hello).
            return self.hello();
        }
        self.call_binary(method, params).map(|(value, _)| value)
    }

    /// [`Client::call`] that also returns the payloads of the binary frames the host sent right before the response, in
    /// order (68i: the raw pixels of `render.pixels`; empty for every other method).
    pub fn call_binary(&self, method: &str, params: Value) -> DltResult<(Value, Vec<Vec<u8>>)> {
        let timeout = self.inner.options.call_timeout;
        let mut retried = false;
        loop {
            let conn = self.inner.connection()?;
            match conn.request(method, params.clone(), timeout) {
                Ok(reply) => return Ok(reply),
                Err(Failure::NotSent(_)) if !retried => {
                    retried = true;
                }
                Err(Failure::NotSent(e)) | Err(Failure::MaybeSent(e)) | Err(Failure::Remote(e)) => return Err(e),
            }
        }
    }

    /// Returns the receiver for pushed events. The first call subscribes this client on connect; later
    /// calls return `None`. Events arrive in `event_seq` order. After a gap or a host restart, the host
    /// sends an event named `resync_required`; re-read the context in that case.
    pub fn take_events(&self) -> Option<Receiver<CoreEvent>> {
        let mut slot = lock(&self.inner.events.sender);
        if slot.is_some() {
            return None;
        }
        let (tx, rx) = mpsc::channel();
        *slot = Some(tx);
        self.inner.subscribe.store(true, Ordering::SeqCst);
        Some(rx)
    }

    /// Closes the current connection. The next call reconnects.
    pub fn close(&self) {
        if let Some(conn) = lock(&self.inner.connection).take() {
            conn.shutdown();
        }
    }
}

impl ClientInner {
    fn connection(&self) -> DltResult<Arc<Connection>> {
        let name = self.pipe_name.clone()?;
        let mut slot = lock(&self.connection);
        if let Some(conn) = slot.as_ref() {
            if conn.is_alive() {
                return Ok(Arc::clone(conn));
            }
        }
        *slot = None;
        let identity = lock(&self.identity).clone();
        let conn = Connection::establish(
            &name,
            &self.options,
            Arc::clone(&self.events),
            self.subscribe.load(Ordering::SeqCst),
            identity,
        )?;
        *slot = Some(Arc::clone(&conn));
        Ok(conn)
    }
}

/// One live pipe connection plus its reader and heartbeat threads.
struct Connection {
    pipe: Pipe,
    writer: Mutex<()>,
    pending: Mutex<HashMap<String, Sender<Reply>>>,
    alive: AtomicBool,
    stop: (Mutex<bool>, Condvar),
    hello: OnceLock<Value>,
}

impl Connection {
    fn establish(
        pipe_name: &str,
        options: &ClientOptions,
        events: Arc<EventHub>,
        subscribe: bool,
        identity: Option<ClientIdentity>,
    ) -> DltResult<Arc<Connection>> {
        let pipe = match pipe::connect_client(pipe_name, CONNECT_WAIT) {
            Ok(pipe) => pipe,
            Err(ConnectError::NotRunning) => return Err(not_running()),
            Err(ConnectError::Other(e)) => {
                return Err(protocol::error(ErrorCode::Io, format!("cannot connect to the host pipe: {e}"), true))
            }
        };
        let conn = Arc::new(Connection {
            pipe,
            writer: Mutex::new(()),
            pending: Mutex::new(HashMap::new()),
            alive: AtomicBool::new(true),
            stop: (Mutex::new(false), Condvar::new()),
            hello: OnceLock::new(),
        });
        conn.spawn_threads(Arc::clone(&events))?;

        let mut params = json!({
            "client_role": options.role.wire_name(),
            "client_version": identity
                .as_ref()
                .and_then(|known| known.version.clone())
                .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string()),
        });
        if let Some(known) = &identity {
            params["client_name"] = json!(known.name);
        }
        if let Some(job) = &options.job {
            params["job_ref"] = protocol::job_ref_value(&job.job_id, &job.attempt_id);
        }
        match conn.request(METHOD_HELLO, params, HANDSHAKE_DEADLINE) {
            Ok((hello, _)) => {
                let _ = conn.hello.set(hello);
            }
            Err(f) => {
                conn.shutdown();
                return Err(failure_error(f));
            }
        }
        if subscribe {
            let after = events.last_seq.load(Ordering::SeqCst);
            if let Err(f) = conn.request(METHOD_EVENTS_SUBSCRIBE, json!({ "after_seq": after }), HANDSHAKE_DEADLINE) {
                conn.shutdown();
                return Err(failure_error(f));
            }
        }
        Ok(conn)
    }

    fn spawn_threads(self: &Arc<Self>, events: Arc<EventHub>) -> DltResult<()> {
        let reader = Arc::clone(self);
        thread::Builder::new()
            .name("designllm-client-reader".into())
            .spawn(move || reader_loop(reader, events))
            .map_err(|e| DltError::new(ErrorCode::Io, format!("cannot start reader thread: {e}")))?;
        let beat = Arc::clone(self);
        thread::Builder::new()
            .name("designllm-client-heartbeat".into())
            .spawn(move || heartbeat_loop(beat, HEARTBEAT_INTERVAL))
            .map_err(|e| DltError::new(ErrorCode::Io, format!("cannot start heartbeat thread: {e}")))?;
        Ok(())
    }

    fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    fn hello(&self) -> Value {
        self.hello.get().cloned().unwrap_or(Value::Null)
    }

    fn shutdown(&self) {
        self.alive.store(false, Ordering::SeqCst);
        {
            let (flag, cvar) = &self.stop;
            *lock(flag) = true;
            cvar.notify_all();
        }
        lock(&self.pending).clear();
        self.pipe.cancel_pending();
    }

    /// One request/response exchange. Writes are serialized; the response is matched by id.
    fn request(&self, method: &str, params: Value, deadline: Duration) -> Result<(Value, Vec<Vec<u8>>), Failure> {
        if !self.is_alive() {
            return Err(Failure::NotSent(connection_lost("connection already closed")));
        }
        let req = protocol::request(method, params);
        let id = req.id.clone();
        let (tx, rx) = mpsc::channel();
        lock(&self.pending).insert(id.clone(), tx);
        if !self.is_alive() {
            lock(&self.pending).remove(&id);
            return Err(Failure::NotSent(connection_lost("connection closed before sending")));
        }
        let frame = protocol::encode(&IpcMessage::Request(req))
            .and_then(|payload| encode_frame(&payload))
            .map_err(|e| {
                lock(&self.pending).remove(&id);
                Failure::NotSent(e)
            })?;
        let written = {
            let _guard = lock(&self.writer);
            self.pipe.write_all(&frame, WRITE_DEADLINE)
        };
        if let Err(e) = written {
            lock(&self.pending).remove(&id);
            self.shutdown();
            return Err(Failure::MaybeSent(connection_lost(&format!("write failed: {e}"))));
        }
        match rx.recv_timeout(deadline) {
            Ok((resp, binary)) => match resp.error {
                Some(error) => Err(Failure::Remote(error)),
                None => Ok((resp.result.unwrap_or(Value::Null), binary)),
            },
            Err(RecvTimeoutError::Timeout) => {
                lock(&self.pending).remove(&id);
                Err(Failure::MaybeSent(protocol::error(
                    ErrorCode::Io,
                    format!("the host did not answer {method} within {} s; the request may still be running", deadline.as_secs()),
                    true,
                )))
            }
            Err(RecvTimeoutError::Disconnected) => {
                Err(Failure::MaybeSent(connection_lost("the reader stopped before the response")))
            }
        }
    }
}

fn failure_error(f: Failure) -> DltError {
    match f {
        Failure::NotSent(e) | Failure::MaybeSent(e) | Failure::Remote(e) => e,
    }
}

fn deliver_event(events: &EventHub, event: CoreEvent) {
    events.last_seq.fetch_max(event.event_seq, Ordering::SeqCst);
    if let Some(tx) = lock(&events.sender).as_ref() {
        let _ = tx.send(event);
    }
}

/// A response from a peer with another protocol version cannot be decoded, but it still answers the request this
/// client sent. The waiting call fails now with that PROTOCOL_MISMATCH, instead of after its deadline.
fn fail_mismatched_response(conn: &Connection, payload: &[u8], error: DltError) {
    if error.code != ErrorCode::ProtocolMismatch {
        return;
    }
    let Ok(value) = serde_json::from_slice::<Value>(payload) else {
        return;
    };
    if value.get("kind").and_then(Value::as_str) != Some("response") {
        return;
    }
    let Some(id) = value.get("id").and_then(Value::as_str) else {
        return;
    };
    if let Some(tx) = lock(&conn.pending).remove(id) {
        let _ = tx.send((protocol::response_err(id, error), Vec::new()));
    }
}

fn reader_loop(conn: Arc<Connection>, events: Arc<EventHub>) {
    let mut decoder = FrameDecoder::with_binary();
    let mut buffer = vec![0u8; 64 * 1024];
    // Binary frames go with the next response: the host writes them and that response under one hold of its write lock (68i).
    let mut binary: Vec<Vec<u8>> = Vec::new();
    let mut binary_bytes = 0usize;
    'read: while conn.is_alive() {
        match conn.pipe.read_some(&mut buffer, READ_SLICE) {
            Ok(ReadOutcome::Data(0)) | Ok(ReadOutcome::TimedOut) => continue,
            Ok(ReadOutcome::Data(n)) => {
                let frames = match decoder.push_frames(&buffer[..n]) {
                    Ok(frames) => frames,
                    Err(_) => break,
                };
                for frame in frames {
                    let payload = match frame {
                        Frame::Binary(payload) => {
                            binary_bytes += payload.len();
                            if binary_bytes > MAX_ATTACHMENT_BYTES {
                                break 'read;
                            }
                            binary.push(payload);
                            continue;
                        }
                        Frame::Json(payload) => payload,
                    };
                    match protocol::decode(&payload) {
                        Ok(IpcMessage::Response(resp)) => {
                            binary_bytes = 0;
                            let attached = std::mem::take(&mut binary);
                            let waiter = lock(&conn.pending).remove(&resp.id);
                            if let Some(tx) = waiter {
                                let _ = tx.send((resp, attached));
                            }
                        }
                        Ok(IpcMessage::Event(event)) => deliver_event(&events, event),
                        Ok(IpcMessage::Request(_)) => {}
                        Err(error) => fail_mismatched_response(&conn, &payload, error),
                    }
                }
            }
            Ok(ReadOutcome::Eof) | Err(_) => break,
        }
    }
    conn.shutdown();
}

fn heartbeat_loop(conn: Arc<Connection>, interval: Duration) {
    loop {
        {
            let (flag, cvar) = &conn.stop;
            let guard = lock(flag);
            let (guard, _) = cvar
                .wait_timeout_while(guard, interval, |stopped| !*stopped)
                .unwrap_or_else(PoisonError::into_inner);
            if *guard {
                return;
            }
        }
        if !conn.is_alive() {
            return;
        }
        if conn.request(METHOD_HEARTBEAT, json!({}), HEARTBEAT_DEADLINE).is_err() {
            conn.shutdown();
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roles_map_to_wire_names() {
        assert_eq!(ClientRole::Gui.wire_name(), "gui");
        assert_eq!(ClientRole::Bridge.wire_name(), "bridge");
        assert_eq!(ClientRole::Cli.wire_name(), "cli");
    }

    #[test]
    fn new_is_the_cli_role_and_only_gui_asks_for_the_gui_role() {
        let dir = std::env::temp_dir().join(format!("dllm-client-role-{}", designllm_contracts::new_id()));
        assert_eq!(Client::new(&dir).inner.options.role.wire_name(), "cli");
        assert_eq!(Client::gui(&dir).inner.options.role.wire_name(), "gui");
    }

    #[test]
    fn workspace_with_no_host_is_host_not_running_and_creates_no_pipe() {
        let dir = std::env::temp_dir().join(format!("dllm-client-test-{}", designllm_contracts::new_id()));
        let client = Client::with_role(&dir, ClientRole::Cli);
        let err = client.call("workspace.list", json!({})).expect_err("no host");
        assert_eq!(err.code, ErrorCode::HostNotRunning);
        assert!(err.retryable);
        assert!(!client.is_connected());
    }

    #[test]
    fn take_events_hands_out_one_receiver() {
        let client = Client::new(&std::env::temp_dir());
        assert!(client.take_events().is_some());
        assert!(client.take_events().is_none());
    }
}
