//! Runtime discovery and capability detection.
//!
//! Detection runs only `--version` and `--help` (no model call, no login state read). The executable is
//! an explicit path from [`RuntimeConfig`] or the environment, or a `.exe` found on `PATH`. For Codex an
//! npm global install on `PATH` resolves to the native `codex.exe` inside the package (`detect/npm.rs`). A `.cmd`
//! or `.ps1` shim is never run: its quoting rules would let arguments escape the command line.
//!
//! Effort values come from the installed help text for Claude Code. For Codex and the mock agent the
//! values are fixed in this crate and reported with `EffortSource::Documented`, because the help text
//! does not list them.

use std::collections::BTreeSet;
use std::env;
use std::ffi::{OsStr, OsString};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use designllm_contracts::{DltError, DltResult, ErrorCode};

mod npm;

/// Deadline for one `--version` or `--help` run.
pub const DETECT_TIMEOUT: Duration = Duration::from_secs(20);

/// Runtimes this build can launch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeKind {
    /// Claude Code (`claude.exe`), print mode with stream-json output.
    Claude,
    /// Codex CLI (`codex.exe`), `exec` mode.
    Codex,
    /// Test agent that speaks MCP and follows a script given in the prompt. Never a model call.
    Mock,
}

impl RuntimeKind {
    /// Maps `JobStartRequest.runtime` to a kind. Unknown names are an error; nothing falls back.
    pub fn parse(name: &str) -> DltResult<Self> {
        match name {
            "claude" => Ok(RuntimeKind::Claude),
            "codex" => Ok(RuntimeKind::Codex),
            "mock" => Ok(RuntimeKind::Mock),
            other => Err(DltError::unsupported(format!("runtime '{other}' is not supported (claude, codex)"))),
        }
    }

    pub fn wire_name(self) -> &'static str {
        match self {
            RuntimeKind::Claude => "claude",
            RuntimeKind::Codex => "codex",
            RuntimeKind::Mock => "mock",
        }
    }

    fn exe_name(self) -> Option<&'static str> {
        match self {
            RuntimeKind::Claude => Some("claude.exe"),
            RuntimeKind::Codex => Some("codex.exe"),
            RuntimeKind::Mock => None,
        }
    }
}

/// Where each runtime executable comes from. `None` means "search PATH for the .exe".
#[derive(Debug, Clone, Default)]
pub struct RuntimeConfig {
    pub claude_path: Option<PathBuf>,
    pub codex_path: Option<PathBuf>,
    /// Program that implements the mock agent protocol (`designllm-host-harness` in tests).
    pub mock_agent: Option<PathBuf>,
}

impl RuntimeConfig {
    /// Reads `DESIGNLLM_CLAUDE_PATH` and `DESIGNLLM_CODEX_PATH`. The mock agent is never enabled from the
    /// environment: it runs an arbitrary program, so only code that sets `mock_agent` explicitly can use it. The one such
    /// place outside tests is a UI-test launch (task 69): the host takes `--ui-test-mock-agent <designllm-host-harness>`
    /// from its command line, checked by the agent path rules, and only inside a launch with `DESIGNLLM_UI_TEST_INPUT`.
    pub fn from_env() -> Self {
        RuntimeConfig {
            claude_path: env::var_os("DESIGNLLM_CLAUDE_PATH").map(PathBuf::from),
            codex_path: env::var_os("DESIGNLLM_CODEX_PATH").map(PathBuf::from),
            mock_agent: None,
        }
    }
}

/// Where the effort list came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffortSource {
    /// Parsed from the executable's own `--help`.
    Help,
    /// Fixed in this crate (documented values). Not confirmed by a live model call.
    Documented,
}

/// Detected capabilities of one runtime executable.
#[derive(Debug, Clone)]
pub struct RuntimeInfo {
    pub kind: RuntimeKind,
    pub program: PathBuf,
    pub version: String,
    /// Effort values this runtime accepts. Empty means no effort can be requested.
    pub efforts: Vec<String>,
    pub effort_source: EffortSource,
    /// Required flags that were found in `--help`.
    pub flags: BTreeSet<String>,
    /// Environment changes for every run of `program`. Empty except for an npm install of Codex.
    pub env: Vec<EnvChange>,
}

/// One environment change for a runtime process: `Some` sets the variable, `None` removes it.
pub type EnvChange = (&'static str, Option<OsString>);

/// Applies `env` to a command that is about to start.
pub(crate) fn apply_env(command: &mut Command, env: &[EnvChange]) {
    for (key, value) in env {
        match value {
            Some(value) => command.env(key, value),
            None => command.env_remove(key),
        };
    }
}

/// Claude Code flags this adapter relies on. Detection fails when any is missing (no partial launch).
pub const CLAUDE_REQUIRED_FLAGS: &[&str] = &[
    "--print",
    "--model",
    "--effort",
    "--output-format",
    "--verbose",
    "--input-format",
    "--mcp-config",
    "--strict-mcp-config",
    "--tools",
    "--allowedTools",
    "--permission-mode",
    "--permission-prompts",
    "--no-session-persistence",
    "--setting-sources",
    "--settings",
    "--disable-slash-commands",
];

/// Codex CLI flags (under `codex exec`) this adapter relies on.
pub const CODEX_REQUIRED_FLAGS: &[&str] = &[
    "--model",
    "--config",
    "--json",
    "--sandbox",
    "--ephemeral",
    "--skip-git-repo-check",
    "--ignore-user-config",
    "--cd",
];

/// Documented Codex reasoning-effort values (config key `model_reasoning_effort`).
pub const CODEX_EFFORTS: &[&str] = &["minimal", "low", "medium", "high", "xhigh"];

/// Effort values the mock agent accepts (the Claude Code scale).
pub const MOCK_EFFORTS: &[&str] = &["low", "medium", "high", "xhigh", "max"];

/// Claude model IDs used when the CLI cannot be asked: the first four of its list, in its order (Claude Code 2.1.329, 2026-11-20).
pub const CLAUDE_MODELS: &[&str] = &["claude-opus-5-5", "claude-fable-5-2", "claude-sonnet-5-5", "claude-haiku-5-5"];

/// Codex model slugs used when `codex debug models --bundled` fails or lists nothing (checked 2026-11-20, codex-cli 0.170.0).
pub const CODEX_MODELS_FALLBACK: &[&str] = &[
    "gpt-6.2-sol",
    "gpt-6.2-astra",
    "gpt-6-astra",
    "gpt-6.1-sol",
    "gpt-6.1-terra",
    "gpt-6-sol",
    "gpt-6-luna",
    "gpt-5.6-sol",
    "gpt-5.6-terra",
    "gpt-5.6-luna",
    "gpt-5.5",
];

/// Time limit for the Claude `initialize` request.
const CLAUDE_INIT_TIMEOUT: Duration = Duration::from_secs(20);
/// Time limit for `codex debug models --bundled`.
const CODEX_MODELS_TIMEOUT: Duration = Duration::from_secs(5);

/// A model the runner menu offers. `id` is the `--model` value, `display_name` the menu label, `efforts` the levels the
/// model accepts (empty when the list did not say).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelInfo {
    pub id: String,
    pub display_name: String,
    pub efforts: Vec<String>,
}

/// Where a model list came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelSource {
    /// Read from the CLI at the time of the lookup.
    Cli,
    /// The fixed list of this crate: the CLI could not be asked or listed nothing.
    Fallback,
}

/// The models of one runtime and where they came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelList {
    pub models: Vec<ModelInfo>,
    pub source: ModelSource,
}

fn plain_models(ids: &[&str]) -> Vec<ModelInfo> {
    ids.iter().map(|id| ModelInfo { id: id.to_string(), display_name: id.to_string(), efforts: Vec::new() }).collect()
}

/// The fixed list of a runtime: Claude's first four, Codex's bundled eleven, none for the mock agent.
pub fn fallback_models(kind: RuntimeKind) -> Vec<ModelInfo> {
    match kind {
        RuntimeKind::Claude => plain_models(CLAUDE_MODELS),
        RuntimeKind::Codex => plain_models(CODEX_MODELS_FALLBACK),
        RuntimeKind::Mock => Vec::new(),
    }
}

/// The `slug`s of the bundled catalogue's `models[]` whose `visibility` is `list`, by ascending `priority`, with the
/// `display_name` and the `supported_reasoning_levels[].effort` values. Text after the JSON value (a warning line) is not read.
/// `None` when the JSON does not parse or no model is listed.
pub fn parse_codex_models(json: &str) -> Option<Vec<ModelInfo>> {
    let value = serde_json::Deserializer::from_str(json).into_iter::<serde_json::Value>().next()?.ok()?;
    let mut rows: Vec<(i64, ModelInfo)> = value
        .get("models")?
        .as_array()?
        .iter()
        .filter(|model| model.get("visibility").and_then(serde_json::Value::as_str) == Some("list"))
        .filter_map(|model| {
            let id = model.get("slug")?.as_str()?.trim();
            if id.is_empty() {
                return None;
            }
            let priority = model.get("priority").and_then(serde_json::Value::as_i64).unwrap_or(i64::MAX);
            let display_name = model.get("display_name").and_then(serde_json::Value::as_str).unwrap_or(id).to_owned();
            let efforts = model
                .get("supported_reasoning_levels")
                .and_then(serde_json::Value::as_array)
                .map(|levels| levels.iter().filter_map(|level| level.get("effort")?.as_str().map(str::to_owned)).collect())
                .unwrap_or_default();
            Some((priority, ModelInfo { id: id.to_owned(), display_name, efforts }))
        })
        .collect();
    rows.sort_by_key(|(priority, _)| *priority);
    (!rows.is_empty()).then(|| rows.into_iter().map(|(_, model)| model).collect())
}

/// The models of a Claude `control_response` line (`response.response.models[]`). `value == "default"` is left out and
/// repeated `resolvedModel`s are kept once, in the CLI's order. `None` when the line has no usable model.
pub fn parse_claude_models(line: &str) -> Option<Vec<ModelInfo>> {
    let value: serde_json::Value = serde_json::from_str(line).ok()?;
    let models = value.pointer("/response/response/models")?.as_array()?;
    let mut out: Vec<ModelInfo> = Vec::new();
    for model in models {
        if model.get("value").and_then(serde_json::Value::as_str) == Some("default") {
            continue;
        }
        let Some(id) = model.get("resolvedModel").and_then(serde_json::Value::as_str).map(str::trim).filter(|id| !id.is_empty()) else {
            continue;
        };
        if out.iter().any(|known| known.id == id) {
            continue;
        }
        let display_name = model.get("displayName").and_then(serde_json::Value::as_str).unwrap_or(id).to_owned();
        let efforts = model
            .get("supportedEffortLevels")
            .and_then(serde_json::Value::as_array)
            .map(|levels| levels.iter().filter_map(|level| level.as_str().map(str::to_owned)).collect())
            .unwrap_or_default();
        out.push(ModelInfo { id: id.to_owned(), display_name, efforts });
    }
    (!out.is_empty()).then_some(out)
}

/// The model list of `kind` read from the CLI's text (`text`), or `None` when it has no usable model.
fn models_from_text(kind: RuntimeKind, text: &str) -> Option<Vec<ModelInfo>> {
    match kind {
        RuntimeKind::Claude => text.lines().find_map(parse_claude_models),
        RuntimeKind::Codex => parse_codex_models(text).or_else(|| text.find('{').and_then(|start| parse_codex_models(&text[start..]))),
        RuntimeKind::Mock => None,
    }
}

/// The menu list of `kind` for the executable at `program`: what `lookup` reads from the CLI, else the fixed list.
pub(crate) fn list_from_lookup(
    kind: RuntimeKind,
    program: &Path,
    env: &[EnvChange],
    lookup: &dyn Fn(&Path, &[EnvChange]) -> Option<String>,
) -> ModelList {
    match lookup(program, env).and_then(|text| models_from_text(kind, &text)) {
        Some(models) => ModelList { models, source: ModelSource::Cli },
        None => ModelList { models: fallback_models(kind), source: ModelSource::Fallback },
    }
}

/// The runner menu's model list for `kind` under `config`. Asks the CLI (Claude `initialize` control request, Codex
/// `debug models --bundled`) and falls back to the fixed list when the executable cannot be found or asked.
pub fn model_list(kind: RuntimeKind, config: &RuntimeConfig) -> ModelList {
    let lookup: &dyn Fn(&Path, &[EnvChange]) -> Option<String> = match kind {
        RuntimeKind::Claude => &claude_init_text,
        RuntimeKind::Codex => &codex_debug_text,
        RuntimeKind::Mock => return ModelList { models: Vec::new(), source: ModelSource::Fallback },
    };
    let explicit = match kind {
        RuntimeKind::Claude => config.claude_path.as_deref(),
        RuntimeKind::Codex => config.codex_path.as_deref(),
        RuntimeKind::Mock => None,
    };
    match resolve(kind, explicit) {
        Ok((program, env)) => list_from_lookup(kind, &program, &env, lookup),
        Err(_) => ModelList { models: fallback_models(kind), source: ModelSource::Fallback },
    }
}

/// The `initialize` request sent to Claude Code, and the id its answer carries.
const CLAUDE_INIT_REQUEST: &str = r#"{"type":"control_request","request_id":"r1","request":{"subtype":"initialize"}}"#;
const CLAUDE_INIT_REQUEST_ID: &str = "r1";

/// True for the `control_response` line that answers [`CLAUDE_INIT_REQUEST`], however its JSON is spaced.
fn is_claude_init_answer(line: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(line).is_ok_and(|value| {
        value.get("type").and_then(serde_json::Value::as_str) == Some("control_response")
            && value.pointer("/response/request_id").and_then(serde_json::Value::as_str) == Some(CLAUDE_INIT_REQUEST_ID)
    })
}

/// Sends the Claude `initialize` control request (no user message, so no model call) and returns its `control_response`
/// line. Stdin is closed after the answer; the process is ended if it does not exit soon after. On Windows the process runs
/// in a kill-on-close job object (`win.rs`), so ending it also ends the processes it started.
fn claude_init_text(program: &Path, env: &[EnvChange]) -> Option<String> {
    const ARGS: [&str; 6] = ["-p", "--input-format", "stream-json", "--output-format", "stream-json", "--verbose"];
    #[cfg(windows)]
    let (mut child, job) = {
        let args: Vec<String> = ARGS.iter().map(|arg| arg.to_string()).collect();
        let (mut child, job) = crate::win::spawn_suspended_in_job_with_env(program, &args, &std::env::temp_dir(), env).ok()?;
        if crate::win::resume_child(&child).is_err() {
            let _ = job.terminate(1);
            let _ = child.wait();
            return None;
        }
        // The job's stderr is a pipe: it is read to the end so that the child never waits on a full pipe.
        let stderr = child.stderr.take();
        thread::spawn(move || read_all(stderr));
        (child, job)
    };
    #[cfg(not(windows))]
    let mut child = {
        let mut command = Command::new(program);
        command.args(ARGS).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
        apply_env(&mut command, env);
        command.spawn().ok()?
    };
    let end = move |child: &mut std::process::Child| {
        #[cfg(windows)]
        let _ = job.terminate(1);
        #[cfg(not(windows))]
        let _ = child.kill();
        let _ = child.wait();
    };
    let (Some(mut stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
        end(&mut child);
        return None;
    };
    let _ = writeln!(stdin, "{CLAUDE_INIT_REQUEST}");
    let _ = stdin.flush();
    let (sender, receiver) = std::sync::mpsc::channel();
    thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if is_claude_init_answer(&line) {
                let _ = sender.send(line);
                break;
            }
        }
    });
    let answer = receiver.recv_timeout(CLAUDE_INIT_TIMEOUT).ok();
    drop(stdin);
    let deadline = Instant::now() + Duration::from_secs(5);
    while matches!(child.try_wait(), Ok(None)) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(50));
    }
    // Ends the process if it is still there, and on Windows whatever it left running.
    end(&mut child);
    answer
}

/// The standard output of `codex debug models --bundled` (5 s, no window), `None` when the command fails. Its stderr (a
/// warning) is not part of the JSON.
fn codex_debug_text(program: &Path, env: &[EnvChange]) -> Option<String> {
    match run_capture_streams(program, &["debug", "models", "--bundled"], env, CODEX_MODELS_TIMEOUT) {
        Ok((0, stdout, _)) => Some(stdout),
        _ => None,
    }
}

/// Detects one runtime. Errors name the missing piece; nothing is silently substituted.
pub fn detect(kind: RuntimeKind, config: &RuntimeConfig) -> DltResult<RuntimeInfo> {
    match kind {
        RuntimeKind::Claude => detect_claude(config),
        RuntimeKind::Codex => detect_codex(config),
        RuntimeKind::Mock => detect_mock(config),
    }
}

fn detect_claude(config: &RuntimeConfig) -> DltResult<RuntimeInfo> {
    let (program, env) = resolve(RuntimeKind::Claude, config.claude_path.as_deref())?;
    let version = first_line(&run_checked(&program, &["--version"], &env)?);
    let help = run_checked(&program, &["--help"], &env)?;
    let missing = missing_flags(&help, CLAUDE_REQUIRED_FLAGS);
    if !missing.is_empty() {
        return Err(DltError::unsupported(format!(
            "claude at {} lacks required flags: {}",
            program.display(),
            missing.join(", ")
        )));
    }
    let efforts = parse_claude_efforts(&help);
    if efforts.is_empty() {
        return Err(DltError::unsupported("claude --help does not list effort levels; effort cannot be requested"));
    }
    Ok(RuntimeInfo {
        kind: RuntimeKind::Claude,
        program,
        version,
        efforts,
        effort_source: EffortSource::Help,
        flags: CLAUDE_REQUIRED_FLAGS.iter().map(|s| s.to_string()).collect(),
        env,
    })
}

fn detect_codex(config: &RuntimeConfig) -> DltResult<RuntimeInfo> {
    let (program, env) = resolve(RuntimeKind::Codex, config.codex_path.as_deref())?;
    let version = first_line(&run_checked(&program, &["--version"], &env)?);
    let help = run_checked(&program, &["exec", "--help"], &env)?;
    let missing = missing_flags(&help, CODEX_REQUIRED_FLAGS);
    if !missing.is_empty() {
        return Err(DltError::unsupported(format!(
            "codex exec at {} lacks required flags: {}",
            program.display(),
            missing.join(", ")
        )));
    }
    Ok(RuntimeInfo {
        kind: RuntimeKind::Codex,
        program,
        version,
        efforts: CODEX_EFFORTS.iter().map(|s| s.to_string()).collect(),
        effort_source: EffortSource::Documented,
        flags: CODEX_REQUIRED_FLAGS.iter().map(|s| s.to_string()).collect(),
        env,
    })
}

fn detect_mock(config: &RuntimeConfig) -> DltResult<RuntimeInfo> {
    let program = config
        .mock_agent
        .clone()
        .ok_or_else(|| DltError::unsupported("the mock runtime needs RuntimeConfig.mock_agent"))?;
    if !program.is_file() {
        return Err(DltError::unsupported(format!("mock agent not found: {}", program.display())));
    }
    Ok(RuntimeInfo {
        kind: RuntimeKind::Mock,
        program,
        version: "mock-agent".to_string(),
        efforts: MOCK_EFFORTS.iter().map(|s| s.to_string()).collect(),
        effort_source: EffortSource::Documented,
        flags: BTreeSet::new(),
        env: Vec::new(),
    })
}

/// Explicit path first, then PATH ([`search_path`]). Returns the program and its environment changes.
fn resolve(kind: RuntimeKind, explicit: Option<&Path>) -> DltResult<(PathBuf, Vec<EnvChange>)> {
    if let Some(path) = explicit {
        return if path.is_file() {
            Ok((path.to_path_buf(), Vec::new()))
        } else {
            Err(DltError::unsupported(format!("{} is not a file: {}", kind.wire_name(), path.display())))
        };
    }
    let exe = kind.exe_name().ok_or_else(|| DltError::unsupported("no executable for this runtime"))?;
    search_path(kind, &env::var_os("PATH").unwrap_or_default()).ok_or_else(|| not_found(kind, exe))
}

fn not_found(kind: RuntimeKind, exe: &str) -> DltError {
    let npm = if kind == RuntimeKind::Codex { " (directly or as an npm install of @openai/codex)" } else { "" };
    DltError::unsupported(format!(
        "{exe} was not found on PATH{npm}; set DESIGNLLM_{}_PATH to the executable",
        kind.wire_name().to_uppercase()
    ))
}

/// Finds `name` on PATH. Only real files count; shims (.cmd/.ps1) are not searched.
pub fn find_on_path(name: &str) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    let found = path_dirs(&path).map(|dir| dir.join(name)).find(|candidate| candidate.is_file());
    found
}

/// The directories of a PATH value. Empty and relative entries are skipped: they would resolve against the
/// current directory, which is not where a runtime is installed.
fn path_dirs(path: &OsStr) -> impl Iterator<Item = PathBuf> + '_ {
    env::split_paths(path).filter(|dir| dir.is_absolute())
}

/// The executable for `kind` from the directories of a PATH value, in PATH order. In each directory the
/// `.exe` comes first; for Codex an npm global install there resolves to its native `codex.exe` with the
/// environment `bin/codex.js` gives it. A `.exe` found directly runs with no environment changes.
fn search_path(kind: RuntimeKind, path: &OsStr) -> Option<(PathBuf, Vec<EnvChange>)> {
    let exe = kind.exe_name()?;
    path_dirs(path).find_map(|dir| {
        let candidate = dir.join(exe);
        if candidate.is_file() {
            Some((candidate, Vec::new()))
        } else if kind == RuntimeKind::Codex {
            npm::codex_native(&dir)
        } else {
            None
        }
    })
}

fn run_checked(program: &Path, args: &[&str], env: &[EnvChange]) -> DltResult<String> {
    let (code, text) = run_capture(program, args, env, DETECT_TIMEOUT)?;
    if code != 0 {
        return Err(DltError::unsupported(format!(
            "{} {} exited with code {code}",
            program.display(),
            args.join(" ")
        )));
    }
    Ok(text)
}

/// Runs a program with a deadline and returns (exit code, stdout+stderr). Output is read on helper threads,
/// so a verbose child cannot block on a full pipe.
pub(crate) fn run_capture(
    program: &Path,
    args: &[&str],
    env: &[EnvChange],
    timeout: Duration,
) -> DltResult<(i32, String)> {
    let (code, mut text, err) = run_capture_streams(program, args, env, timeout)?;
    text.push_str(&err);
    Ok((code, text))
}

/// [`run_capture`] with stdout and stderr apart: (exit code, stdout, stderr).
fn run_capture_streams(
    program: &Path,
    args: &[&str],
    env: &[EnvChange],
    timeout: Duration,
) -> DltResult<(i32, String, String)> {
    let mut command = Command::new(program);
    command.args(args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    apply_env(&mut command, env);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(crate::win::CREATE_NO_WINDOW);
    }
    let mut child = command
        .spawn()
        .map_err(|e| DltError::unsupported(format!("cannot run {}: {e}", program.display())))?;
    let out = child.stdout.take();
    let err = child.stderr.take();
    let out_reader = thread::spawn(move || read_all(out));
    let err_reader = thread::spawn(move || read_all(err));
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait()? {
            Some(status) => break status,
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(DltError::new(
                    ErrorCode::Io,
                    format!("{} did not finish within {} s", program.display(), timeout.as_secs()),
                ));
            }
            None => thread::sleep(Duration::from_millis(20)),
        }
    };
    let out = out_reader.join().unwrap_or_default();
    let err = err_reader.join().unwrap_or_default();
    Ok((status.code().unwrap_or(-1), out, err))
}

fn read_all<R: Read>(stream: Option<R>) -> String {
    let mut text = String::new();
    if let Some(mut s) = stream {
        let mut bytes = Vec::new();
        let _ = s.read_to_end(&mut bytes);
        text.push_str(&String::from_utf8_lossy(&bytes));
    }
    text
}

fn first_line(text: &str) -> String {
    text.lines().next().unwrap_or("").trim().to_string()
}

/// Flags from `required` that do not appear in `help`.
pub fn missing_flags(help: &str, required: &[&str]) -> Vec<String> {
    required.iter().filter(|flag| !help.contains(*flag)).map(|s| s.to_string()).collect()
}

/// Effort list from the parenthetical after `--effort <level>` in Claude Code's help.
pub fn parse_claude_efforts(help: &str) -> Vec<String> {
    let Some(pos) = help.find("--effort <level>") else {
        return Vec::new();
    };
    let rest = &help[pos..];
    let Some(open) = rest.find('(') else {
        return Vec::new();
    };
    let Some(close) = rest[open..].find(')') else {
        return Vec::new();
    };
    rest[open + 1..open + close]
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty() && s.chars().all(|c| c.is_ascii_lowercase()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLAUDE_HELP_FRAGMENT: &str = "  --effort <level>                      Effort level for the current session\n                                        (low, medium, high, xhigh, max)\n  --model <model>                       Model for the current session.";

    #[test]
    fn effort_list_is_parsed_from_the_help_line() {
        assert_eq!(parse_claude_efforts(CLAUDE_HELP_FRAGMENT), vec!["low", "medium", "high", "xhigh", "max"]);
        assert!(parse_claude_efforts("no effort here").is_empty());
    }

    #[test]
    fn missing_flags_are_reported_by_name() {
        let help = "--model <m> --effort <level> (low, max)";
        assert_eq!(missing_flags(help, &["--model", "--effort", "--mcp-config"]), vec!["--mcp-config"]);
    }

    #[test]
    fn runtime_names_are_exact() {
        assert_eq!(RuntimeKind::parse("claude").unwrap(), RuntimeKind::Claude);
        assert_eq!(RuntimeKind::parse("codex").unwrap(), RuntimeKind::Codex);
        assert_eq!(RuntimeKind::parse("Claude").unwrap_err().code, ErrorCode::UnsupportedOperation);
        assert_eq!(RuntimeKind::parse("gemini").unwrap_err().code, ErrorCode::UnsupportedOperation);
    }

    #[test]
    fn explicit_missing_executable_is_unsupported_not_a_fallback() {
        let config = RuntimeConfig {
            claude_path: Some(PathBuf::from(r"C:\definitely\missing\claude.exe")),
            ..Default::default()
        };
        assert_eq!(detect(RuntimeKind::Claude, &config).unwrap_err().code, ErrorCode::UnsupportedOperation);
    }

    /// A unique folder under the system temp directory, removed when the guard drops (also on a failed assert).
    pub(super) struct TempDir(PathBuf);

    impl TempDir {
        pub(super) fn new(name: &str) -> Self {
            let dir = env::temp_dir().join(format!("designllm-detect-{name}-{}", designllm_contracts::new_id()));
            std::fs::create_dir_all(&dir).unwrap();
            TempDir(dir)
        }
    }

    impl std::ops::Deref for TempDir {
        type Target = Path;
        fn deref(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    /// An npm global prefix as `npm install -g @openai/codex` lays it out: shims in the prefix, the package
    /// under `node_modules`, and the native binary in the platform package nested inside it.
    fn fake_npm_prefix(root: &Path) -> PathBuf {
        let prefix = root.join("npm");
        write(&prefix.join("codex.cmd"), "@ECHO off\r\n\"%_prog%\" \"%dp0%\\node_modules\\@openai\\codex\\bin\\codex.js\" %*\r\n");
        write(&prefix.join("codex.ps1"), "# shim\n");
        let package = prefix.join(r"node_modules\@openai\codex");
        write(&package.join("package.json"), r#"{"name":"@openai/codex","bin":{"codex":"bin/codex.js"}}"#);
        write(&package.join(r"bin\codex.js"), "// entry\n");
        prefix
    }

    fn platform_dirs() -> (&'static str, &'static str) {
        if cfg!(target_arch = "aarch64") {
            ("codex-win32-arm64", "aarch64-pc-windows-msvc")
        } else {
            ("codex-win32-x64", "x86_64-pc-windows-msvc")
        }
    }

    /// The program `search_path` found, without its environment.
    fn program(found: Option<(PathBuf, Vec<EnvChange>)>) -> Option<PathBuf> {
        found.map(|(program, _)| program)
    }

    fn npm_env(package: &Path) -> Vec<EnvChange> {
        vec![
            ("CODEX_MANAGED_PACKAGE_ROOT", Some(package.as_os_str().to_owned())),
            ("CODEX_MANAGED_BY_NPM", Some("1".into())),
            ("CODEX_MANAGED_BY_BUN", None),
            ("CODEX_MANAGED_BY_PNPM", None),
            ("CODEX_MANAGED_BY_VITE_PLUS", None),
        ]
    }

    #[test]
    fn npm_codex_resolves_to_the_native_exe_not_the_cmd_shim() {
        let root = TempDir::new("npm-nested");
        let prefix = fake_npm_prefix(&root);
        let (platform, triple) = platform_dirs();
        let platform_root = prefix.join(r"node_modules\@openai\codex\node_modules\@openai").join(platform);
        write(&platform_root.join("package.json"), "{}");
        let native = platform_root.join("vendor").join(triple).join(r"bin\codex.exe");
        write(&native, "native");
        let path = env::join_paths([root.join("empty").as_path(), prefix.as_path()]).unwrap();

        let (found, env) = search_path(RuntimeKind::Codex, &path).expect("npm install resolves");
        assert_eq!(found, native);
        assert_eq!(found.extension().and_then(|e| e.to_str()), Some("exe"), "never the .cmd/.ps1 shim");
        assert_eq!(env, npm_env(&prefix.join(r"node_modules\@openai\codex")), "the environment bin/codex.js gives it");
    }

    #[test]
    fn npm_codex_follows_the_hoisted_platform_package_and_the_vendor_fallback() {
        let (platform, triple) = platform_dirs();

        let root = TempDir::new("npm-hoisted");
        let prefix = fake_npm_prefix(&root);
        let hoisted = prefix.join(r"node_modules\@openai").join(platform);
        write(&hoisted.join("package.json"), "{}");
        let native = hoisted.join("vendor").join(triple).join(r"bin\codex.exe");
        write(&native, "native");
        assert_eq!(program(search_path(RuntimeKind::Codex, prefix.as_os_str())), Some(native));

        let root = TempDir::new("npm-vendor");
        let prefix = fake_npm_prefix(&root);
        let native = prefix.join(r"node_modules\@openai\codex\vendor").join(triple).join(r"bin\codex.exe");
        write(&native, "native");
        assert_eq!(program(search_path(RuntimeKind::Codex, prefix.as_os_str())), Some(native));
    }

    #[test]
    fn a_shim_without_the_native_binary_is_not_found_and_never_run() {
        let root = TempDir::new("npm-shim-only");
        let prefix = fake_npm_prefix(&root);
        assert_eq!(search_path(RuntimeKind::Codex, prefix.as_os_str()), None, "codex.cmd alone is not a runner");

        // Another package under the codex name is not trusted either.
        let (platform, triple) = platform_dirs();
        let platform_root = prefix.join(r"node_modules\@openai\codex\node_modules\@openai").join(platform);
        write(&platform_root.join("package.json"), "{}");
        write(&platform_root.join("vendor").join(triple).join(r"bin\codex.exe"), "native");
        write(&prefix.join(r"node_modules\@openai\codex\package.json"), r#"{"name":"not-codex"}"#);
        assert_eq!(search_path(RuntimeKind::Codex, prefix.as_os_str()), None);

        let message = not_found(RuntimeKind::Codex, "codex.exe").message;
        assert!(message.contains("npm install of @openai/codex") && message.contains("DESIGNLLM_CODEX_PATH"), "{message}");
        assert!(!not_found(RuntimeKind::Claude, "claude.exe").message.contains("npm"));
    }

    #[test]
    fn path_order_decides_and_a_direct_exe_gets_no_npm_environment() {
        let (platform, triple) = platform_dirs();
        let root = TempDir::new("npm-order");
        let prefix = fake_npm_prefix(&root);
        let platform_root = prefix.join(r"node_modules\@openai\codex\node_modules\@openai").join(platform);
        write(&platform_root.join("package.json"), "{}");
        let native = platform_root.join("vendor").join(triple).join(r"bin\codex.exe");
        write(&native, "native");
        let standalone = root.join(r"standalone\codex.exe");
        write(&standalone, "standalone");

        let exe_first = env::join_paths([root.join("standalone").as_path(), prefix.as_path()]).unwrap();
        assert_eq!(search_path(RuntimeKind::Codex, &exe_first), Some((standalone, Vec::new())), "a direct install gets no env");
        let npm_first = env::join_paths([prefix.as_path(), root.join("standalone").as_path()]).unwrap();
        assert_eq!(program(search_path(RuntimeKind::Codex, &npm_first)), Some(native));
        assert_eq!(search_path(RuntimeKind::Claude, &npm_first), None, "the npm rule is for Codex only");

        let claude = root.join(r"claude\claude.exe");
        write(&claude, "claude");
        assert_eq!(search_path(RuntimeKind::Claude, root.join("claude").as_os_str()), Some((claude, Vec::new())));
    }

    #[test]
    fn empty_and_relative_path_entries_are_skipped() {
        // An unset PATH reads as an empty value; split_paths yields one empty entry for it, which would
        // otherwise be joined into a bare `codex.exe` looked up in the current directory.
        assert_eq!(env::split_paths(OsStr::new("")).count(), 1);
        assert_eq!(path_dirs(OsStr::new("")).count(), 0);
        let mixed = OsString::from(r";.;bin;C:relative;\rooted;C:\abs\one;;C:\abs\two;");
        assert_eq!(path_dirs(&mixed).collect::<Vec<_>>(), vec![PathBuf::from(r"C:\abs\one"), PathBuf::from(r"C:\abs\two")]);
        for kind in [RuntimeKind::Claude, RuntimeKind::Codex] {
            assert_eq!(search_path(kind, OsStr::new("")), None);
            assert_eq!(search_path(kind, OsStr::new(";;.")), None);
        }
    }

    #[test]
    fn the_runtime_environment_reaches_the_child() {
        let root = TempDir::new("env-child");
        let env: Vec<EnvChange> = vec![
            ("CODEX_MANAGED_PACKAGE_ROOT", Some(root.as_os_str().to_owned())),
            ("CODEX_MANAGED_BY_NPM", Some("1".into())),
            ("CODEX_MANAGED_BY_BUN", None),
        ];
        let cmd = Path::new(r"C:\Windows\System32\cmd.exe");
        let (code, text) = run_capture(cmd, &["/c", "set", "CODEX_MANAGED"], &env, DETECT_TIMEOUT).unwrap();
        assert_eq!(code, 0, "{text}");
        assert!(text.contains("CODEX_MANAGED_BY_NPM=1"), "{text}");
        assert!(text.contains(&format!("CODEX_MANAGED_PACKAGE_ROOT={}", root.display())), "{text}");
        assert!(!text.contains("CODEX_MANAGED_BY_BUN="), "{text}");
    }

    #[test]
    fn mock_needs_an_explicit_program() {
        let err = detect(RuntimeKind::Mock, &RuntimeConfig::default()).unwrap_err();
        assert_eq!(err.code, ErrorCode::UnsupportedOperation);
    }

    const CODEX_CATALOG_FIXTURE: &str = r#"{"models":[{"slug":"gpt-6.2-astra","display_name":"GPT-6.2-Astra","visibility":"list","priority":2,"supported_reasoning_levels":[{"effort":"low","description":"x"},{"effort":"high","description":"y"}]},{"slug":"codex-auto-review","display_name":"Codex Auto Review","visibility":"hide","priority":12},{"slug":"gpt-6.2-sol","display_name":"GPT-6.2-Sol","visibility":"list","priority":1,"supported_reasoning_levels":[{"effort":"medium","description":"z"}]},{"slug":"gpt-5.5","display_name":"GPT-5.5","visibility":"list"}]}"#;
    const CLAUDE_INIT_FIXTURE: &str = r#"{"type":"control_response","response":{"subtype":"success","request_id":"r1","response":{"models":[{"value":"default","resolvedModel":"claude-opus-5-5","displayName":"Default (recommended)","supportedEffortLevels":["low","max"]},{"value":"opus","resolvedModel":"claude-opus-5-5","displayName":"Opus 5.5","supportedEffortLevels":["low","medium","high","xhigh","max"]},{"value":"sonnet","resolvedModel":"claude-sonnet-5-5","displayName":"Sonnet 5.5","supportedEffortLevels":["low","medium","high"]}]}}}"#;

    #[test]
    fn codex_models_are_the_listed_slugs_by_priority_with_their_efforts() {
        let models = parse_codex_models(CODEX_CATALOG_FIXTURE).unwrap();
        let ids: Vec<&str> = models.iter().map(|model| model.id.as_str()).collect();
        assert_eq!(ids, vec!["gpt-6.2-sol", "gpt-6.2-astra", "gpt-5.5"]);
        assert_eq!(models[0].display_name, "GPT-6.2-Sol");
        assert_eq!(models[1].efforts, vec!["low", "high"]);
        assert!(models[2].efforts.is_empty());
    }

    #[test]
    fn codex_models_are_none_on_bad_or_empty_json() {
        assert!(parse_codex_models("not json").is_none());
        assert!(parse_codex_models(r#"{"models":[{"slug":"x","visibility":"hide","priority":1}]}"#).is_none());
        assert!(parse_codex_models(r#"{"other":1}"#).is_none());
    }

    #[test]
    fn claude_control_response_models_are_parsed_without_default() {
        let models = parse_claude_models(CLAUDE_INIT_FIXTURE).unwrap();
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].id, "claude-opus-5-5");
        assert_eq!(models[0].display_name, "Opus 5.5");
        assert_eq!(models[0].efforts, vec!["low", "medium", "high", "xhigh", "max"]);
        assert_eq!(models[1].id, "claude-sonnet-5-5");
        assert!(parse_claude_models("{}").is_none());
    }

    #[test]
    fn a_lookup_that_reads_nothing_gives_the_fixed_list() {
        let none = |_: &Path, _: &[EnvChange]| None;
        let codex = list_from_lookup(RuntimeKind::Codex, Path::new("codex"), &[], &none);
        assert_eq!(codex.source, ModelSource::Fallback);
        assert_eq!(codex.models.len(), 11);
        assert_eq!(codex.models[0].id, "gpt-6.2-sol");
        let claude = list_from_lookup(RuntimeKind::Claude, Path::new("claude"), &[], &none);
        assert_eq!(claude.source, ModelSource::Fallback);
        assert_eq!(claude.models.len(), 4);
        assert_eq!(claude.models[0].id, "claude-opus-5-5");
    }

    #[test]
    fn a_lookup_that_reads_a_list_is_the_cli_list() {
        let codex_text = |_: &Path, _: &[EnvChange]| Some(format!("warning: noise\n{CODEX_CATALOG_FIXTURE}"));
        let codex = list_from_lookup(RuntimeKind::Codex, Path::new("codex"), &[], &codex_text);
        assert_eq!(codex.source, ModelSource::Cli);
        assert_eq!(codex.models.len(), 3);
        let claude_text = |_: &Path, _: &[EnvChange]| Some(CLAUDE_INIT_FIXTURE.to_owned());
        let claude = list_from_lookup(RuntimeKind::Claude, Path::new("claude"), &[], &claude_text);
        assert_eq!(claude.source, ModelSource::Cli);
        assert_eq!(claude.models[1].id, "claude-sonnet-5-5");
    }

    #[test]
    fn a_missing_executable_gives_the_fixed_list_without_running_anything() {
        let config = RuntimeConfig { codex_path: Some(PathBuf::from("definitely-missing-codex-binary")), ..RuntimeConfig::default() };
        let list = model_list(RuntimeKind::Codex, &config);
        assert_eq!(list.source, ModelSource::Fallback);
        assert_eq!(list.models.len(), 11);
        let mock = model_list(RuntimeKind::Mock, &RuntimeConfig::default());
        assert!(mock.models.is_empty());
    }

    #[test]
    fn a_warning_after_the_codex_json_does_not_hide_the_list() {
        let text = format!("{CODEX_CATALOG_FIXTURE}\nwarning: a newer version is available\n");
        let ids: Vec<String> = models_from_text(RuntimeKind::Codex, &text).unwrap().into_iter().map(|model| model.id).collect();
        assert_eq!(ids, vec!["gpt-6.2-sol", "gpt-6.2-astra", "gpt-5.5"]);
    }

    #[test]
    fn the_claude_answer_line_is_found_by_its_json_not_its_spacing() {
        assert!(is_claude_init_answer(CLAUDE_INIT_FIXTURE));
        let spaced = r#"{ "type": "control_response", "response": { "subtype": "success", "request_id": "r1", "response": {} } }"#;
        assert!(is_claude_init_answer(spaced));
        assert!(!is_claude_init_answer(r#"{"type":"control_response","response":{"request_id":"r2"}}"#), "another request's answer");
        assert!(!is_claude_init_answer(r#"{"type":"system","subtype":"init","request_id":"r1"}"#));
        assert!(!is_claude_init_answer("control_response r1"));
    }

    #[test]
    fn the_fixed_claude_list_is_the_first_four_of_the_cli_order() {
        let ids: Vec<String> = fallback_models(RuntimeKind::Claude).into_iter().map(|model| model.id).collect();
        assert_eq!(ids, vec!["claude-opus-5-5", "claude-fable-5-2", "claude-sonnet-5-5", "claude-haiku-5-5"]);
    }
}
