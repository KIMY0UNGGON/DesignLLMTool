//! MCP server over stdio (JSON-RPC 2.0, one JSON message per line).
//!
//! Lifecycle (2025-11-25 §Lifecycle): `initialize` → response → `notifications/initialized`; tools are
//! refused before the initialized notification. Version negotiation: a supported requested version is
//! echoed back, otherwise the latest supported version is returned and the client decides whether to
//! continue. Batches are not part of protocol 2025-11-25 and are refused.
//!
//! Stdout carries MCP messages only; diagnostics belong on stderr. The bridge keeps no state across
//! processes: every tool call goes to the host through `Client`, which never starts a host.

use std::io::{self, BufRead, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use std::collections::{BTreeMap, BTreeSet};

use designllm_contracts::{ArtifactKind, DltError, DltResult, ErrorCode, JobRef};
use serde_json::{json, Value};

use crate::assets::{base64_encode, fetch_asset};
use crate::catalog::{find, tools_for, validate_arguments, Post, Scope, ToolSpec};
use crate::client::{Client, ClientRole};
use crate::manual;
use crate::update_notice::UpdateSlot;

/// Latest MCP revision this server implements.
pub const LATEST_PROTOCOL_VERSION: &str = "2025-11-25";
/// Revisions accepted in `initialize`. Both carry `structuredContent` and tool annotations.
pub const SUPPORTED_PROTOCOL_VERSIONS: &[&str] = &["2025-11-25", "2025-06-18"];
/// Server name reported in `serverInfo`.
pub const SERVER_NAME: &str = "designllm";

const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
/// MCP: resources/read for a URI that no resource has.
const RESOURCE_NOT_FOUND: i64 = -32002;

/// Largest image returned inline as MCP image content.
const INLINE_IMAGE_LIMIT: u64 = 8 * 1024 * 1024;
/// Largest text asset returned inline.
const INLINE_TEXT_LIMIT: u64 = 1024 * 1024;
/// Structured results above this size are summarized in the text block (the structured content is still complete).
const TEXT_BLOCK_LIMIT: usize = 256 * 1024;
/// Tool calls running at the same time.
const MAX_CONCURRENT_CALLS: usize = 8;
/// `ui.screenshot`: the wait when the caller gives no timeout, the longest allowed wait, the extra time the bridge
/// allows beyond the caller's timeout, and the interval between reads of the run.
const SCREENSHOT_DEFAULT_TIMEOUT_MS: u64 = 15_000;
const SCREENSHOT_MAX_TIMEOUT_MS: u64 = 120_000;
const SCREENSHOT_GRACE_MS: u64 = 2_000;
const SCREENSHOT_POLL: Duration = Duration::from_millis(100);

/// Where tool calls go. `Client` implements it; tests implement it in memory.
pub trait ToolBackend: Send + Sync {
    /// Sends one core method and returns its `result`.
    fn call(&self, method: &str, params: Value) -> DltResult<Value>;
    /// The MCP client's `clientInfo` (name and version), before the first call. A client that connects to the host
    /// sends it in `system.hello`. Default: nothing.
    fn announce_client(&self, _name: &str, _version: Option<&str>) {}
}

/// Backend that forwards to the host through a [`Client`].
pub struct ClientBackend {
    client: Client,
}

struct Startup {
    start: Box<dyn Fn(bool) -> DltResult<()> + Send + Sync>,
    gate: Mutex<()>,
}

impl ClientBackend {
    pub fn new(client: Client) -> Self {
        ClientBackend { client }
    }
}

impl ToolBackend for ClientBackend {
    fn call(&self, method: &str, params: Value) -> DltResult<Value> {
        self.client.call(method, params)
    }

    fn announce_client(&self, name: &str, version: Option<&str>) {
        self.client.announce(name, version);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Created,
    Initialized,
    Ready,
}

/// A tool call accepted by the router and waiting to run.
#[derive(Debug, Clone)]
pub struct PendingCall {
    pub id: Value,
    pub tool: ToolSpec,
    pub arguments: Value,
    background: bool,
}

/// What the transport must do with one routed message.
#[derive(Debug)]
pub enum Routed {
    /// Write this JSON-RPC message.
    Reply(Value),
    /// Run the tool on a worker thread, then reply.
    Call(PendingCall),
    /// No reply (notification or ignored message).
    Nothing,
}

struct Permits {
    used: Mutex<usize>,
    freed: Condvar,
}

impl Permits {
    fn acquire(&self) {
        let mut used = lock(&self.used);
        while *used >= MAX_CONCURRENT_CALLS {
            used = self.freed.wait(used).unwrap_or_else(PoisonError::into_inner);
        }
        *used += 1;
    }

    fn release(&self) {
        *lock(&self.used) -= 1;
        self.freed.notify_one();
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Error code of a change refused because this connection has not yet received the artifact's design guide.
pub const GUIDE_REQUIRED: &str = "GUIDE_REQUIRED";
/// Methods that change an artifact's design or content: the first one per guide is held until the guide is delivered.
/// Undo and redo restore earlier states and are not held; GUI commands act on what a person sees and are not held either.
const GUIDED_METHODS: &[&str] = &[
    "changes.validate", "changes.apply", "proposal.create", "proposal.apply", "design_themes.apply", "figure.dataset.edit",
    "design_system.set", "design_system.publish",
];
/// Methods whose answer introduces an artifact to the agent: their result carries the artifact's guide once.
const INTRODUCING_METHODS: &[&str] = &["artifact.create", "artifact.open", "artifact.inspect", "context.read", "artifact.validate"];
/// Design-quality warnings attached after an apply, at most this many.
const QUALITY_NOTE_LIMIT: usize = 20;

/// Which guides this connection has delivered, which are on their way to the client, and what it knows of each artifact.
#[derive(Default)]
struct GuideState {
    /// Guides whose response reached the client (or an in-process caller).
    delivered: BTreeSet<&'static str>,
    /// Guides put into a response that has not been written yet: changes for them are held until it is.
    pending: BTreeSet<&'static str>,
    artifacts: BTreeMap<String, (ArtifactKind, String)>,
}

/// What a guide check found for one artifact.
enum Claim {
    /// The client has the guide.
    Delivered,
    /// Another response of this connection carries it and has not been written yet.
    Pending,
    /// This call delivers it: the text goes into its response.
    Claimed(&'static str, String),
}


/// Records the id, kind and title of one artifact summary.
fn learn_summary(guides: &mut GuideState, item: &Value) {
    let id = item.get("id").or_else(|| item.get("artifact_id")).and_then(Value::as_str);
    let kind = item.get("kind").cloned().and_then(|kind| serde_json::from_value::<ArtifactKind>(kind).ok());
    if let (Some(id), Some(kind)) = (id, kind) {
        let title = item.get("title").and_then(Value::as_str).unwrap_or_default().to_string();
        guides.artifacts.insert(id.to_string(), (kind, title));
    }
}

/// An optional string argument of a tool the bridge answers itself: absent or null is None, any other type is refused.
fn text_argument<'a>(arguments: &'a Value, name: &str) -> DltResult<Option<&'a str>> {
    match arguments.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.as_str())),
        Some(_) => Err(DltError::invalid(format!("{name} must be a string"))),
    }
}

/// A failed lookup behind the guide gate: the change is refused with the backend's own error (code, retryability, ids and
/// diagnostics kept), its message saying that nothing was applied.
fn lookup_failed(mut error: DltError) -> DltError {
    error.message = format!("the design guide for this change could not be checked, so nothing was applied: {}", error.message);
    error
}

/// Protocol state for one stdio session.
pub struct McpServer {
    backend: Arc<dyn ToolBackend>,
    tools: Vec<ToolSpec>,
    phase: Mutex<Phase>,
    permits: Permits,
    /// The connection of an app job: `initialize` reports the MCP client to the job (`job.set_client`).
    job_bound: bool,
    startup: Option<Startup>,
    /// Whether this connection must deliver an artifact's design guide before its first change (workspace clients). A job's
    /// connection does not: the job's prompt already carries the guide (`manual::job_design_guidance`).
    guide_gate: bool,
    guides: Mutex<GuideState>,
    /// A newer release found by the background update check (`--mcp` workspace connections only).
    update: Option<UpdateSlot>,
    /// Whether this connection has told the agent about it: once per connection.
    update_told: AtomicBool,
}

impl McpServer {
    /// Server exposing the tools of `scope`. A job-bound scope makes this the job's own connection.
    pub fn new(backend: Arc<dyn ToolBackend>, scope: Scope) -> Self {
        let mut server = Self::with_tools(backend, tools_for(scope));
        server.job_bound = scope == Scope::JobBound;
        server.guide_gate = scope == Scope::Workspace;
        server
    }

    /// Server exposing exactly `tools`.
    pub fn with_tools(backend: Arc<dyn ToolBackend>, tools: Vec<ToolSpec>) -> Self {
        McpServer {
            backend,
            tools,
            phase: Mutex::new(Phase::Created),
            permits: Permits { used: Mutex::new(0), freed: Condvar::new() },
            job_bound: false,
            startup: None,
            guide_gate: false,
            guides: Mutex::new(GuideState::default()),
            update: None,
            update_told: AtomicBool::new(false),
        }
    }

    /// The same server without the guide gate, for in-process callers that test engine behaviour rather than agent guidance.
    pub fn without_guide_gate(mut self) -> Self {
        self.guide_gate = false;
        self
    }

    /// The same server, telling the agent once about a newer release when `slot` holds one.
    pub fn with_update_notice(mut self, slot: UpdateSlot) -> Self {
        self.update = Some(slot);
        self
    }

    /// Adds the update notice to the first successful tool result that completes once the slot is filled: a text block, and
    /// `update_available` in the structured content. Error results never carry it, so it does not mix with a refusal.
    fn attach_update_notice(&self, result: &mut Value) {
        let Some(info) = self.update.as_ref().and_then(|slot| slot.get()) else { return };
        if result.get("isError") == Some(&Value::Bool(true)) {
            return;
        }
        let Some(content) = result.get_mut("content").and_then(Value::as_array_mut) else { return };
        if self.update_told.swap(true, Ordering::SeqCst) {
            return;
        }
        content.push(json!({ "type": "text", "text": info.notice_text() }));
        if let Some(structured) = result.get_mut("structuredContent").and_then(Value::as_object_mut) {
            structured.insert("update_available".to_string(), info.structured());
        }
    }

    /// Kind and title of `artifact_id`: from what this connection has seen, else from `workspace.list`. `Ok(None)` when the
    /// workspace has no such artifact; a failed listing is an error, so a change is never let through on a guess.
    fn artifact_info(&self, artifact_id: &str) -> DltResult<Option<(ArtifactKind, String)>> {
        if let Some(found) = lock(&self.guides).artifacts.get(artifact_id).cloned() {
            return Ok(Some(found));
        }
        let listing = self.backend.call("workspace.list", json!({}))?;
        self.learn_artifacts(&listing);
        Ok(lock(&self.guides).artifacts.get(artifact_id).cloned())
    }

    /// Records the id, kind and title of every artifact summary in a `workspace.list` answer.
    fn learn_artifacts(&self, listing: &Value) {
        let Some(items) = listing.get("artifacts").and_then(Value::as_array) else { return };
        let mut guides = lock(&self.guides);
        for item in items {
            learn_summary(&mut guides, item);
        }
    }

    /// The guide state of an artifact for this connection. The key is checked first, so a delivered guide costs no text.
    /// `force` claims the guide again even when it was delivered (an explicit `design_guide` request).
    fn claim(&self, kind: ArtifactKind, title: &str, force: bool) -> Claim {
        let key = manual::guide_key(kind, title);
        {
            let mut guides = lock(&self.guides);
            if !force {
                if guides.delivered.contains(key) {
                    return Claim::Delivered;
                }
                if guides.pending.contains(key) {
                    return Claim::Pending;
                }
            }
            guides.pending.insert(key);
        }
        Claim::Claimed(key, manual::guide_for(kind, title).text)
    }

    /// Marks the guides a response carried as delivered, once that response is written. Called by the stdio loop after the
    /// write, and by [`McpServer::execute`] for in-process callers.
    pub fn acknowledge(&self, keys: &[&'static str]) {
        if keys.is_empty() {
            return;
        }
        let mut guides = lock(&self.guides);
        for key in keys {
            guides.pending.remove(key);
            guides.delivered.insert(key);
        }
    }

    /// Forgets guides whose response could not be written: the next change for them delivers the guide again.
    pub fn release(&self, keys: &[&'static str]) {
        let mut guides = lock(&self.guides);
        for key in keys {
            guides.pending.remove(key);
        }
    }

    /// Writes a response that carries `keys` and records them as delivered in one step: guide checks wait while it is
    /// written, so a client that has read the guide is never told it is still on its way. A failed write releases them.
    pub fn write_delivering(&self, keys: &[&'static str], write: impl FnOnce() -> io::Result<()>) -> io::Result<()> {
        if keys.is_empty() {
            return write();
        }
        let mut guides = lock(&self.guides);
        let written = write();
        for key in keys {
            guides.pending.remove(key);
            if written.is_ok() {
                guides.delivered.insert(key);
            }
        }
        written
    }

    /// The artifact a guided call changes: `artifact_id`, a proposal's nested change set, or a stored proposal's artifact.
    fn guided_target(&self, call: &PendingCall) -> DltResult<Option<String>> {
        let direct = call
            .arguments
            .get("artifact_id")
            .or_else(|| call.arguments.get("change_set").and_then(|set| set.get("artifact_id")))
            .and_then(Value::as_str);
        if let Some(id) = direct {
            return Ok(Some(id.to_string()));
        }
        if call.tool.method == "proposal.apply" {
            if let Some(proposal_id) = call.arguments.get("proposal_id").and_then(Value::as_str) {
                let proposal = self.backend.call("proposal.get", json!({ "proposal_id": proposal_id }))?;
                let id = ["/artifact_id", "/change_set/artifact_id", "/proposal/artifact_id", "/proposal/change_set/artifact_id"]
                    .iter()
                    .find_map(|pointer| proposal.pointer(pointer).and_then(Value::as_str))
                    .map(str::to_string);
                return Ok(id);
            }
        }
        Ok(None)
    }

    /// GUIDE_REQUIRED for the first change to an artifact whose guide this connection has not delivered. The refusal carries
    /// the guide and nothing reaches the core, so the same call can be sent again unchanged. A change sent while another
    /// response is carrying the guide is held too, with a short note. A failed artifact lookup refuses the change.
    fn guide_refusal(&self, call: &PendingCall, delivered: &mut Vec<&'static str>) -> DltResult<Option<Value>> {
        if !self.guide_gate || !GUIDED_METHODS.contains(&call.tool.method) {
            return Ok(None);
        }
        let Some(artifact_id) = self.guided_target(call).map_err(lookup_failed)? else { return Ok(None) };
        let Some((kind, title)) = self.artifact_info(&artifact_id).map_err(lookup_failed)? else { return Ok(None) };
        let text = match self.claim(kind, &title, false) {
            Claim::Delivered => return Ok(None),
            Claim::Pending => format!(
                "{GUIDE_REQUIRED}: nothing was applied. The design guide for {artifact_id} is in another response of this connection that you have not received yet. Read it, then send this call again."
            ),
            Claim::Claimed(key, guide) => {
                delivered.push(key);
                format!(
                    "{GUIDE_REQUIRED}: nothing was applied. This is the first change to {artifact_id} on this connection, and its design guide had not been read. Read the guide below, plan the design by it, then send the call again (the same request_id is fine: the core never saw this call).\n\n{guide}"
                )
            }
        };
        Ok(Some(json!({
            "content": [{ "type": "text", "text": text }],
            "structuredContent": { "code": GUIDE_REQUIRED, "message": "read the design guide, then send the call again", "artifact_id": artifact_id },
            "isError": true,
        })))
    }

    /// After a call: learn artifact kinds from its answer, and add the artifact's guide to the content the first time an
    /// artifact of that guide is introduced. Lookup failures only leave the guide out: this call changed nothing.
    fn guide_content(&self, call: &PendingCall, value: &Value, delivered: &mut Vec<&'static str>) -> Option<Value> {
        if !self.guide_gate {
            return None;
        }
        match call.tool.method {
            "workspace.list" => {
                self.learn_artifacts(value);
                None
            }
            method if INTRODUCING_METHODS.contains(&method) => {
                {
                    let mut guides = lock(&self.guides);
                    learn_summary(&mut guides, value);
                    if let Some(summary) = value.get("artifact") {
                        learn_summary(&mut guides, summary);
                    }
                }
                let artifact_id = call.arguments.get("artifact_id").and_then(Value::as_str).map(str::to_string).or_else(|| {
                    value.get("id").or_else(|| value.get("artifact_id")).and_then(Value::as_str).map(str::to_string)
                });
                let (kind, title) = match artifact_id.as_deref().map(|id| self.artifact_info(id)) {
                    Some(Ok(Some(found))) => found,
                    _ => {
                        let kind = call.arguments.get("kind").cloned().and_then(|kind| serde_json::from_value::<ArtifactKind>(kind).ok())?;
                        (kind, call.arguments.get("title").and_then(Value::as_str).unwrap_or_default().to_string())
                    }
                };
                let Claim::Claimed(key, guide) = self.claim(kind, &title, false) else { return None };
                delivered.push(key);
                Some(json!({ "type": "text", "text": format!("Design guide for this artifact (mandatory; read it before the first change):\n\n{guide}") }))
            }
            _ => None,
        }
    }

    /// After a successful apply: the design-quality warnings of the committed revision, read from the receipt's own
    /// diagnostics (no second validation), so the agent sees them without asking.
    fn quality_note(&self, call: &PendingCall, receipt: &Value) -> Option<Value> {
        if call.tool.method != "changes.apply" {
            return None;
        }
        // The receipt can list a warning twice (from the engine's change and from the check of its result): each once. A shared
        // token change also reports warnings of the artifacts that use it, each naming its artifact in `params.artifact_id`.
        let target = call.arguments.get("artifact_id").and_then(Value::as_str);
        let mut seen = BTreeSet::new();
        let warnings: Vec<String> = receipt
            .get("diagnostics")
            .and_then(Value::as_array)?
            .iter()
            .filter(|d| d.get("blocking") != Some(&Value::Bool(true)))
            .filter(|d| d.get("code").and_then(Value::as_str).is_some_and(|code| manual::DESIGN_QUALITY_CODES.contains(&code)))
            .map(|d| {
                let code = d.get("code").and_then(Value::as_str).unwrap_or_default();
                let node = d.get("node_id").and_then(Value::as_str).unwrap_or("-");
                let message = d.get("message").and_then(Value::as_str).unwrap_or_default();
                match d.pointer("/params/artifact_id").and_then(Value::as_str).filter(|owner| Some(*owner) != target) {
                    Some(owner) => format!("- {code} on {node} in artifact {owner}: {message}"),
                    None => format!("- {code} on {node}: {message}"),
                }
            })
            .filter(|line| seen.insert(line.clone()))
            .take(QUALITY_NOTE_LIMIT)
            .collect();
        if warnings.is_empty() {
            return None;
        }
        let revision = receipt.get("revision").map_or_else(String::new, |revision| format!(" (revision {revision})"));
        Some(json!({ "type": "text", "text": format!(
            "Design-quality warnings after this change{revision} (fix each one the user did not explicitly ask for, then apply again):\n{}",
            warnings.join("\n")
        ) }))
    }

    /// Parses one line and routes it.
    pub fn route_line(&self, line: &str) -> Routed {
        // A Windows client's first write can start with a UTF-8 byte order mark (.NET stdin writers add one): it is no JSON.
        let line = line.strip_prefix('\u{feff}').unwrap_or(line);
        match serde_json::from_str::<Value>(line) {
            Ok(value) => self.route(value),
            Err(e) => Routed::Reply(error_reply(Value::Null, PARSE_ERROR, &format!("Parse error: {e}"))),
        }
    }

    /// Routes one parsed JSON-RPC message.
    pub fn route(&self, msg: Value) -> Routed {
        if msg.is_array() {
            return Routed::Reply(error_reply(
                Value::Null,
                INVALID_REQUEST,
                "JSON-RPC batches are not part of protocol 2025-11-25",
            ));
        }
        let Some(obj) = msg.as_object() else {
            return Routed::Reply(error_reply(Value::Null, INVALID_REQUEST, "Invalid Request: expected a JSON object"));
        };
        let id = obj.get("id").cloned();
        if obj.get("jsonrpc") != Some(&json!("2.0")) {
            return Routed::Reply(error_reply(
                id.unwrap_or(Value::Null),
                INVALID_REQUEST,
                "Invalid Request: jsonrpc must be \"2.0\"",
            ));
        }
        let Some(method_value) = obj.get("method") else {
            // A response to a request this server never sends: nothing to answer.
            if obj.contains_key("result") || obj.contains_key("error") {
                return Routed::Nothing;
            }
            return Routed::Reply(error_reply(
                id.unwrap_or(Value::Null),
                INVALID_REQUEST,
                "Invalid Request: method is missing",
            ));
        };
        let Some(method) = method_value.as_str() else {
            return Routed::Reply(error_reply(
                id.unwrap_or(Value::Null),
                INVALID_REQUEST,
                "Invalid Request: method must be a string",
            ));
        };
        match id {
            Some(id) => self.request(id, method, obj.get("params")),
            None => {
                self.notification(method);
                Routed::Nothing
            }
        }
    }

    fn ready(&self) -> bool {
        *lock(&self.phase) == Phase::Ready
    }

    fn request(&self, id: Value, method: &str, params: Option<&Value>) -> Routed {
        match method {
            "initialize" => self.initialize(id, params),
            "ping" => Routed::Reply(result_reply(id, json!({}))),
            "tools/list" if self.ready() => {
                let tools: Vec<Value> = self.tools.iter().map(|tool| {
                    let mut definition = tool.to_mcp();
                    if self.startup.is_some() {
                        definition["inputSchema"]["properties"]["background"] = json!({
                            "type":"boolean", "description":"Set true when the user explicitly requests background work. Otherwise the desktop window opens by default."
                        });
                    }
                    definition
                }).collect();
                Routed::Reply(result_reply(id, json!({ "tools": tools })))
            }
            "tools/call" if self.ready() => self.call(id, params),
            "resources/list" if self.ready() => Routed::Reply(result_reply(id, manual::resources_result())),
            "resources/templates/list" if self.ready() => Routed::Reply(result_reply(id, json!({ "resourceTemplates": [] }))),
            "resources/read" if self.ready() => Routed::Reply(read_resource_reply(id, params)),
            "prompts/list" if self.ready() => Routed::Reply(result_reply(id, manual::prompts_result())),
            "prompts/get" if self.ready() => Routed::Reply(get_prompt_reply(id, params)),
            "tools/list" | "tools/call" | "resources/list" | "resources/templates/list" | "resources/read" | "prompts/list"
            | "prompts/get" => Routed::Reply(not_initialized(id)),
            other => Routed::Reply(error_reply(id, METHOD_NOT_FOUND, &format!("Method not found: {other}"))),
        }
    }

    fn notification(&self, method: &str) {
        if method == "notifications/initialized" {
            let mut phase = lock(&self.phase);
            if *phase == Phase::Initialized {
                *phase = Phase::Ready;
            }
        }
        // Other notifications (cancelled, progress) are ignored: tool calls run to completion.
    }

    /// `job.set_client`: the job's state shows the MCP client's name and version (the agent name in the GUI). Best
    /// effort: when the core refuses it (for example, the job is no longer active), the reason goes to stderr and
    /// `initialize` still succeeds. The job's tools then answer with the core's own errors.
    fn report_client(&self, params: Option<&Value>) {
        let Some(info) = params.and_then(|p| p.get("clientInfo")) else {
            return;
        };
        let Some(name) = info.get("name").and_then(Value::as_str).map(str::trim).filter(|name| !name.is_empty()) else {
            return;
        };
        let mut arguments = json!({ "client_name": name });
        if let Some(version) = info.get("version").and_then(Value::as_str) {
            arguments["client_version"] = json!(version);
        }
        if let Err(error) = self.backend.call("job.set_client", arguments) {
            eprintln!("designllm: job.set_client was refused: {error}");
        }
    }

    /// The `clientInfo` of `initialize`: the trimmed name and, when the client sent one, its version.
    fn client_identity(params: Option<&Value>) -> Option<(String, Option<String>)> {
        let info = params?.get("clientInfo")?;
        let name = info.get("name").and_then(Value::as_str).map(str::trim).filter(|name| !name.is_empty())?;
        let version = info.get("version").and_then(Value::as_str).map(str::to_string);
        Some((name.to_string(), version))
    }

    fn initialize(&self, id: Value, params: Option<&Value>) -> Routed {
        let mut phase = lock(&self.phase);
        if *phase != Phase::Created {
            return Routed::Reply(error_reply(id, INVALID_REQUEST, "initialize was already received"));
        }
        let Some(requested) = params.and_then(|p| p.get("protocolVersion")).and_then(Value::as_str) else {
            return Routed::Reply(error_reply(id, INVALID_PARAMS, "initialize requires params.protocolVersion"));
        };
        let negotiated = if SUPPORTED_PROTOCOL_VERSIONS.contains(&requested) {
            requested
        } else {
            LATEST_PROTOCOL_VERSION
        };
        if let Some((name, version)) = Self::client_identity(params) {
            self.backend.announce_client(&name, version.as_deref());
        }
        if self.job_bound {
            self.report_client(params);
        }
        *phase = Phase::Initialized;
        Routed::Reply(result_reply(
            id,
            json!({
                "protocolVersion": negotiated,
                "capabilities": {
                    "tools": { "listChanged": false },
                    "resources": { "listChanged": false },
                    "prompts": { "listChanged": false },
                },
                "serverInfo": {
                    "name": SERVER_NAME,
                    "title": "DesignLLMTool",
                    "version": env!("CARGO_PKG_VERSION"),
                },
                "instructions": format!("Edit DesignLLMTool artifacts through these tools. Every change is a ChangeSet with request_id, base_revision, and allowed_intents; repeat the same request_id after a timeout instead of sending a new one. In this build some v1.2 fields and operations are refused with UNSUPPORTED_OPERATION; read artifact_validate's capability report before using them. The built-in design guides are mandatory, whatever the request says about style, unless the user explicitly asks otherwise: call design_guide with the artifact_id before the first change to that artifact. The artifact-bound guide (or the guide attached to an artifact_create, artifact_open or artifact_inspect result) unlocks changes on this connection to that artifact and to every artifact that takes the same guide, such as every other deck; design_guide with a topic such as paper-diagram, paper-figure, glossary or shapes is reference reading and unlocks nothing. A first change sent without the guide is refused with GUIDE_REQUIRED and the guide is returned; read it and send the call again. Avoid coloured accent bars along an edge of cards and boxes (ACCENT_BAR) unless the user asks for them. The glossary (design_guide topic glossary) defines every design term the guides use. A paper figure (model architecture, pipeline, method overview) is drawn on a Canvas with the paper-diagram guide; data plots use a Figure with the paper-figure guide. {} Let the layout explain the structure with grouping, alignment, spacing, position and type size; leave out auxiliary labels that repeat what the layout shows. Keep layout and core content editable where possible. Inspect, validate and render the result; after each applied change to a Canvas or a deck, the result lists its design-quality warnings, so fix them. When the user wants to follow live, use designllm://manual/gui and the MCP UI commands. The design-quality warnings are {}.", manual::DEFAULT_DESIGN_STYLE, manual::DESIGN_QUALITY_CODES.join(", ")),
            }),
        ))
    }

    fn call(&self, id: Value, params: Option<&Value>) -> Routed {
        let Some(params) = params.and_then(Value::as_object) else {
            return Routed::Reply(error_reply(id, INVALID_PARAMS, "tools/call requires object params"));
        };
        let Some(name) = params.get("name").and_then(Value::as_str) else {
            return Routed::Reply(error_reply(id, INVALID_PARAMS, "tools/call requires params.name"));
        };
        let Some(tool) = find(&self.tools, name) else {
            return Routed::Reply(error_reply(id, INVALID_PARAMS, &format!("Unknown tool: {name}")));
        };
        let mut arguments = match params.get("arguments") {
            None | Some(Value::Null) => json!({}),
            Some(value) => value.clone(),
        };
        let mut background = false;
        if self.startup.is_some() {
            if let Some(value) = arguments.as_object_mut().and_then(|args| args.remove("background")) {
                let Some(requested) = value.as_bool() else {
                    return Routed::Reply(result_reply(id, tool_failure_text("background must be a boolean")));
                };
                background = requested;
            }
        }
        // Argument-schema failures are tool-execution errors (2025-11-25 §Error Handling): the model sees them
        // in the result and can correct the call. Unknown tools stay protocol errors (-32602) above.
        if let Err(message) = validate_arguments(&(tool.schema)(), &arguments) {
            return Routed::Reply(result_reply(id, tool_failure_text(&format!("{}: {message}", tool.name))));
        }
        Routed::Call(PendingCall { id, tool, arguments, background })
    }

    /// Runs a call and returns the MCP `CallToolResult`. Core failures become `isError: true` results. An in-process caller
    /// has the result in hand when this returns, so the guides it carries count as delivered at once.
    pub fn execute(&self, call: &PendingCall) -> Value {
        let (result, delivered) = self.execute_unacknowledged(call);
        self.acknowledge(&delivered);
        result
    }

    /// [`McpServer::execute`] without marking the guides delivered: the caller does so with [`McpServer::acknowledge`]
    /// once the result has been written to the client.
    pub fn execute_unacknowledged(&self, call: &PendingCall) -> (Value, Vec<&'static str>) {
        self.permits.acquire();
        let mut delivered = Vec::new();
        let mut result = match self.prepare_call(call.background).and_then(|()| self.run(call, &mut delivered)) {
            Ok(result) => result,
            Err(error) => tool_failure(&error),
        };
        self.attach_update_notice(&mut result);
        self.permits.release();
        (result, delivered)
    }

    fn prepare_call(&self, background: bool) -> DltResult<()> {
        let Some(startup) = &self.startup else { return Ok(()) };
        let _gate = lock(&startup.gate);
        (startup.start)(background)
    }

    /// `design_guide`: the mandatory guide of one artifact (with `artifact_id`, and the page `topic` names after it when both
    /// are given), a manual page by `topic`, or the index with the shared workflow. Answered here from the built-in manual.
    /// Only the artifact-bound answer unlocks changes, to that artifact and to every artifact that takes the same guide (the
    /// guide key: same text): a topic page alone is reference reading.
    fn run_guide(&self, call: &PendingCall, delivered: &mut Vec<&'static str>) -> DltResult<Value> {
        let topic = text_argument(&call.arguments, "topic")?;
        let topic_text = match topic {
            Some(topic) => Some(manual::guide_topic(topic).ok_or_else(|| DltError::invalid(format!("design_guide has no topic {topic}")))?),
            None => None,
        };
        if let Some(artifact_id) = text_argument(&call.arguments, "artifact_id")? {
            let (kind, title) = self
                .artifact_info(artifact_id)?
                .ok_or_else(|| DltError::new(ErrorCode::NotFound, format!("artifact {artifact_id} is not in this workspace")))?;
            let Claim::Claimed(key, mut text) = self.claim(kind, &title, true) else { unreachable!("a forced claim always claims") };
            delivered.push(key);
            if let Some(extra) = topic_text {
                text.push_str("\n\n");
                text.push_str(&extra);
            }
            return Ok(tool_success(json!({ "artifact_id": artifact_id, "guide": key, "topic": topic }), vec![json!({ "type": "text", "text": text })]));
        }
        if let (Some(topic), Some(text)) = (topic, topic_text) {
            return Ok(tool_success(json!({ "topic": topic }), vec![json!({ "type": "text", "text": text })]));
        }
        let mut text = manual::guide_topic("index").unwrap_or_default();
        text.push('\n');
        text.push_str(&manual::guide_topic("design-workflow").unwrap_or_default());
        text.push('\n');
        text.push_str(&manual::guide_topic("glossary").unwrap_or_default());
        Ok(tool_success(json!({ "topic": "index" }), vec![json!({ "type": "text", "text": text })]))
    }

    /// `ui.screenshot`: queues the run, waits for the GUI to finish it, then returns the stored PNG as image content.
    /// A run still pending when the wait ends is reported with its status, so the caller can read it later.
    fn run_screenshot(&self, call: &PendingCall) -> DltResult<Value> {
        let queued = self.backend.call(call.tool.method, call.arguments.clone())?;
        let run_id = queued["run_id"]
            .as_str()
            .ok_or_else(|| DltError::invalid("ui.screenshot returned no run_id"))?
            .to_string();
        let timeout_ms = call
            .arguments
            .get("timeout_ms")
            .and_then(Value::as_u64)
            .unwrap_or(SCREENSHOT_DEFAULT_TIMEOUT_MS)
            .clamp(1, SCREENSHOT_MAX_TIMEOUT_MS);
        let deadline = Instant::now() + Duration::from_millis(timeout_ms + SCREENSHOT_GRACE_MS);
        let mut run = queued;
        while is_pending(&run) && Instant::now() < deadline {
            thread::sleep(SCREENSHOT_POLL);
            run = self.backend.call("ui.command.get", json!({ "run_id": run_id }))?;
        }
        match run["status"].as_str() {
            Some("completed") => {
                let asset = run["result"]["asset"].clone();
                let sha256 = asset["sha256"]
                    .as_str()
                    .ok_or_else(|| DltError::invalid("the screenshot result names no asset"))?
                    .to_string();
                let stored = fetch_asset(self.backend.as_ref(), &sha256, INLINE_IMAGE_LIMIT)?;
                let block = image_block(&stored.bytes, &stored.mime)
                    .ok_or_else(|| DltError::invalid(format!("screenshot asset {sha256} is {}, not a PNG", stored.mime)))?;
                let structured = json!({
                    "run_id": run_id,
                    "status": "completed",
                    "width_px": run["result"]["width_px"],
                    "height_px": run["result"]["height_px"],
                    "asset": asset,
                });
                Ok(tool_success(structured, vec![block]))
            }
            Some("failed") | Some("timed_out") => {
                let error = serde_json::from_value::<DltError>(run["error"].clone()).unwrap_or_else(|_| {
                    DltError::new(ErrorCode::Internal, format!("the screenshot run {run_id} ended as {}", run["status"]))
                });
                Err(error)
            }
            _ => Ok(tool_failure_text(&format!(
                "the screenshot run {run_id} is still {}; read it later with ui_command_get",
                run["status"].as_str().unwrap_or("pending")
            ))),
        }
    }

    fn run(&self, call: &PendingCall, delivered: &mut Vec<&'static str>) -> DltResult<Value> {
        // An operation hidden from MCP sessions is refused before the core is asked (operation_exposure).
        if let Some(refusal) = crate::operation_exposure::refusal(call.tool.method, &call.arguments) { return Err(refusal); }
        // The host refuses this too (non-gui connections); kept because a server over an in-process backend has no host.
        if let Some(refusal) = crate::review_gate::agent_refusal(call.tool.method, &call.arguments) { return Err(refusal); }
        if let Some(refusal) = self.guide_refusal(call, delivered)? { return Ok(refusal); }
        match call.tool.post {
            Post::Guide => self.run_guide(call, delivered),
            Post::None => {
                let value = self.backend.call(call.tool.method, call.arguments.clone())?;
                let extra: Vec<Value> =
                    [self.guide_content(call, &value, delivered), self.quality_note(call, &value)].into_iter().flatten().collect();
                Ok(tool_success(value, extra))
            }
            Post::RenderPreview => {
                let value = self.backend.call(call.tool.method, call.arguments.clone())?;
                let mut content = Vec::new();
                if let Some(preview) = value.get("preview").filter(|p| !p.is_null()) {
                    let sha = preview
                        .get("sha256")
                        .and_then(Value::as_str)
                        .ok_or_else(|| DltError::invalid("render preview has no sha256"))?;
                    let asset = fetch_asset(self.backend.as_ref(), sha, INLINE_IMAGE_LIMIT)?;
                    if let Some(block) = image_block(&asset.bytes, &asset.mime) {
                        content.push(block);
                    }
                }
                Ok(tool_success(value, content))
            }
            Post::TemplatePreview => {
                let value = self.backend.call(call.tool.method, call.arguments.clone())?;
                let mut content = Vec::new();
                if let Some(preview) = value.get("asset").filter(|a| !a.is_null()) {
                    let sha = preview
                        .get("sha256")
                        .and_then(Value::as_str)
                        .ok_or_else(|| DltError::invalid("template preview has no sha256"))?;
                    let fetched = fetch_asset(self.backend.as_ref(), sha, INLINE_IMAGE_LIMIT)?;
                    if let Some(block) = image_block(&fetched.bytes, &fetched.mime) {
                        content.push(block);
                    }
                }
                Ok(tool_success(value, content))
            }
            Post::UiScreenshot => self.run_screenshot(call),
            Post::AssetContent => {
                let sha = call
                    .arguments
                    .get("sha256")
                    .and_then(Value::as_str)
                    .ok_or_else(|| DltError::invalid("sha256 is required"))?;
                let limit = call.arguments.get("max_bytes").and_then(Value::as_u64).unwrap_or(INLINE_IMAGE_LIMIT);
                let asset = fetch_asset(self.backend.as_ref(), sha, limit.min(INLINE_IMAGE_LIMIT))?;
                let mut content = Vec::new();
                if let Some(block) = image_block(&asset.bytes, &asset.mime) {
                    content.push(block);
                } else if let Some(block) = text_block(&asset.bytes, &asset.mime) {
                    content.push(block);
                }
                let structured = json!({
                    "sha256": asset.sha256,
                    "mime": asset.mime,
                    "bytes": asset.bytes.len(),
                    "verified": true,
                });
                Ok(tool_success(structured, content))
            }
        }
    }
}

/// A GUI run is pending while it is queued or delivered.
fn is_pending(run: &Value) -> bool {
    matches!(run["status"].as_str(), Some("queued") | Some("delivered"))
}

/// Image content for MCP clients (PNG, JPEG, GIF, WebP). Other types return None.
fn image_block(bytes: &[u8], mime: &str) -> Option<Value> {
    let supported = matches!(mime, "image/png" | "image/jpeg" | "image/gif" | "image/webp");
    supported.then(|| json!({ "type": "image", "data": base64_encode(bytes), "mimeType": mime }))
}

/// Text content for small text-like assets (UTF-8 required). Other types return None.
fn text_block(bytes: &[u8], mime: &str) -> Option<Value> {
    let textual = mime.starts_with("text/") || mime == "application/json";
    if !textual || bytes.len() as u64 > INLINE_TEXT_LIMIT {
        return None;
    }
    let text = std::str::from_utf8(bytes).ok()?;
    Some(json!({ "type": "text", "text": text }))
}

fn tool_success(structured: Value, extra_content: Vec<Value>) -> Value {
    let text = structured.to_string();
    let text = if text.len() > TEXT_BLOCK_LIMIT {
        json!({ "summary": format!("structured result of {} bytes is in structuredContent", text.len()) }).to_string()
    } else {
        text
    };
    let mut content = vec![json!({ "type": "text", "text": text })];
    content.extend(extra_content);
    let structured = match structured {
        Value::Object(_) => structured,
        other => json!({ "value": other }),
    };
    json!({ "content": content, "structuredContent": structured, "isError": false })
}

fn tool_failure(error: &DltError) -> Value {
    let code = serde_json::to_value(&error.code)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| "INTERNAL".to_string());
    let structured = serde_json::to_value(error).unwrap_or_else(|_| json!({ "code": code, "message": error.message }));
    json!({
        "content": [{ "type": "text", "text": format!("{code}: {}", error.message) }],
        "structuredContent": structured,
        "isError": true,
    })
}

/// resources/read: the content of one manual page or the presets. An unknown URI is RESOURCE_NOT_FOUND with the URI.
fn read_resource_reply(id: Value, params: Option<&Value>) -> Value {
    let Some(uri) = params.and_then(|p| p.get("uri")).and_then(Value::as_str) else {
        return error_reply(id, INVALID_PARAMS, "resources/read requires params.uri");
    };
    match manual::read_resource(uri) {
        Some(content) => result_reply(id, json!({ "contents": [content] })),
        None => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": RESOURCE_NOT_FOUND, "message": "Resource not found", "data": { "uri": uri } },
        }),
    }
}

/// prompts/get: one task prompt, with the optional `artifact_id` argument.
fn get_prompt_reply(id: Value, params: Option<&Value>) -> Value {
    let Some(name) = params.and_then(|p| p.get("name")).and_then(Value::as_str) else {
        return error_reply(id, INVALID_PARAMS, "prompts/get requires params.name");
    };
    match manual::get_prompt(name, params.and_then(|p| p.get("arguments"))) {
        Some(prompt) => result_reply(id, prompt),
        None => error_reply(id, INVALID_PARAMS, &format!("Unknown prompt: {name}")),
    }
}

fn result_reply(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

/// `CallToolResult` for a failure the model should see (`isError: true`, text only).
fn tool_failure_text(text: &str) -> Value {
    json!({ "content": [{ "type": "text", "text": text }], "isError": true })
}

fn error_reply(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

fn not_initialized(id: Value) -> Value {
    error_reply(id, INVALID_REQUEST, "server is not initialized; send initialize, then notifications/initialized")
}

fn write_message<W: Write>(out: &Mutex<W>, value: &Value) -> io::Result<()> {
    // serde_json escapes newlines inside strings, so the compact text is one line.
    let text = value.to_string();
    let mut guard = lock(out);
    guard.write_all(text.as_bytes())?;
    guard.write_all(b"\n")?;
    guard.flush()
}

/// Runs the stdio loop until the input closes. Tool calls run on worker threads; replies can arrive out of order.
pub fn serve_stdio<R: BufRead, W: Write + Send + 'static>(input: R, output: W, server: Arc<McpServer>) -> io::Result<()> {
    let out = Arc::new(Mutex::new(output));
    for line in input.lines() {
        let line = line?;
        if line.trim_start_matches('\u{feff}').trim().is_empty() {
            continue;
        }
        match server.route_line(&line) {
            Routed::Reply(value) => write_message(&out, &value)?,
            Routed::Nothing => {}
            Routed::Call(call) => {
                let server = Arc::clone(&server);
                let out = Arc::clone(&out);
                thread::spawn(move || {
                    let (result, delivered) = server.execute_unacknowledged(&call);
                    // A guide counts as delivered once its response is on its way to the client; until then, changes for
                    // that artifact on other workers are held (`Claim::Pending`).
                    let _ = server.write_delivering(&delivered, || write_message(&out, &json!({ "jsonrpc": "2.0", "id": call.id, "result": result })));
                });
            }
        }
    }
    Ok(())
}

/// Attach-only MCP bridge over stdio. Returns when stdin closes. Does not start a host.
///
/// `bound_job` = `Some` for the MCP server a runtime starts for an app job. That connection is bound to
/// the job's attempt on the host, and only job-scope tools are listed.
pub fn run_stdio(workspace: &Path, bound_job: Option<JobRef>) -> DltResult<()> {
    run_stdio_backend(workspace, bound_job, None, None)
}

/// Starts the app on the first workspace tool call that needs the host. `update` is filled by the background update check
/// when a newer release exists; the agent is told about it once on this connection.
pub fn run_stdio_with_startup(
    workspace: &Path,
    start: impl Fn(bool) -> DltResult<()> + Send + Sync + 'static,
    update: Option<UpdateSlot>,
) -> DltResult<()> {
    run_stdio_backend(workspace, None, Some(Startup { start: Box::new(start), gate: Mutex::new(()) }), update)
}

fn run_stdio_backend(workspace: &Path, bound_job: Option<JobRef>, startup: Option<Startup>, update: Option<UpdateSlot>) -> DltResult<()> {
    let (scope, client) = match bound_job {
        Some(job) => (Scope::JobBound, Client::with_job(workspace, job)),
        None => (Scope::Workspace, Client::with_role(workspace, ClientRole::Bridge)),
    };
    let mut server = McpServer::new(Arc::new(ClientBackend::new(client)), scope);
    server.startup = startup;
    server.update = update;
    let stdin = io::stdin();
    serve_stdio(stdin.lock(), io::stdout(), Arc::new(server)).map_err(DltError::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assets::ASSET_CHUNK_BYTES;
    use base64::Engine;
    use designllm_contracts::{sha256, ErrorCode};
    use std::collections::VecDeque;

    /// In-memory backend: records calls and answers from a queue of canned results.
    #[derive(Default)]
    struct Fake {
        calls: Mutex<Vec<(String, Value)>>,
        answers: Mutex<VecDeque<DltResult<Value>>>,
        assets: Vec<(String, Vec<u8>)>,
    }

    impl ToolBackend for Fake {
        fn call(&self, method: &str, params: Value) -> DltResult<Value> {
            self.calls.lock().unwrap().push((method.to_string(), params.clone()));
            if method == "asset.read" {
                let sha = params["sha256"].as_str().unwrap();
                let (_, bytes) = self.assets.iter().find(|(h, _)| h == sha).expect("asset present");
                let offset = params["offset"].as_u64().unwrap() as usize;
                let max = (params["max_bytes"].as_u64().unwrap() as usize).min(ASSET_CHUNK_BYTES as usize);
                let end = (offset + max).min(bytes.len());
                return Ok(json!({
                    "mime": "image/png",
                    "base64": base64_encode(&bytes[offset..end]),
                    "offset": offset,
                    "eof": end == bytes.len(),
                    "total_bytes": bytes.len(),
                }));
            }
            self.answers.lock().unwrap().pop_front().unwrap_or_else(|| Ok(json!({ "ok": true })))
        }
    }

    fn server_with(fake: Fake, scope: Scope) -> (Arc<Fake>, McpServer) {
        let fake = Arc::new(fake);
        let backend: Arc<dyn ToolBackend> = fake.clone();
        (fake, McpServer::new(backend, scope))
    }

    fn reply(server: &McpServer, msg: Value) -> Value {
        match server.route(msg) {
            Routed::Reply(v) => v,
            other => panic!("expected a reply, got {other:?}"),
        }
    }

    fn initialize(server: &McpServer, version: &str) -> Value {
        reply(
            server,
            json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":version,"capabilities":{},"clientInfo":{"name":"t","version":"0"}}}),
        )
    }

    fn ready(server: &McpServer) {
        let _ = initialize(server, LATEST_PROTOCOL_VERSION);
        assert!(matches!(server.route(json!({"jsonrpc":"2.0","method":"notifications/initialized"})), Routed::Nothing));
    }

    #[test]
    fn workspace_calls_default_to_visible_and_keep_background_out_of_core_arguments() {
        let backend = Arc::new(Fake::default());
        let mut server = McpServer::new(backend.clone(), Scope::Workspace);
        let starts = Arc::new(Mutex::new(Vec::new()));
        let observed = starts.clone();
        server.startup = Some(Startup { start: Box::new(move |background| {
            lock(&observed).push(background);
            Ok(())
        }), gate: Mutex::new(()) });
        ready(&server);

        let listing = reply(&server, json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}));
        assert_eq!(listing.pointer("/result/tools/0/inputSchema/properties/background/type"), Some(&json!("boolean")));
        for arguments in [json!({"background":true}), json!({}), json!({})] {
            let Routed::Call(call) = server.route(json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{
                "name":"workspace_list","arguments":arguments
            }})) else { panic!("valid workspace tool call") };
            assert!(server.execute(&call)["isError"] != true);
        }
        assert_eq!(*lock(&starts), vec![true, false, false]);
        assert!(lock(&backend.calls).iter().all(|(_, arguments)| arguments == &json!({})));
    }

    #[test]
    fn negotiation_echoes_supported_and_answers_latest_otherwise() {
        let (_f, s) = server_with(Fake::default(), Scope::Workspace);
        assert_eq!(initialize(&s, "2025-06-18")["result"]["protocolVersion"], "2025-06-18");
        let (_f, s) = server_with(Fake::default(), Scope::Workspace);
        assert_eq!(initialize(&s, "1999-01-01")["result"]["protocolVersion"], LATEST_PROTOCOL_VERSION);
        let (_f, s) = server_with(Fake::default(), Scope::Workspace);
        let r = reply(&s, json!({"jsonrpc":"2.0","id":2,"method":"initialize","params":{}}));
        assert_eq!(r["error"]["code"], INVALID_PARAMS);
    }

    #[test]
    fn initialize_advertises_tools_and_a_server_identity() {
        let (_f, s) = server_with(Fake::default(), Scope::Workspace);
        let r = initialize(&s, LATEST_PROTOCOL_VERSION);
        assert!(r["result"]["capabilities"]["tools"].is_object());
        assert_eq!(r["result"]["serverInfo"]["name"], SERVER_NAME);
    }

    #[test]
    fn tools_are_refused_until_the_initialized_notification() {
        let (_f, s) = server_with(Fake::default(), Scope::Workspace);
        assert_eq!(reply(&s, json!({"jsonrpc":"2.0","id":3,"method":"tools/list"}))["error"]["code"], INVALID_REQUEST);
        let _ = initialize(&s, LATEST_PROTOCOL_VERSION);
        assert_eq!(reply(&s, json!({"jsonrpc":"2.0","id":4,"method":"tools/list"}))["error"]["code"], INVALID_REQUEST);
        ready(&s);
        assert!(reply(&s, json!({"jsonrpc":"2.0","id":5,"method":"tools/list"}))["result"]["tools"].is_array());
    }

    #[test]
    fn second_initialize_and_batches_are_refused() {
        let (_f, s) = server_with(Fake::default(), Scope::Workspace);
        let _ = initialize(&s, LATEST_PROTOCOL_VERSION);
        assert_eq!(initialize(&s, LATEST_PROTOCOL_VERSION)["error"]["code"], INVALID_REQUEST);
        let (_f, s) = server_with(Fake::default(), Scope::Workspace);
        assert_eq!(reply(&s, json!([{"jsonrpc":"2.0","id":1,"method":"ping"}]))["error"]["code"], INVALID_REQUEST);
    }

    #[test]
    fn tools_list_entries_have_schema_and_explicit_annotations() {
        let (_f, s) = server_with(Fake::default(), Scope::Workspace);
        ready(&s);
        let list = reply(&s, json!({"jsonrpc":"2.0","id":6,"method":"tools/list"}));
        let tools = list["result"]["tools"].as_array().unwrap();
        assert!(tools.len() >= 30);
        for tool in tools {
            assert_eq!(tool["inputSchema"]["type"], "object", "{tool}");
            assert!(tool["annotations"]["readOnlyHint"].is_boolean());
        }
    }

    #[test]
    fn job_bound_scope_lists_only_scoped_tools() {
        let (_f, s) = server_with(Fake::default(), Scope::JobBound);
        ready(&s);
        let names: Vec<String> = reply(&s, json!({"jsonrpc":"2.0","id":7,"method":"tools/list"}))["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect();
        assert!(names.contains(&"changes_apply".to_string()));
        assert!(names.contains(&"workspace_list".to_string()), "workspace.list remains available in JobBound scope");
        assert!(!names.contains(&"job_start".to_string()));
    }

    #[test]
    fn unknown_tool_is_a_protocol_error_and_bad_arguments_are_tool_errors_never_reaching_the_backend() {
        let (fake, s) = server_with(Fake::default(), Scope::Workspace);
        ready(&s);
        let unknown = reply(&s, json!({"jsonrpc":"2.0","id":8,"method":"tools/call","params":{"name":"nope"}}));
        assert_eq!(unknown["error"]["code"], INVALID_PARAMS);
        let missing = reply(&s, json!({"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":"asset_lookup","arguments":{}}}));
        assert!(missing.get("error").is_none(), "argument failures are results, not protocol errors");
        assert_eq!(missing["result"]["isError"], true);
        assert!(missing["result"]["content"][0]["text"].as_str().unwrap().contains("sha256"));
        let extra = reply(&s, json!({"jsonrpc":"2.0","id":10,"method":"tools/call","params":{"name":"workspace_list","arguments":{"x":1}}}));
        assert_eq!(extra["result"]["isError"], true);
        assert!(fake.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn tool_call_maps_to_the_core_method_with_arguments_unchanged() {
        let fake = Fake::default();
        let response = json!({
            "workspace_id": "workspace-1",
            "artifacts": [],
            "workspace_notices": [{
                "code": "backup_notice",
                "message": "Workspace backup guidance",
                "node_id": null,
                "blocking": false,
                "params": {},
            }],
        });
        fake.answers.lock().unwrap().push_back(Ok(response.clone()));
        let (fake, s) = server_with(fake, Scope::Workspace);
        ready(&s);
        let call = match s.route(json!({"jsonrpc":"2.0","id":11,"method":"tools/call","params":{"name":"workspace_list"}})) {
            Routed::Call(c) => c,
            other => panic!("expected a call, got {other:?}"),
        };
        let result = s.execute(&call);
        assert_eq!(result["isError"], false);
        assert_eq!(result["structuredContent"], response, "Post::None preserves optional workspace notices");
        assert_eq!(fake.calls.lock().unwrap()[0].0, "workspace.list");

        let response_without_notices = json!({ "workspace_id": "workspace-2", "artifacts": [] });
        fake.answers.lock().unwrap().push_back(Ok(response_without_notices.clone()));
        let call_without_notices = match s.route(json!({"jsonrpc":"2.0","id":12,"method":"tools/call","params":{"name":"workspace_list"}})) {
            Routed::Call(c) => c,
            other => panic!("expected a call, got {other:?}"),
        };
        let result_without_notices = s.execute(&call_without_notices);
        assert_eq!(result_without_notices["isError"], false);
        assert_eq!(result_without_notices["structuredContent"], response_without_notices, "legacy results without notices remain valid");
    }

    #[test]
    fn core_error_becomes_an_is_error_result_with_the_contract_code() {
        let fake = Fake::default();
        fake.answers.lock().unwrap().push_back(Err(DltError::new(ErrorCode::RevisionConflict, "stale base")));
        let (_fake, s) = server_with(fake, Scope::Workspace);
        ready(&s);
        let call = match s.route(json!({"jsonrpc":"2.0","id":12,"method":"tools/call","params":{"name":"workspace_list"}})) {
            Routed::Call(c) => c,
            other => panic!("{other:?}"),
        };
        let result = s.execute(&call);
        assert_eq!(result["isError"], true);
        assert!(result["content"][0]["text"].as_str().unwrap().starts_with("REVISION_CONFLICT"));
        assert_eq!(result["structuredContent"]["code"], "REVISION_CONFLICT");
    }

    #[test]
    fn asset_read_assembles_chunks_and_returns_image_content() {
        let png: Vec<u8> = [vec![0x89u8, b'P', b'N', b'G'], vec![7u8; 9000]].concat();
        let sha = sha256(&png);
        let fake = Fake { assets: vec![(sha.clone(), png.clone())], ..Default::default() };
        let (_fake, s) = server_with(fake, Scope::Workspace);
        ready(&s);
        let call = match s.route(json!({"jsonrpc":"2.0","id":13,"method":"tools/call","params":{"name":"asset_read","arguments":{"sha256": sha}}})) {
            Routed::Call(c) => c,
            other => panic!("{other:?}"),
        };
        let result = s.execute(&call);
        assert_eq!(result["isError"], false, "{result}");
        let image = result["content"].as_array().unwrap().iter().find(|b| b["type"] == "image").expect("image block");
        assert_eq!(image["mimeType"], "image/png");
        let decoded = base64::engine::general_purpose::STANDARD.decode(image["data"].as_str().unwrap()).unwrap();
        assert_eq!(decoded, png);
        assert_eq!(result["structuredContent"]["verified"], true);
    }

    #[test]
    fn render_preview_is_fetched_by_hash_and_returned_as_image() {
        let png: Vec<u8> = vec![0x89, b'P', b'N', b'G', 1, 2, 3];
        let sha = sha256(&png);
        let render = json!({"revision": 3, "preview": {"sha256": sha, "mime": "image/png", "bytes": png.len()}});
        let fake = Fake { assets: vec![(sha.clone(), png.clone())], ..Default::default() };
        fake.answers.lock().unwrap().push_back(Ok(render));
        let (_fake, s) = server_with(fake, Scope::Workspace);
        ready(&s);
        let call = match s.route(json!({"jsonrpc":"2.0","id":14,"method":"tools/call","params":{"name":"render_get","arguments":{"artifact_id":"a","expected_revision":3}}})) {
            Routed::Call(c) => c,
            other => panic!("{other:?}"),
        };
        let result = s.execute(&call);
        assert!(result["content"].as_array().unwrap().iter().any(|b| b["type"] == "image"));
        assert_eq!(result["structuredContent"]["revision"], 3);
    }

    #[test]
    fn parse_errors_and_unknown_methods_have_jsonrpc_codes() {
        let (_f, s) = server_with(Fake::default(), Scope::Workspace);
        match s.route_line("{not json") {
            Routed::Reply(v) => assert_eq!(v["error"]["code"], PARSE_ERROR),
            other => panic!("{other:?}"),
        }
        // resources/list is implemented now; logging/setLevel is a method this server does not have.
        assert_eq!(reply(&s, json!({"jsonrpc":"2.0","id":15,"method":"logging/setLevel"}))["error"]["code"], METHOD_NOT_FOUND);
        assert_eq!(reply(&s, json!({"jsonrpc":"2.0","id":16,"method":"ping"}))["result"], json!({}));
    }

    #[test]
    fn notifications_are_silent_and_responses_from_the_client_are_ignored() {
        let (_f, s) = server_with(Fake::default(), Scope::Workspace);
        assert!(matches!(s.route(json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{}})), Routed::Nothing));
        assert!(matches!(s.route(json!({"jsonrpc":"2.0","id":"x","result":{}})), Routed::Nothing));
    }

    #[test]
    fn serve_stdio_writes_one_line_per_reply_and_ends_at_eof() {
        let (_f, s) = server_with(Fake::default(), Scope::Workspace);
        let input = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}\n\n{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n";
        let out = Arc::new(Mutex::new(Vec::<u8>::new()));
        struct Sink(Arc<Mutex<Vec<u8>>>);
        impl Write for Sink {
            fn write(&mut self, b: &[u8]) -> io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        serve_stdio(io::Cursor::new(&input[..]), Sink(Arc::clone(&out)), Arc::new(s)).unwrap();
        let text = String::from_utf8(out.lock().unwrap().clone()).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 1, "{text}");
        assert!(lines[0].contains("\"result\":{}"));
    }

    #[test]
    fn a_leading_byte_order_mark_is_not_a_parse_error() {
        let (_f, s) = server_with(Fake::default(), Scope::Workspace);
        let line = "\u{feff}{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{\"protocolVersion\":\"2025-11-25\",\"capabilities\":{},\"clientInfo\":{\"name\":\"t\",\"version\":\"0\"}}}";
        let Routed::Reply(reply) = s.route_line(line) else { panic!("initialize is answered") };
        assert_eq!(reply["result"]["serverInfo"]["name"], SERVER_NAME, "{reply}");
    }

    fn run_tool(server: &McpServer, name: &str, arguments: Value) -> Value {
        let Routed::Call(call) = server.route(json!({"jsonrpc":"2.0","id":20,"method":"tools/call","params":{"name":name,"arguments":arguments}}))
        else { panic!("{name} is a valid call") };
        server.execute(&call)
    }

    fn change_set(artifact_id: &str) -> Value {
        json!({"request_id":"00000000-0000-4000-8000-000000000001","actor":"agent","workspace_id":"w","artifact_id":artifact_id,"base_revision":1,
            "allowed_intents":["content"],"operations":[]})
    }

    fn listing(kind: &str, title: &str) -> DltResult<Value> {
        Ok(json!({"workspace_id":"w","artifacts":[{"id":"a1","kind":kind,"title":title,"revision":1,"engine_id":"e","dirty":false}]}))
    }

    fn methods(fake: &Fake) -> Vec<String> {
        fake.calls.lock().unwrap().iter().map(|(method, _)| method.clone()).collect()
    }

    #[test]
    fn the_first_change_is_held_until_the_artifact_guide_is_delivered_and_then_passes() {
        let fake = Fake::default();
        fake.answers.lock().unwrap().push_back(listing("canvas", "Transformer 구조도"));
        let (fake, s) = server_with(fake, Scope::Workspace);
        ready(&s);
        let held = run_tool(&s, "changes_apply", change_set("a1"));
        assert_eq!(held["isError"], true);
        assert_eq!(held["structuredContent"]["code"], GUIDE_REQUIRED);
        let text = held["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("paper-diagram") && text.contains("circle_plus") && text.contains("Design glossary"), "the paper guide comes with it");
        assert_eq!(methods(&fake), ["workspace.list"], "nothing reached the core's change methods");
        let passed = run_tool(&s, "changes_apply", change_set("a1"));
        assert_eq!(passed["isError"], false, "{passed}");
        assert_eq!(methods(&fake), ["workspace.list", "changes.apply"], "the apply runs; its warnings come from its own receipt");
    }

    #[test]
    fn design_guide_answers_topics_locally_and_delivers_an_artifact_guide() {
        let fake = Fake::default();
        fake.answers.lock().unwrap().push_back(listing("figure", "Results"));
        let (fake, s) = server_with(fake, Scope::Workspace);
        ready(&s);
        let glossary = run_tool(&s, "design_guide", json!({"topic":"glossary"}));
        assert_eq!(glossary["isError"], false);
        assert!(glossary["content"][1]["text"].as_str().unwrap().contains("Auxiliary label"));
        assert!(methods(&fake).is_empty(), "topics never reach the core");
        let unknown = run_tool(&s, "design_guide", json!({"topic":"nope"}));
        assert_eq!(unknown["isError"], true, "the schema enum or the manual refuses an unknown topic");
        let guide = run_tool(&s, "design_guide", json!({"artifact_id":"a1"}));
        assert_eq!(guide["structuredContent"]["guide"], "paper-figure");
        let passed = run_tool(&s, "changes_validate", change_set("a1"));
        assert_eq!(passed["isError"], false, "the guide was delivered: {passed}");
        assert_eq!(methods(&fake), ["workspace.list", "changes.validate"]);
    }

    #[test]
    fn creating_an_artifact_returns_its_guide_once_and_job_connections_are_never_held() {
        let fake = Fake::default();
        fake.answers.lock().unwrap().push_back(Ok(json!({"id":"a2","kind":"slides","title":"Deck","revision":0,"engine_id":"e","dirty":false})));
        let (_fake, s) = server_with(fake, Scope::Workspace);
        ready(&s);
        let created = run_tool(&s, "artifact_create", json!({"request_id":"00000000-0000-4000-8000-000000000002","actor":"agent","kind":"slides","title":"Deck","size":{"width":12192000,"height":6858000}}));
        assert_eq!(created["content"].as_array().unwrap().len(), 2, "the result and the guide");
        assert!(created["content"][1]["text"].as_str().unwrap().contains("manual/slides"));
        let again = run_tool(&s, "changes_apply", change_set("a2"));
        assert_eq!(again["isError"], false, "the guide was delivered with the create: {again}");

        let (fake, job) = server_with(Fake::default(), Scope::JobBound);
        ready(&job);
        assert_eq!(run_tool(&job, "changes_apply", change_set("a1"))["isError"], false);
        assert_eq!(methods(&fake), ["job.set_client", "changes.apply"], "a job's prompt carries the guide, so its connection is not held");
    }

    #[test]
    fn an_applied_change_reports_the_design_quality_warnings_of_the_result() {
        let fake = Fake::default();
        fake.answers.lock().unwrap().push_back(listing("canvas", "Poster"));
        let (fake, s) = server_with(fake, Scope::Workspace);
        ready(&s);
        assert_eq!(run_tool(&s, "design_guide", json!({"artifact_id":"a1"}))["structuredContent"]["guide"], "canvas");
        // The receipt of changes.apply carries the committed revision's non-blocking diagnostics.
        fake.answers.lock().unwrap().push_back(Ok(json!({"revision":2,"diagnostics":[
            {"code":"COLOR_STOCK_DEFAULT","message":"card is filled with #2563EB","node_id":"card","blocking":false,"params":{}},
            {"code":"COLOR_STOCK_DEFAULT","message":"card is filled with #2563EB","node_id":"card","blocking":false,"params":{}},
            {"code":"COLOR_STOCK_DEFAULT","message":"card is filled with #2563EB","node_id":"card","blocking":false,"params":{"artifact_id":"a1"}},
            {"code":"COLOR_STOCK_DEFAULT","message":"card is filled with #2563EB","node_id":"card","blocking":false,"params":{"artifact_id":"a2"}},
            {"code":"COLOR_STOCK_DEFAULT","message":"card is filled with #2563EB","node_id":"card","blocking":false,"params":{"artifact_id":"a3"}},
            {"code":"SOMETHING_ELSE","message":"other","node_id":"x","blocking":false,"params":{}}
        ]})));
        let applied = run_tool(&s, "changes_apply", change_set("a1"));
        let note = applied["content"][1]["text"].as_str().expect("a quality note");
        assert!(note.contains("COLOR_STOCK_DEFAULT on card") && !note.contains("SOMETHING_ELSE"), "{note}");
        assert!(note.contains("revision 2"), "{note}");
        assert_eq!(note.matches("COLOR_STOCK_DEFAULT on card:").count(), 1, "a warning the receipt lists twice is shown once: {note}");
        assert!(note.contains("COLOR_STOCK_DEFAULT on card in artifact a2:") && note.contains("COLOR_STOCK_DEFAULT on card in artifact a3:"), "a token change's warnings in the artifacts that use it name them: {note}");
        assert_eq!(methods(&fake), ["workspace.list", "changes.apply"], "no second validation");
    }

    #[test]
    fn a_change_sent_while_the_guide_is_on_its_way_is_held_until_that_response_is_written() {
        let fake = Fake::default();
        fake.answers.lock().unwrap().push_back(listing("slides", "Deck"));
        let (fake, s) = server_with(fake, Scope::Workspace);
        ready(&s);
        let route = |name: &str, arguments: Value| match s.route(json!({"jsonrpc":"2.0","id":30,"method":"tools/call","params":{"name":name,"arguments":arguments}})) {
            Routed::Call(call) => call,
            other => panic!("{name}: {other:?}"),
        };
        // The first change claims the guide; its response is not written yet.
        let (first, delivered) = s.execute_unacknowledged(&route("changes_apply", change_set("a1")));
        assert_eq!(first["structuredContent"]["code"], GUIDE_REQUIRED);
        assert_eq!(delivered, ["slides"]);
        let (second, none) = s.execute_unacknowledged(&route("changes_apply", change_set("a1")));
        assert_eq!(second["structuredContent"]["code"], GUIDE_REQUIRED, "a parallel change is held too");
        assert!(second["content"][0]["text"].as_str().unwrap().contains("another response"), "{second}");
        assert!(none.is_empty(), "the held call carries no guide of its own");
        s.acknowledge(&delivered);
        assert_eq!(run_tool(&s, "changes_apply", change_set("a1"))["isError"], false, "after the write the change passes");
        assert_eq!(methods(&fake), ["workspace.list", "changes.apply"], "one listing, one apply");
    }

    #[test]
    fn a_guide_whose_response_could_not_be_written_is_delivered_again() {
        let fake = Fake::default();
        fake.answers.lock().unwrap().push_back(listing("slides", "Deck"));
        let (_fake, s) = server_with(fake, Scope::Workspace);
        ready(&s);
        let Routed::Call(call) = s.route(json!({"jsonrpc":"2.0","id":31,"method":"tools/call","params":{"name":"changes_apply","arguments":change_set("a1")}}))
        else { panic!("a call") };
        let (first, delivered) = s.execute_unacknowledged(&call);
        assert_eq!(first["structuredContent"]["code"], GUIDE_REQUIRED);
        let failed = s.write_delivering(&delivered, || Err(io::Error::new(io::ErrorKind::BrokenPipe, "gone")));
        assert!(failed.is_err());
        let again = run_tool(&s, "changes_apply", change_set("a1"));
        assert_eq!(again["structuredContent"]["code"], GUIDE_REQUIRED);
        assert!(again["content"][0]["text"].as_str().unwrap().contains("manual/slides"), "the full guide again, not the pending note: {again}");
        assert_eq!(run_tool(&s, "changes_apply", change_set("a1"))["isError"], false, "delivered by the in-process call");
    }

    #[test]
    fn a_failed_lookup_refuses_the_change_and_design_guide_tells_failure_from_absence() {
        let fake = Fake::default();
        fake.answers.lock().unwrap().push_back(Err(DltError::new(ErrorCode::HostNotRunning, "the host is not running")));
        fake.answers.lock().unwrap().push_back(Err(DltError::new(ErrorCode::HostNotRunning, "the host is not running")));
        fake.answers.lock().unwrap().push_back(listing("canvas", "Poster"));
        let (fake, s) = server_with(fake, Scope::Workspace);
        ready(&s);
        let refused = run_tool(&s, "changes_apply", change_set("a1"));
        assert_eq!(refused["isError"], true);
        assert_eq!(refused["structuredContent"]["code"], "HOST_NOT_RUNNING", "{refused}");
        assert!(refused["content"][0]["text"].as_str().unwrap().contains("nothing was applied"));
        assert_eq!(methods(&fake), ["workspace.list"], "the change never reached the core");
        let guide = run_tool(&s, "design_guide", json!({"artifact_id":"a1"}));
        assert_eq!(guide["structuredContent"]["code"], "HOST_NOT_RUNNING", "a failed listing is not NOT_FOUND: {guide}");
        let missing = run_tool(&s, "design_guide", json!({"artifact_id":"nope"}));
        assert_eq!(missing["structuredContent"]["code"], "NOT_FOUND", "{missing}");
    }

    #[test]
    fn a_topic_page_alone_does_not_unlock_changes_and_both_arguments_give_both() {
        let fake = Fake::default();
        fake.answers.lock().unwrap().push_back(listing("slides", "Deck"));
        let (_fake, s) = server_with(fake, Scope::Workspace);
        ready(&s);
        assert_eq!(run_tool(&s, "design_guide", json!({"topic":"slides"}))["isError"], false);
        assert_eq!(run_tool(&s, "changes_apply", change_set("a1"))["structuredContent"]["code"], GUIDE_REQUIRED, "reference reading is not the artifact's guide");
        let both = run_tool(&s, "design_guide", json!({"artifact_id":"a1","topic":"glossary"}));
        assert_eq!(both["structuredContent"]["guide"], "slides");
        let text = both["content"][1]["text"].as_str().unwrap();
        assert!(text.contains("manual/slides") && text.matches("Design glossary").count() >= 2, "the artifact guide, then the topic");
    }

    #[test]
    fn a_delivered_guide_unlocks_every_artifact_that_takes_the_same_guide_and_no_other() {
        let fake = Fake::default();
        fake.answers.lock().unwrap().push_back(Ok(json!({"workspace_id":"w","artifacts":[
            {"id":"a1","kind":"slides","title":"Deck A","revision":1,"engine_id":"e","dirty":false},
            {"id":"a2","kind":"slides","title":"Deck B","revision":1,"engine_id":"e","dirty":false},
            {"id":"a3","kind":"canvas","title":"Transformer 구조도","revision":1,"engine_id":"e","dirty":false}]})));
        let (_fake, s) = server_with(fake, Scope::Workspace);
        ready(&s);
        assert_eq!(run_tool(&s, "design_guide", json!({"artifact_id":"a1"}))["structuredContent"]["guide"], "slides");
        assert_eq!(run_tool(&s, "changes_apply", change_set("a2"))["isError"], false, "deck B takes the same guide, already delivered");
        let other = run_tool(&s, "changes_apply", change_set("a3"));
        assert_eq!(other["structuredContent"]["code"], GUIDE_REQUIRED, "a paper diagram takes another guide: {other}");
    }

    #[test]
    fn a_change_waits_while_its_guide_is_being_written_and_passes_once_the_write_is_done() {
        let fake = Fake::default();
        fake.answers.lock().unwrap().push_back(listing("slides", "Deck"));
        let (_fake, s) = server_with(fake, Scope::Workspace);
        let s = Arc::new(s);
        ready(&s);
        let Routed::Call(call) = s.route(json!({"jsonrpc":"2.0","id":32,"method":"tools/call","params":{"name":"changes_apply","arguments":change_set("a1")}}))
        else { panic!("a call") };
        let (first, delivered) = s.execute_unacknowledged(&call);
        assert_eq!(first["structuredContent"]["code"], GUIDE_REQUIRED);
        let (writing_tx, writing_rx) = std::sync::mpsc::channel::<()>();
        let (finish_tx, finish_rx) = std::sync::mpsc::channel::<()>();
        let writer = {
            let s = Arc::clone(&s);
            thread::spawn(move || s.write_delivering(&delivered, || { writing_tx.send(()).unwrap(); finish_rx.recv().unwrap(); Ok(()) }))
        };
        writing_rx.recv().unwrap();
        let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let second = {
            let (s, done) = (Arc::clone(&s), Arc::clone(&done));
            thread::spawn(move || { let reply = run_tool(&s, "changes_apply", change_set("a1")); done.store(true, std::sync::atomic::Ordering::SeqCst); reply })
        };
        thread::sleep(Duration::from_millis(150));
        assert!(!done.load(std::sync::atomic::Ordering::SeqCst), "the change waits for the guide's write instead of being answered meanwhile");
        finish_tx.send(()).unwrap();
        writer.join().unwrap().unwrap();
        let reply = second.join().unwrap();
        assert_eq!(reply["isError"], false, "the write recorded the guide before the waiting change looked: {reply}");
    }

    fn update_info() -> crate::update_notice::UpdateInfo {
        crate::update_notice::UpdateInfo {
            latest: "0.2.0".into(),
            current: "0.1.1".into(),
            html_url: "https://github.com/KIMY0UNGGON/DesignLLMTool/releases/tag/v0.2.0".into(),
        }
    }

    fn notices(result: &Value) -> usize {
        result["content"].as_array().unwrap().iter().filter(|b| b["text"].as_str().is_some_and(|t| t.contains("is available (this is"))).count()
    }

    #[test]
    fn the_update_notice_text_names_both_versions_the_steps_and_the_page() {
        let text = update_info().notice_text();
        assert_eq!(
            text,
            "DesignLLMTool 0.2.0 is available (this is 0.1.1). To update: in the DesignLLMTool clone run \"git pull\" then \"./install.sh\" (macOS/Linux) or \"powershell -ExecutionPolicy Bypass -File .\\install.ps1\" (Windows), and restart the AI client. https://github.com/KIMY0UNGGON/DesignLLMTool/releases/tag/v0.2.0"
        );
    }

    #[test]
    fn a_known_newer_release_is_told_once_per_connection_on_the_first_successful_result() {
        let slot = crate::update_notice::UpdateSlot::default();
        let (_fake, s) = server_with(Fake::default(), Scope::Workspace);
        let s = s.with_update_notice(slot.clone());
        ready(&s);
        let before = run_tool(&s, "workspace_list", json!({}));
        assert_eq!(notices(&before), 0, "nothing is known yet: {before}");
        assert!(before["structuredContent"].get("update_available").is_none());

        slot.set(update_info()).unwrap();
        let refused = run_tool(&s, "design_guide", json!({"topic":"no-such-topic"}));
        assert_eq!(refused["isError"], true);
        assert_eq!(notices(&refused), 0, "an error result does not carry the notice: {refused}");

        let first = run_tool(&s, "workspace_list", json!({}));
        assert_eq!(first["isError"], false);
        assert_eq!(notices(&first), 1, "{first}");
        assert_eq!(first["structuredContent"]["update_available"]["latest_version"], "0.2.0");
        assert_eq!(first["structuredContent"]["update_available"]["current_version"], "0.1.1");
        assert_eq!(first["content"][0]["text"], json!({"ok": true}).to_string(), "the result's own text block is unchanged");

        let second = run_tool(&s, "workspace_list", json!({}));
        assert_eq!(notices(&second), 0, "told once per connection: {second}");
        assert!(second["structuredContent"].get("update_available").is_none());

        // A new connection (a new server over the same slot) tells it again, once.
        let (_fake, other) = server_with(Fake::default(), Scope::Workspace);
        let other = other.with_update_notice(slot);
        ready(&other);
        assert_eq!(notices(&run_tool(&other, "workspace_list", json!({}))), 1);
        assert_eq!(notices(&run_tool(&other, "workspace_list", json!({}))), 0);
    }

    #[test]
    fn concurrent_calls_tell_the_update_once() {
        let slot = crate::update_notice::UpdateSlot::default();
        slot.set(update_info()).unwrap();
        let (_fake, s) = server_with(Fake::default(), Scope::Workspace);
        let s = Arc::new(s.with_update_notice(slot));
        ready(&s);
        let workers: Vec<_> = (0..8).map(|_| { let s = Arc::clone(&s); thread::spawn(move || notices(&run_tool(&s, "workspace_list", json!({})))) }).collect();
        let told: usize = workers.into_iter().map(|w| w.join().unwrap()).sum();
        assert_eq!(told, 1);
    }

    #[test]
    fn a_server_without_an_update_slot_never_adds_a_notice() {
        let (_fake, s) = server_with(Fake::default(), Scope::Workspace);
        ready(&s);
        let result = run_tool(&s, "workspace_list", json!({}));
        assert_eq!(result["content"].as_array().unwrap().len(), 1);
    }
}
