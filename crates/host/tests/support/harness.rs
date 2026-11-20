//! Test-only executable for the host integration tests. Not a product binary.
//!
//! It uses the same argument shapes as the product CLI (`--host`, `--mcp`), so the tests run the code path
//! that root's application will run:
//!
//! - `--host --workspace DIR [--headless] [--test-engine] [--startup-ms N] [--idle-ms N] [--client-idle-ms N]
//!    [--tick-ms N] [--job-ms N] [--parallel N] [--gui-exe PATH]...`: serves the workspace (managed unless
//!   `--headless`). Prints `READY {json}` once the pipe is listening. `--test-engine` registers the test canvas
//!   engine. `--gui-exe PATH` (repeatable) lets PATH hold the gui role; the tests pass their own executable, which
//!   stands in for the desktop app.
//! - `--mcp --workspace DIR [--job-id J --attempt-id A]`: attach-only MCP stdio bridge.
//! - `client --workspace DIR --method M [--params JSON] [--role gui|cli|bridge]`: one request, JSON result.
//! - `mock-agent --model M --effort E --mcp-config JSON` (prompt on stdin): a scripted agent. It speaks MCP
//!   to the bridge named in its config and never calls a model. The script is the line `SCRIPT: name`. It first
//!   prints `PROMPT <line>`, the prompt's `Target node ids:` line, so a test can see the nodes it was given.
//!   Job identity (job id, attempt id, artifact id, base revision, allowed intents) is read from the host's
//!   preamble, the same text a real agent receives. `set-text` always calls changes_apply; `set-text-as-told`
//!   follows the prompt's delivery rule and calls proposal_create for a proposal job.
//! - `--version`, `--help`, `--print ...`: a fake Claude Code for `DESIGNLLM_CLAUDE_PATH`. `--help` lists every
//!   flag the adapter requires and the effort list. `--print` records start and end in the file named by
//!   `DESIGNLLM_FAKE_CLAUDE_LOG`, holds for `DESIGNLLM_FAKE_CLAUDE_HOLD_MS`, prints a stream-json init line
//!   (`DESIGNLLM_FAKE_CLAUDE_REPORT_MODEL`, `DESIGNLLM_FAKE_CLAUDE_OMIT_INIT`, `DESIGNLLM_FAKE_CLAUDE_NO_OUTPUT`,
//!   `DESIGNLLM_FAKE_CLAUDE_FLOOD_BYTES` change it), then runs the mock script.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

#[path = "engine.rs"]
mod engine;

use designllm_bridge::{run_stdio, Client, ClientRole};
use designllm_contracts::{
    new_id, AssetStore, DltError, DltResult, EncodedOutput, EngineAdapter, HitTestResult, JobRef, Point, Rect,
    RenderPage, RenderProfile, RenderProvider, RenderScene, ShapedText, Style, TextPosition, TextRange,
};
use designllm_core::Workspace;
use designllm_host::{serve_with, HostMode, HostOptions, HostReady};
use designllm_runtimes::RuntimeConfig;
use serde_json::{json, Value};

/// Renderer for the harness. Every query reports that no renderer is registered.
struct NoRenderer;

fn no_renderer() -> DltError {
    DltError::unsupported("the test harness has no renderer registered")
}

impl RenderProvider for NoRenderer {
    fn shape_text(&self, _text: &str, _style: &Style) -> DltResult<ShapedText> {
        Err(no_renderer())
    }
    fn encode(
        &self,
        _scene: &RenderScene,
        _format: &str,
        _profile: &RenderProfile,
        _assets: &dyn AssetStore,
    ) -> DltResult<EncodedOutput> {
        Err(no_renderer())
    }
    fn font_catalog_fingerprint(&self) -> String {
        "harness-no-renderer".into()
    }
    fn available_fonts(&self) -> Vec<String> {
        Vec::new()
    }
    fn hit_test(&self, _page: &RenderPage, _point: Point) -> DltResult<Option<HitTestResult>> {
        Err(no_renderer())
    }
    fn caret_rect(&self, _page: &RenderPage, _position: &TextPosition) -> DltResult<Rect> {
        Err(no_renderer())
    }
    fn selection_rects(&self, _page: &RenderPage, _range: &TextRange) -> DltResult<Vec<Rect>> {
        Err(no_renderer())
    }
}

/// Parses `--key value` and `--flag` pairs after the mode argument.
struct Args {
    items: Vec<String>,
}

impl Args {
    fn value(&self, key: &str) -> Option<String> {
        let i = self.items.iter().position(|a| a == key)?;
        self.items.get(i + 1).cloned()
    }

    /// Every value given with `key`, in order (`--gui-exe A --gui-exe B`).
    fn values(&self, key: &str) -> Vec<String> {
        self.items.windows(2).filter(|pair| pair[0] == key).map(|pair| pair[1].clone()).collect()
    }

    fn flag(&self, key: &str) -> bool {
        self.items.iter().any(|a| a == key)
    }

    fn number(&self, key: &str, default: u64) -> u64 {
        self.value(key).and_then(|v| v.parse().ok()).unwrap_or(default)
    }

    fn require(&self, key: &str) -> Result<String, String> {
        self.value(key).ok_or_else(|| format!("missing {key}"))
    }
}

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let code = match argv.first().map(String::as_str) {
        Some("--host") => run_host(&Args { items: argv.clone() }),
        Some("--mcp") => run_mcp(&Args { items: argv.clone() }),
        Some("client") => run_client(&Args { items: argv.clone() }),
        Some("mock-agent") => run_mock_agent(&Args { items: argv.clone() }),
        Some("--print") => run_fake_claude(&Args { items: argv.clone() }),
        Some("--version") => {
            println!("{FAKE_CLAUDE_VERSION}");
            0
        }
        Some("--help") => {
            println!("{FAKE_CLAUDE_HELP}");
            0
        }
        _ => {
            eprintln!("usage: designllm-host-harness --host|--mcp|client|mock-agent|--print|--version|--help ...");
            2
        }
    };
    std::process::exit(code);
}

fn run_host(args: &Args) -> i32 {
    let root = match args.require("--workspace") {
        Ok(v) => PathBuf::from(v),
        Err(e) => return fail(2, "usage", &e),
    };
    let engines: Vec<Arc<dyn EngineAdapter>> = if args.flag("--test-engine") {
        vec![Arc::new(engine::TestCanvasEngine)]
    } else {
        Vec::new()
    };
    let workspace = match Workspace::open(&root, engines, Arc::new(NoRenderer)) {
        Ok(ws) => ws,
        Err(e) => {
            let code = if e.code == designllm_contracts::ErrorCode::WorkspaceLocked { 3 } else { 4 };
            return fail(code, "open", &e.to_string());
        }
    };
    let mode = if args.flag("--headless") { HostMode::Headless } else { HostMode::Managed };
    let mut options = HostOptions {
        runtime: RuntimeConfig {
            mock_agent: std::env::current_exe().ok(),
            ..RuntimeConfig::from_env()
        },
        startup_grace: Duration::from_millis(args.number("--startup-ms", 20_000)),
        idle_grace: Duration::from_millis(args.number("--idle-ms", 5_000)),
        client_idle_timeout: Duration::from_millis(args.number("--client-idle-ms", 30_000)),
        tick: Duration::from_millis(args.number("--tick-ms", 100)),
        max_job_duration: Duration::from_millis(args.number("--job-ms", 45 * 60 * 1000)),
        max_parallel_jobs: args.number("--parallel", 2) as usize,
        gui_executables: args.values("--gui-exe").into_iter().map(PathBuf::from).collect(),
        ..HostOptions::default()
    };
    options.on_ready = Some(Arc::new(|ready: &HostReady| {
        let payload = json!({
            "pid": ready.pid,
            "pipe": ready.pipe_name,
            "mode": match ready.mode { HostMode::Headless => "headless", HostMode::Managed => "managed" },
            "workspace": ready.workspace_root.display().to_string(),
        });
        println!("READY {payload}");
        let _ = std::io::stdout().flush();
    }));
    match serve_with(workspace, mode, options) {
        Ok(()) => 0,
        Err(e) => {
            let code = if e.code == designllm_contracts::ErrorCode::WorkspaceLocked { 3 } else { 4 };
            fail(code, "serve", &e.to_string())
        }
    }
}

fn run_mcp(args: &Args) -> i32 {
    let root = match args.require("--workspace") {
        Ok(v) => PathBuf::from(v),
        Err(e) => return fail(2, "usage", &e),
    };
    let bound = match (args.value("--job-id"), args.value("--attempt-id")) {
        (Some(job_id), Some(attempt_id)) => Some(JobRef { job_id, attempt_id }),
        (None, None) => None,
        _ => return fail(2, "usage", "--job-id and --attempt-id must be given together"),
    };
    match run_stdio(&root, bound) {
        Ok(()) => 0,
        Err(e) => fail(4, "mcp", &e.to_string()),
    }
}

fn run_client(args: &Args) -> i32 {
    let root = match args.require("--workspace") {
        Ok(v) => PathBuf::from(v),
        Err(e) => return fail(2, "usage", &e),
    };
    let method = match args.require("--method") {
        Ok(v) => v,
        Err(e) => return fail(2, "usage", &e),
    };
    let params: Value = match args.value("--params") {
        Some(text) => match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(e) => return fail(2, "usage", &format!("--params is not JSON: {e}")),
        },
        None => json!({}),
    };
    let role = match args.value("--role").as_deref() {
        Some("gui") => ClientRole::Gui,
        Some("bridge") => ClientRole::Bridge,
        _ => ClientRole::Cli,
    };
    let client = Client::with_role(&root, role);
    match client.call(&method, params) {
        Ok(value) => {
            println!("{}", json!({ "ok": true, "result": value }));
            0
        }
        Err(e) => {
            println!("{}", json!({ "ok": false, "error": e }));
            1
        }
    }
}

fn fail(code: i32, what: &str, message: &str) -> i32 {
    eprintln!("{what}: {message}");
    code
}

/// Line-oriented JSON-RPC peer over a child process's stdio.
struct McpPeer {
    child: Child,
    stdin: ChildStdin,
    lines: std::io::Lines<BufReader<ChildStdout>>,
    next_id: u64,
}

impl McpPeer {
    fn request(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        let line = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string();
        writeln!(self.stdin, "{line}").expect("write to bridge");
        self.stdin.flush().expect("flush bridge");
        loop {
            let text = self.lines.next().expect("bridge replied").expect("read bridge line");
            let value: Value = serde_json::from_str(&text).expect("bridge reply is JSON");
            if value.get("id") == Some(&json!(id)) {
                return value;
            }
        }
    }

    fn notify(&mut self, method: &str) {
        writeln!(self.stdin, "{}", json!({"jsonrpc": "2.0", "method": method})).expect("notify");
        self.stdin.flush().expect("flush");
    }

    fn call_tool(&mut self, name: &str, arguments: Value) -> Value {
        let reply = self.request("tools/call", json!({"name": name, "arguments": arguments}));
        reply["result"].clone()
    }
}

fn start_mcp_peer(config: &Value) -> Result<McpPeer, String> {
    let server = config
        .get("mcpServers")
        .and_then(|s| s.get("designllm"))
        .ok_or("mcp config has no designllm server")?;
    let command = server["command"].as_str().ok_or("command missing")?;
    let args: Vec<String> = server["args"]
        .as_array()
        .ok_or("args missing")?
        .iter()
        .map(|a| a.as_str().unwrap_or_default().to_string())
        .collect();
    let mut child = Command::new(command)
        .args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("cannot start bridge: {e}"))?;
    let stdin = child.stdin.take().ok_or("no bridge stdin")?;
    let stdout = child.stdout.take().ok_or("no bridge stdout")?;
    let mut peer = McpPeer { child, stdin, lines: BufReader::new(stdout).lines(), next_id: 0 };
    let init = peer.request(
        "initialize",
        json!({"protocolVersion": "2025-11-25", "capabilities": {}, "clientInfo": {"name": "harness", "version": "0"}}),
    );
    if init.get("result").is_none() {
        return Err(format!("initialize failed: {init}"));
    }
    peer.notify("notifications/initialized");
    Ok(peer)
}

/// Facts the host put in the preamble of the prompt.
#[derive(Debug, Default)]
struct Preamble {
    job: String,
    attempt: String,
    artifact: String,
    revision: u64,
    intents: Vec<String>,
}

fn parse_preamble(prompt: &str) -> Preamble {
    let mut out = Preamble::default();
    for line in prompt.lines() {
        if let Some(rest) = line.strip_prefix("Job: ") {
            // "Job: <job>. Attempt: <attempt>."
            if let Some((job, attempt)) = rest.split_once(". Attempt: ") {
                out.job = job.to_string();
                out.attempt = attempt.trim_end_matches('.').to_string();
            }
        } else if let Some(rest) = line.strip_prefix("Artifact: ") {
            // "Artifact: <id>. Base revision: <n>."
            if let Some((id, revision)) = rest.split_once(". Base revision: ") {
                out.artifact = id.to_string();
                out.revision = revision.trim_end_matches('.').parse().unwrap_or(0);
            }
        } else if let Some(rest) = line.strip_prefix("Allowed intents: ") {
            out.intents = rest.trim_end_matches('.').split(", ").map(str::to_string).collect();
        }
    }
    out
}

fn prompt_value(prompt: &str, key: &str) -> Option<String> {
    prompt.lines().find_map(|l| l.strip_prefix(key).map(|v| v.trim().to_string()))
}

/// The `--workspace` value of the bridge the runtime was given.
fn workspace_of(config: &Value) -> Option<String> {
    let args = config["mcpServers"]["designllm"]["args"].as_array()?;
    let at = args.iter().position(|arg| arg.as_str() == Some("--workspace"))?;
    args.get(at + 1)?.as_str().map(str::to_string)
}

/// One line for the outcome of a gui call, as the job progress shows it.
fn gui_answer(answer: DltResult<Value>) -> String {
    match answer {
        Ok(_) => "accepted".to_string(),
        Err(error) => format!("refused: {}", error.message),
    }
}

/// Builds a ChangeSet from the preamble facts. `operations` are contract operations.
fn change_set(facts: &Preamble, workspace_id: &str, operations: Value, intents: &[&str]) -> Value {
    json!({
        "request_id": new_id(),
        "actor": "job",
        "workspace_id": workspace_id,
        "artifact_id": facts.artifact,
        "base_revision": facts.revision,
        "allowed_intents": intents,
        "job_ref": {"job_id": facts.job, "attempt_id": facts.attempt},
        "read_dependencies": [],
        "operations": operations,
    })
}

/// Version line of the fake Claude Code (`claude --version`).
const FAKE_CLAUDE_VERSION: &str = "2.1.0 (fake Claude Code for DesignLLMTool host tests)";

/// Help text of the fake Claude Code: every flag the adapter requires, and the effort list it parses.
const FAKE_CLAUDE_HELP: &str = "Usage: claude [options] [command] [prompt]
Fake help for DesignLLMTool host tests. No model is called.
  -p, --print                       Print response and exit
  --model <model>                   Model for the current session.
  --effort <level>                  Effort level for the current session
(low, medium, high, xhigh, max)
  --output-format <format>          Output format (text, json, stream-json)
  --verbose                         Override verbose mode setting
  --input-format <format>           Input format (text, stream-json)
  --no-session-persistence          Disable session persistence
  --setting-sources <sources>       Setting sources to load (user, project, local)
  --settings <file-or-json>         Settings file or JSON string
  --disable-slash-commands          Disable all slash commands
  --permission-mode <mode>          Permission mode
  --permission-prompts <mode>       Permission prompt handling
  --strict-mcp-config               Use only the MCP servers from --mcp-config
  --mcp-config <configs...>         MCP server configurations (JSON)
  --tools <tools...>                Tools available to the session
  --allowedTools <tools...>         Tools allowed without a prompt";

/// `claude --print`: records start and end, holds when asked to, prints a stream-json init line, then runs the mock
/// script. The init line reports the `--model` value unless DESIGNLLM_FAKE_CLAUDE_REPORT_MODEL is set. Switches:
/// OMIT_INIT prints an assistant line instead of the init line, NO_OUTPUT prints nothing, FLOOD_BYTES prints that
/// many bytes of stream-json after the init line. No model is called.
fn run_fake_claude(args: &Args) -> i32 {
    let log = std::env::var_os("DESIGNLLM_FAKE_CLAUDE_LOG").map(PathBuf::from);
    let record = |event: &str| {
        let Some(path) = &log else {
            return;
        };
        let millis = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
            let _ = writeln!(file, "{event} {} {millis}", std::process::id());
        }
    };
    record("start");
    let hold_ms = std::env::var("DESIGNLLM_FAKE_CLAUDE_HOLD_MS").ok().and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
    if hold_ms > 0 {
        std::thread::sleep(Duration::from_millis(hold_ms));
    }
    let switch = |name: &str| std::env::var_os(name).is_some();
    if switch("DESIGNLLM_FAKE_CLAUDE_NO_OUTPUT") {
        record("end");
        return 0;
    }
    if switch("DESIGNLLM_FAKE_CLAUDE_OMIT_INIT") {
        emit(json!({
            "type": "assistant",
            "message": {"role": "assistant", "content": [{"type": "text", "text": "output before init"}]},
        }));
    } else {
        // The init line reports the requested model unless a test asks for another one.
        let reported = std::env::var("DESIGNLLM_FAKE_CLAUDE_REPORT_MODEL")
            .unwrap_or_else(|_| args.value("--model").unwrap_or_default());
        emit(json!({
            "type": "system",
            "subtype": "init",
            "cwd": "fake",
            "session_id": "fake-session",
            "model": reported,
            "tools": [],
            "mcp_servers": [],
            "permissionMode": "dontAsk",
            "slash_commands": [],
            "apiKeySource": "none",
        }));
    }
    if let Some(total) = std::env::var("DESIGNLLM_FAKE_CLAUDE_FLOOD_BYTES").ok().and_then(|v| v.parse::<usize>().ok()) {
        let pad = "x".repeat(64 * 1024);
        for n in 0..(total / (pad.len() + 64)) {
            emit(json!({"type": "stream_event", "n": n, "pad": pad.clone()}));
        }
    }
    let code = run_mock_agent(args);
    record("end");
    code
}

/// Prints one stream-json line and flushes it.
fn emit(value: serde_json::Value) {
    println!("{value}");
    let _ = std::io::stdout().flush();
}

fn run_mock_agent(args: &Args) -> i32 {
    let config_text = match args.require("--mcp-config") {
        Ok(v) => v,
        Err(e) => return fail(2, "usage", &e),
    };
    let model = args.value("--model").unwrap_or_default();
    let effort = args.value("--effort").unwrap_or_default();
    let mut prompt = String::new();
    let _ = std::io::stdin().read_to_string(&mut prompt);
    let targets = prompt.lines().find(|l| l.starts_with("Target node ids: ")).unwrap_or("Target node ids: missing");
    println!("PROMPT {targets}");
    let _ = std::io::stdout().flush();
    let script = prompt
        .lines()
        .find_map(|l| l.strip_prefix("SCRIPT:").map(|s| s.trim().to_string()))
        .unwrap_or_else(|| "none".to_string());
    println!("MOCK pid={} model={model} effort={effort} script={script}", std::process::id());
    let _ = std::io::stdout().flush();

    let config: Value = match serde_json::from_str(&config_text) {
        Ok(v) => v,
        Err(e) => return fail(2, "mcp-config", &e.to_string()),
    };
    if script == "exit-3" {
        return 3;
    }
    if script == "sleep-tree" {
        // A grandchild of the runtime: it must end with the job, and nothing else may be touched.
        let grandchild = Command::new("cmd.exe")
            .args(["/d", "/c", "ping -n 120 127.0.0.1 > NUL"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        match grandchild {
            Ok(mut child) => {
                println!("GRANDCHILD {}", child.id());
                let _ = std::io::stdout().flush();
                let _ = child.wait();
            }
            Err(e) => return fail(4, "spawn", &e.to_string()),
        }
        return 0;
    }

    let mut peer = match start_mcp_peer(&config) {
        Ok(p) => p,
        Err(e) => return fail(4, "mcp", &e),
    };
    let facts = parse_preamble(&prompt);
    let text = prompt_value(&prompt, "TEXT:").unwrap_or_else(|| "Changed by agent".to_string());
    let node = prompt_value(&prompt, "NODE:").unwrap_or_else(|| "node-title".to_string());
    let outcome = match script.as_str() {
        "tools" => {
            let list = peer.request("tools/list", json!({}));
            let count = list["result"]["tools"].as_array().map(Vec::len).unwrap_or(0);
            println!("TOOLS {count}");
            0
        }
        "workspace-list" => {
            let result = peer.call_tool("workspace_list", json!({}));
            println!("RESULT {result}");
            0
        }
        "set-text" | "set-text-late" => {
            if script == "set-text-late" {
                // Written after the host has had time to cancel: the core must refuse it with CANCELED.
                println!("READY-LATE");
                let _ = std::io::stdout().flush();
                std::thread::sleep(Duration::from_millis(2500));
            }
            let listed = peer.call_tool("workspace_list", json!({}));
            let workspace_id = listed["structuredContent"]["workspace_id"].as_str().unwrap_or("").to_string();
            let operations = json!([{"op": "set_text", "node_id": node, "text": text}]);
            let set = change_set(&facts, &workspace_id, operations, &["content"]);
            let result = peer.call_tool("changes_apply", set);
            println!("RESULT {result}");
            0
        }
        "set-text-as-told" => {
            // Follows the prompt's delivery rule: a proposal job sends the change with proposal_create, not changes_apply.
            let proposal = prompt.lines().any(|l| l.starts_with("- This job delivers a proposal") && l.contains("then proposal_create"));
            let tool = if proposal { "proposal_create" } else { "changes_apply" };
            let listed = peer.call_tool("workspace_list", json!({}));
            let workspace_id = listed["structuredContent"]["workspace_id"].as_str().unwrap_or("").to_string();
            let operations = json!([{"op": "set_text", "node_id": node, "text": text}]);
            let set = change_set(&facts, &workspace_id, operations, &["content"]);
            let arguments = if proposal { json!({"change_set": set}) } else { set };
            println!("TOOL {tool}");
            let result = peer.call_tool(tool, arguments);
            println!("RESULT {result}");
            0
        }
        "set-style" => {
            let listed = peer.call_tool("workspace_list", json!({}));
            let workspace_id = listed["structuredContent"]["workspace_id"].as_str().unwrap_or("").to_string();
            let operations = json!([{"op": "set_style", "node_ids": [node], "patch": {"fill": "#FF0000"}}]);
            let set = change_set(&facts, &workspace_id, operations, &["design"]);
            let result = peer.call_tool("changes_apply", set);
            println!("RESULT {result}");
            0
        }
        "content-under-design" => {
            // A design-only allowance with a content operation: the core must refuse the whole change.
            let listed = peer.call_tool("workspace_list", json!({}));
            let workspace_id = listed["structuredContent"]["workspace_id"].as_str().unwrap_or("").to_string();
            let operations = json!([{"op": "set_text", "node_id": node, "text": text}]);
            let set = change_set(&facts, &workspace_id, operations, &["design"]);
            let result = peer.call_tool("changes_apply", set);
            println!("RESULT {result}");
            0
        }
        s if s.starts_with("call:") => {
            // `SCRIPT: call:<tool>` followed by `ARGS: {json}` on its own line.
            let tool = &s["call:".len()..];
            let arguments = prompt_value(&prompt, "ARGS:")
                .and_then(|a| serde_json::from_str::<Value>(&a).ok())
                .unwrap_or(json!({}));
            let reply = peer.request("tools/call", json!({"name": tool, "arguments": arguments}));
            match reply.get("error") {
                // A tool this connection does not list is a protocol error, not a tool result.
                Some(error) => println!("REFUSED {} {}", error["code"], error["message"].as_str().unwrap_or_default()),
                None => println!("RESULT {}", reply["result"]),
            }
            0
        }
        "gui-self" => {
            // The runtime's own process claims the gui role. Its executable is the application's, so only the job
            // membership check can refuse it.
            let workspace = PathBuf::from(workspace_of(&config).unwrap_or_default());
            let answer = Client::with_role(&workspace, ClientRole::Gui)
                .call("ui.state.publish", json!({"view_id": "gui-self", "state": {"screen": "editor"}}));
            println!("GUI-SELF {}", gui_answer(answer));
            0
        }
        "gui-child" => {
            // A child of the runtime runs the same executable with the gui role.
            let workspace = workspace_of(&config).unwrap_or_default();
            let params = json!({"view_id": "gui-child", "state": {"screen": "editor"}}).to_string();
            let spawned = std::env::current_exe().and_then(|program| {
                Command::new(program)
                    .args([
                        "client",
                        "--workspace",
                        workspace.as_str(),
                        "--role",
                        "gui",
                        "--method",
                        "ui.state.publish",
                        "--params",
                        params.as_str(),
                    ])
                    .output()
            });
            match spawned {
                Ok(out) => println!("GUI-CHILD {}", String::from_utf8_lossy(&out.stdout).trim()),
                Err(e) => println!("GUI-CHILD spawn failed: {e}"),
            }
            0
        }
        _ => 0,
    };
    let _ = peer.child.kill();
    let _ = peer.child.wait();
    outcome
}
