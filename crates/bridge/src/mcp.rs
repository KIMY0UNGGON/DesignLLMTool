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
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use designllm_contracts::{DltError, DltResult, ErrorCode, JobRef};
use serde_json::{json, Value};

use crate::assets::{base64_encode, fetch_asset};
use crate::catalog::{find, tools_for, validate_arguments, Post, Scope, ToolSpec};
use crate::client::{Client, ClientRole};
use crate::manual;

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

/// Protocol state for one stdio session.
pub struct McpServer {
    backend: Arc<dyn ToolBackend>,
    tools: Vec<ToolSpec>,
    phase: Mutex<Phase>,
    permits: Permits,
    /// The connection of an app job: `initialize` reports the MCP client to the job (`job.set_client`).
    job_bound: bool,
}

impl McpServer {
    /// Server exposing the tools of `scope`. A job-bound scope makes this the job's own connection.
    pub fn new(backend: Arc<dyn ToolBackend>, scope: Scope) -> Self {
        let mut server = Self::with_tools(backend, tools_for(scope));
        server.job_bound = scope == Scope::JobBound;
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
        }
    }

    /// Parses one line and routes it.
    pub fn route_line(&self, line: &str) -> Routed {
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
                let tools: Vec<Value> = self.tools.iter().map(ToolSpec::to_mcp).collect();
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
                "instructions": "Edit DesignLLMTool artifacts through these tools. Every change is a ChangeSet with request_id, base_revision, and allowed_intents; repeat the same request_id after a timeout instead of sending a new one. In this build some v1.2 fields and operations are refused with UNSUPPORTED_OPERATION; read artifact_validate's capability report before using them. Usage manuals for each task kind are MCP resources (designllm://manual/index) and prompts.",
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
        let arguments = match params.get("arguments") {
            None | Some(Value::Null) => json!({}),
            Some(value) => value.clone(),
        };
        // Argument-schema failures are tool-execution errors (2025-11-25 §Error Handling): the model sees them
        // in the result and can correct the call. Unknown tools stay protocol errors (-32602) above.
        if let Err(message) = validate_arguments(&(tool.schema)(), &arguments) {
            return Routed::Reply(result_reply(id, tool_failure_text(&format!("{}: {message}", tool.name))));
        }
        Routed::Call(PendingCall { id, tool, arguments })
    }

    /// Runs a call and returns the MCP `CallToolResult`. Core failures become `isError: true` results.
    pub fn execute(&self, call: &PendingCall) -> Value {
        self.permits.acquire();
        let result = match self.run(call) {
            Ok(result) => result,
            Err(error) => tool_failure(&error),
        };
        self.permits.release();
        result
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

    fn run(&self, call: &PendingCall) -> DltResult<Value> {
        // An operation hidden from MCP sessions is refused before the core is asked (operation_exposure).
        if let Some(refusal) = crate::operation_exposure::refusal(call.tool.method, &call.arguments) { return Err(refusal); }
        // The host refuses this too (non-gui connections); kept because a server over an in-process backend has no host.
        if let Some(refusal) = crate::review_gate::agent_refusal(call.tool.method, &call.arguments) { return Err(refusal); }
        match call.tool.post {
            Post::None => {
                let value = self.backend.call(call.tool.method, call.arguments.clone())?;
                Ok(tool_success(value, Vec::new()))
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
        if line.trim().is_empty() {
            continue;
        }
        match server.route_line(&line) {
            Routed::Reply(value) => write_message(&out, &value)?,
            Routed::Nothing => {}
            Routed::Call(call) => {
                let server = Arc::clone(&server);
                let out = Arc::clone(&out);
                thread::spawn(move || {
                    let result = server.execute(&call);
                    let _ = write_message(&out, &json!({ "jsonrpc": "2.0", "id": call.id, "result": result }));
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
    let (scope, client) = match bound_job {
        Some(job) => (Scope::JobBound, Client::with_job(workspace, job)),
        None => (Scope::Workspace, Client::with_role(workspace, ClientRole::Bridge)),
    };
    let server = Arc::new(McpServer::new(Arc::new(ClientBackend::new(client)), scope));
    let stdin = io::stdin();
    serve_stdio(stdin.lock(), io::stdout(), server).map_err(DltError::from)
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
}
