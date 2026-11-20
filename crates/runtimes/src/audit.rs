//! Run audit: what the CLI reports about the model it runs, and a bounded transcript of the run.
//!
//! Claude. The installed CLI prints `{"type":"system","subtype":"init",...,"model":M,...}` as its first stream-json
//! line, before any model request. The init object takes `model` from the session model (checked in the installed
//! claude.exe, where the init object is built with `model: e.model`). The CLI's model registry lists each first-party
//! id as the bare id (`claude-haiku-5-5`) or as the bare id followed by an 8-digit snapshot date
//! (`claude-haiku-4-5-20251001`). Rule: M must equal the requested id, or the requested id followed by `-` and exactly
//! eight digits. Any other value is a mismatch, and so is an alias. Output before the init line, or no init line at
//! all, also fails the run. The rule comes from the bundled source of the installed CLI. No live model call confirmed it.
//!
//! Codex. The `--json` stream names the model only in a `session_configured` event (top-level `model`, or
//! `msg.model`). That event name exists in the installed codex.exe, but its JSON shape was not confirmed by a live run.
//! When no such event carries a model, the check is recorded as unavailable and the run is not failed.
//!
//! Effort. Neither CLI reports the effort it used. Effort is checked only as "passed on the command line". The
//! transcript header says so.
//!
//! Transcript. `<workspace>/logs/jobs/<job>/<attempt>.jsonl` holds a header record, each stdout line (as JSON when it
//! parses, otherwise as text), a note when stdout is cut, the tail of stderr, and an end record. The file never exceeds
//! [`TRANSCRIPT_CAP_BYTES`]. Known credential shapes are redacted. Environment variables and command-line arguments
//! are never written.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::Path;

use designllm_contracts::{Diagnostic, DltError, DltResult, ErrorCode};
use serde_json::{json, Map, Value};

use crate::detect::RuntimeKind;

/// Largest transcript file, in bytes.
pub const TRANSCRIPT_CAP_BYTES: u64 = 4 * 1024 * 1024;
/// Largest stderr tail kept in the transcript.
const STDERR_TAIL_BYTES: u64 = 64 * 1024;
/// Largest reserve for the end record and the notes.
const FOOTER_RESERVE_BYTES: u64 = 8 * 1024;
/// Claude record types that count as model output. None may appear before the init line.
const CLAUDE_OUTPUT_TYPES: &[&str] = &["assistant", "user", "result", "stream_event", "tool_use", "tool_result"];
/// Codex event that carries the session model.
const CODEX_MODEL_EVENT: &str = "session_configured";

/// The model rule. The reported id must equal the requested id, or be the requested id followed by `-` and an
/// 8-digit snapshot date.
pub fn model_matches(requested: &str, reported: &str) -> bool {
    if reported == requested {
        return true;
    }
    match reported.strip_prefix(requested).and_then(|rest| rest.strip_prefix('-')) {
        Some(date) => date.len() == 8 && date.bytes().all(|b| b.is_ascii_digit()),
        None => false,
    }
}

/// What the stream showed about the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelCheck {
    /// Nothing that carries a model has been read yet.
    Pending,
    /// The CLI reported a model that matches the request.
    Verified { reported: String },
    /// The stream carried no model. The run is not failed; the reason is recorded.
    Unavailable(&'static str),
    /// The check failed. The attempt is stopped and the job fails with the violation.
    Failed,
    /// The runtime has no model check (the mock agent).
    NotApplicable,
}

impl ModelCheck {
    /// Status word used in the transcript.
    pub fn status(&self) -> &'static str {
        match self {
            ModelCheck::Pending => "pending",
            ModelCheck::Verified { .. } => "verified",
            ModelCheck::Unavailable(_) => "unavailable",
            ModelCheck::Failed => "failed",
            ModelCheck::NotApplicable => "not_applicable",
        }
    }
}

/// Audit state of one attempt: the model check and the transcript.
pub struct RunAudit {
    kind: RuntimeKind,
    requested_model: String,
    reported: Option<String>,
    check: ModelCheck,
    violation: Option<DltError>,
    transcript: Transcript,
    finished: bool,
}

impl RunAudit {
    /// Creates the audit and, when `transcript` is given, its file with the header record. Fails when the file cannot
    /// be created, so an attempt never runs without the transcript it was asked to keep.
    pub fn open(
        kind: RuntimeKind,
        requested_model: &str,
        requested_effort: &str,
        program: &Path,
        transcript: Option<&Path>,
    ) -> DltResult<Self> {
        Self::open_with_cap(kind, requested_model, requested_effort, program, transcript, TRANSCRIPT_CAP_BYTES)
    }

    fn open_with_cap(
        kind: RuntimeKind,
        requested_model: &str,
        requested_effort: &str,
        program: &Path,
        transcript: Option<&Path>,
        cap: u64,
    ) -> DltResult<Self> {
        let mut transcript = Transcript::open(transcript, cap)?;
        transcript.record(&json!({
            "stream": "audit",
            "event": "start",
            "runtime": kind.wire_name(),
            "requested_model": requested_model,
            "requested_effort": requested_effort,
            "effort_check": "command_line_only",
            "program": program.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default(),
            "transcript_cap_bytes": cap,
        }));
        let check = if kind == RuntimeKind::Mock { ModelCheck::NotApplicable } else { ModelCheck::Pending };
        Ok(RunAudit {
            kind,
            requested_model: requested_model.to_string(),
            reported: None,
            check,
            violation: None,
            transcript,
            finished: false,
        })
    }

    /// The check so far.
    pub fn check(&self) -> &ModelCheck {
        &self.check
    }

    /// The violation that fails the job: a failed model check, or a transcript that could not be written.
    pub fn violation(&self) -> Option<DltError> {
        self.violation.clone()
    }

    /// Reads one stdout line and records it. Returns true when this line caused a violation; the caller then stops
    /// the attempt.
    pub fn observe_stdout(&mut self, line: &str) -> bool {
        let parsed: Option<Value> = serde_json::from_str(line).ok();
        match parsed.clone() {
            Some(value) => self.transcript.stdout(None, Some(redact_value(value))),
            None => self.transcript.stdout(Some(redact(line)), None),
        }
        if self.violation.is_some() {
            return false;
        }
        let Some(Value::Object(object)) = parsed else {
            return false;
        };
        match self.kind {
            RuntimeKind::Claude => self.claude_object(&object),
            RuntimeKind::Codex => self.codex_object(&object),
            RuntimeKind::Mock => false,
        }
    }

    /// Called once stdout has ended. Decides the checks that can only be settled at the end of the stream.
    pub fn finish_stdout(&mut self) {
        if !matches!(self.check, ModelCheck::Pending) {
            return;
        }
        match self.kind {
            RuntimeKind::Claude => {
                let error = init_error(&self.requested_model, "the claude CLI printed no init line");
                self.fail(error);
            }
            RuntimeKind::Codex => {
                self.check = ModelCheck::Unavailable("no session_configured event carried a model");
                self.note_check();
            }
            RuntimeKind::Mock => {}
        }
    }

    /// Writes the stderr tail and the end record. Idempotent. A transcript that could not be written fails the run.
    pub fn finish(&mut self, exit_code: Option<i32>, killed: bool, stderr_lines: &[String]) {
        if self.finished {
            return;
        }
        self.finished = true;
        if self.violation.is_none() {
            if let Some(error) = self.transcript.io_error() {
                self.violation = Some(DltError::new(ErrorCode::Io, format!("cannot write the run transcript: {error}")));
            }
        }
        let footer = json!({
            "stream": "audit",
            "event": "end",
            "exit_code": exit_code,
            "killed": killed,
            "model_check": self.check.status(),
            "reported_model": &self.reported,
            "stdout_lines": self.transcript.stdout_seq(),
            "dropped_stdout_lines": self.transcript.dropped_stdout(),
            "violation": self.violation.as_ref().map(|error| error.message.clone()),
        });
        self.transcript.finish(footer, stderr_lines);
    }

    fn claude_object(&mut self, object: &Map<String, Value>) -> bool {
        let kind = object.get("type").and_then(Value::as_str).unwrap_or("");
        let subtype = object.get("subtype").and_then(Value::as_str).unwrap_or("");
        if kind == "system" && subtype == "init" {
            return match object.get("model").and_then(Value::as_str) {
                Some(reported) => self.judge(reported),
                None => {
                    let error = init_error(&self.requested_model, "the init line has no model field");
                    self.fail(error)
                }
            };
        }
        if CLAUDE_OUTPUT_TYPES.contains(&kind) && matches!(self.check, ModelCheck::Pending) {
            let why = format!("the claude CLI printed a \"{kind}\" message before its init line");
            let error = init_error(&self.requested_model, &why);
            return self.fail(error);
        }
        false
    }

    fn codex_object(&mut self, object: &Map<String, Value>) -> bool {
        let message = object.get("msg").and_then(Value::as_object);
        let event = object
            .get("type")
            .and_then(Value::as_str)
            .or_else(|| message.and_then(|m| m.get("type")).and_then(Value::as_str));
        if event != Some(CODEX_MODEL_EVENT) {
            return false;
        }
        let reported = object
            .get("model")
            .and_then(Value::as_str)
            .or_else(|| message.and_then(|m| m.get("model")).and_then(Value::as_str));
        match reported {
            Some(reported) => self.judge(reported),
            None => false,
        }
    }

    /// Compares one reported model with the request. Returns true when the check failed on this call.
    fn judge(&mut self, reported: &str) -> bool {
        self.reported = Some(reported.to_string());
        if model_matches(&self.requested_model, reported) {
            if matches!(self.check, ModelCheck::Pending) {
                self.check = ModelCheck::Verified { reported: reported.to_string() };
                self.note_check();
            }
            return false;
        }
        let error = mismatch_error(self.kind, &self.requested_model, reported);
        self.fail(error)
    }

    fn fail(&mut self, error: DltError) -> bool {
        self.check = ModelCheck::Failed;
        self.violation = Some(error);
        self.note_check();
        true
    }

    fn note_check(&mut self) {
        let record = json!({
            "stream": "audit",
            "event": "model_check",
            "status": self.check.status(),
            "requested_model": &self.requested_model,
            "reported_model": &self.reported,
        });
        self.transcript.record(&record);
    }
}

fn mismatch_error(kind: RuntimeKind, requested: &str, reported: &str) -> DltError {
    let message = format!(
        "runtime model mismatch: the job requested model \"{requested}\" but the {} CLI reported \"{reported}\". \
         The attempt was stopped; no other model is used.",
        kind.wire_name()
    );
    with_diagnostic(message, "runtime_model_mismatch")
}

fn init_error(requested: &str, why: &str) -> DltError {
    let message = format!(
        "runtime init missing: {why}, so the model \"{requested}\" could not be verified. \
         The attempt was stopped; no other model is used."
    );
    with_diagnostic(message, "runtime_init_missing")
}

fn with_diagnostic(message: String, code: &str) -> DltError {
    let mut error = DltError::new(ErrorCode::Internal, message.clone());
    error.diagnostics.push(Diagnostic { code: code.to_string(), message, node_id: None, blocking: true, params: Default::default() });
    error
}

/// The transcript file. Writes stop at the cap, and the end record always fits in its reserve.
struct Transcript {
    file: Option<File>,
    stdout_limit: u64,
    stderr_reserve: u64,
    written: u64,
    seq: u64,
    stdout_full: bool,
    dropped_stdout: u64,
    io_error: Option<String>,
}

impl Transcript {
    fn open(path: Option<&Path>, cap: u64) -> DltResult<Self> {
        let file = match path {
            Some(path) => {
                if let Some(dir) = path.parent() {
                    fs::create_dir_all(dir)?;
                }
                Some(OpenOptions::new().create(true).write(true).truncate(true).open(path)?)
            }
            None => None,
        };
        let stderr_reserve = STDERR_TAIL_BYTES.min(cap / 8);
        let footer_reserve = FOOTER_RESERVE_BYTES.min(cap / 8);
        Ok(Transcript {
            file,
            stdout_limit: cap - stderr_reserve - footer_reserve,
            stderr_reserve,
            written: 0,
            seq: 0,
            stdout_full: false,
            dropped_stdout: 0,
            io_error: None,
        })
    }

    fn write(&mut self, text: &str) {
        let Some(file) = self.file.as_mut() else {
            return;
        };
        match file.write_all(text.as_bytes()) {
            Ok(()) => self.written += text.len() as u64,
            Err(error) => {
                self.io_error.get_or_insert_with(|| error.to_string());
                self.file = None;
            }
        }
    }

    fn record(&mut self, value: &Value) {
        let mut text = value.to_string();
        text.push('\n');
        self.write(&text);
    }

    fn stdout(&mut self, text: Option<String>, value: Option<Value>) {
        if self.file.is_none() {
            return;
        }
        if self.stdout_full {
            self.dropped_stdout += 1;
            return;
        }
        self.seq += 1;
        let record = match (value, text) {
            (Some(value), _) => json!({"seq": self.seq, "stream": "stdout", "json": value}),
            (None, Some(text)) => json!({"seq": self.seq, "stream": "stdout", "text": text}),
            (None, None) => json!({"seq": self.seq, "stream": "stdout", "text": ""}),
        };
        let mut line = record.to_string();
        line.push('\n');
        if self.written + line.len() as u64 > self.stdout_limit {
            self.stdout_full = true;
            self.dropped_stdout += 1;
            let note = json!({
                "stream": "audit",
                "event": "stdout_truncated",
                "last_kept_seq": self.seq - 1,
                "limit_bytes": self.stdout_limit,
                "note": "later stdout lines are not kept; the attempt itself was not stopped for this",
            });
            self.record(&note);
            return;
        }
        self.write(&line);
    }

    fn finish(&mut self, footer: Value, stderr_lines: &[String]) {
        if self.file.is_none() {
            return;
        }
        let redacted: Vec<String> = stderr_lines.iter().map(|line| redact(line)).collect();
        // Newest lines first, while they fit in the stderr reserve (the record's own keys need a little room).
        let budget = self.stderr_reserve.saturating_sub(512);
        let mut kept_newest_first: Vec<&str> = Vec::new();
        let mut used = 0u64;
        for line in redacted.iter().rev() {
            let size = serde_json::to_string(line).map(|text| text.len() as u64).unwrap_or(0) + 1;
            if used + size > budget {
                break;
            }
            used += size;
            kept_newest_first.push(line);
        }
        let kept = kept_newest_first.len();
        let dropped_lines = redacted.len() - kept;
        kept_newest_first.reverse();
        self.record(&json!({
            "stream": "stderr_tail",
            "kept_lines": kept,
            "dropped_lines": dropped_lines,
            "lines": kept_newest_first,
        }));
        self.record(&footer);
        // Nothing is written after the end record.
        self.file = None;
    }

    fn io_error(&self) -> Option<&str> {
        self.io_error.as_deref()
    }

    fn stdout_seq(&self) -> u64 {
        self.seq
    }

    fn dropped_stdout(&self) -> u64 {
        self.dropped_stdout
    }
}

/// Replaces credential-looking text with `[REDACTED]`. Known shapes only: `sk-` keys (including `sk-ant-`) and
/// `Bearer` tokens. This is best effort, not a complete secret scanner.
pub fn redact(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    let mut previous: Option<char> = None;
    while let Some(c) = rest.chars().next() {
        let at_boundary = !previous.is_some_and(|p| p.is_ascii_alphanumeric());
        if at_boundary {
            if let Some((prefix_len, body_len)) = secret_at(rest) {
                out.push_str(&rest[..prefix_len]);
                out.push_str("[REDACTED]");
                rest = &rest[prefix_len + body_len..];
                previous = Some(']');
                continue;
            }
        }
        out.push(c);
        previous = Some(c);
        rest = &rest[c.len_utf8()..];
    }
    out
}

/// (prefix bytes, token bytes) when a secret starts at the beginning of `text`. The token characters are ASCII, so
/// the character count is the byte count.
fn secret_at(text: &str) -> Option<(usize, usize)> {
    let shapes: [(&str, usize, fn(char) -> bool); 2] = [("sk-", 16, is_key_char), ("Bearer ", 8, is_bearer_char)];
    for (prefix, min_len, allowed) in shapes {
        if let Some(body) = text.strip_prefix(prefix) {
            let body_len = body.chars().take_while(|c| allowed(*c)).count();
            if body_len >= min_len {
                return Some((prefix.len(), body_len));
            }
        }
    }
    None
}

fn is_key_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

fn is_bearer_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '~' | '+' | '/' | '=' | '-')
}

fn redact_value(value: Value) -> Value {
    match value {
        Value::String(text) => Value::String(redact(&text)),
        Value::Array(items) => Value::Array(items.into_iter().map(redact_value).collect()),
        Value::Object(map) => Value::Object(map.into_iter().map(|(key, value)| (key, redact_value(value))).collect()),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("designllm-audit-{}", designllm_contracts::new_id()));
        fs::create_dir_all(&dir).expect("test folder");
        dir
    }

    fn claude(requested: &str) -> RunAudit {
        RunAudit::open(RuntimeKind::Claude, requested, "max", Path::new("claude.exe"), None).expect("audit")
    }

    fn codex(requested: &str) -> RunAudit {
        RunAudit::open(RuntimeKind::Codex, requested, "high", Path::new("codex.exe"), None).expect("audit")
    }

    #[test]
    fn the_model_rule_accepts_the_id_and_its_dated_snapshot_only() {
        assert!(model_matches("claude-haiku-5-5", "claude-haiku-5-5"));
        assert!(model_matches("claude-haiku-4-5", "claude-haiku-4-5-20251001"));
        assert!(!model_matches("claude-haiku-5-5", "claude-haiku-5-6"));
        assert!(!model_matches("claude-haiku-4-5", "claude-haiku-4-5-2025100"), "a 7-digit suffix is not a date");
        assert!(!model_matches("claude-haiku-4-5", "claude-haiku-4-5-latest"));
        assert!(!model_matches("haiku", "claude-haiku-4-5-20251001"), "an alias is not the resolved id");
        assert!(!model_matches("claude-haiku-5-5", "claude-haiku-5-55-20251001"));
    }

    #[test]
    fn a_claude_init_line_with_the_requested_model_verifies_without_violation() {
        let mut audit = claude("claude-haiku-5-5");
        assert!(!audit.observe_stdout(r#"{"type":"system","subtype":"init","model":"claude-haiku-5-5"}"#));
        assert_eq!(audit.check(), &ModelCheck::Verified { reported: "claude-haiku-5-5".into() });
        assert!(audit.violation().is_none());
    }

    #[test]
    fn a_claude_init_line_with_another_model_fails_and_names_both_ids() {
        let mut audit = claude("claude-haiku-5-5");
        assert!(audit.observe_stdout(r#"{"type":"system","subtype":"init","model":"claude-haiku-5-6"}"#));
        let error = audit.violation().expect("violation");
        assert!(error.message.contains("claude-haiku-5-5") && error.message.contains("claude-haiku-5-6"), "{}", error.message);
        assert_eq!(error.diagnostics[0].code, "runtime_model_mismatch");
        assert_eq!(audit.check(), &ModelCheck::Failed);
    }

    #[test]
    fn a_later_init_line_with_another_model_fails_too() {
        let mut audit = claude("claude-test-1");
        assert!(!audit.observe_stdout(r#"{"type":"system","subtype":"init","model":"claude-test-1"}"#));
        assert!(audit.observe_stdout(r#"{"type":"system","subtype":"init","model":"claude-other-9"}"#));
        assert!(audit.violation().expect("violation").message.contains("claude-other-9"));
    }

    #[test]
    fn claude_output_before_the_init_line_fails_and_plain_text_before_it_is_ignored() {
        let mut audit = claude("claude-haiku-5-5");
        assert!(!audit.observe_stdout("warning: plain text before init"));
        assert!(audit.observe_stdout(r#"{"type":"assistant","message":{"content":[]}}"#));
        let message = audit.violation().expect("violation").message;
        assert!(message.contains("before its init line"), "{message}");
    }

    #[test]
    fn a_claude_run_without_an_init_line_fails_at_the_end_of_its_output() {
        let mut audit = claude("claude-haiku-5-5");
        assert!(!audit.observe_stdout("not json at all"));
        audit.finish_stdout();
        let message = audit.violation().expect("violation").message;
        assert!(message.contains("no init line") && message.contains("claude-haiku-5-5"), "{message}");
    }

    #[test]
    fn a_claude_init_line_without_a_model_field_fails() {
        let mut audit = claude("claude-haiku-5-5");
        assert!(audit.observe_stdout(r#"{"type":"system","subtype":"init"}"#));
        assert!(audit.violation().expect("violation").message.contains("no model field"));
    }

    #[test]
    fn codex_verifies_a_session_configured_model_at_either_level_and_fails_a_mismatch() {
        let mut top = codex("gpt-5-codex");
        assert!(!top.observe_stdout(r#"{"type":"session_configured","model":"gpt-5-codex"}"#));
        assert!(matches!(top.check(), ModelCheck::Verified { .. }));
        let mut nested = codex("gpt-5-codex");
        assert!(!nested.observe_stdout(r#"{"id":"0","msg":{"type":"session_configured","model":"gpt-5-codex"}}"#));
        assert!(matches!(nested.check(), ModelCheck::Verified { .. }));
        let mut other = codex("gpt-5-codex");
        assert!(other.observe_stdout(r#"{"type":"session_configured","model":"gpt-5-mini"}"#));
        assert!(other.violation().expect("violation").message.contains("gpt-5-mini"));
    }

    #[test]
    fn a_codex_stream_without_a_model_is_unavailable_and_does_not_fail() {
        let mut audit = codex("gpt-5-codex");
        assert!(!audit.observe_stdout(r#"{"type":"thread.started","thread_id":"t-1"}"#));
        audit.finish_stdout();
        assert!(matches!(audit.check(), ModelCheck::Unavailable(_)));
        assert!(audit.violation().is_none());
    }

    #[test]
    fn the_mock_agent_is_never_checked() {
        let mut audit = RunAudit::open(RuntimeKind::Mock, "mock-model", "max", Path::new("mock.exe"), None).expect("audit");
        assert!(!audit.observe_stdout("MOCK pid=1 model=mock-model"));
        audit.finish_stdout();
        assert_eq!(audit.check(), &ModelCheck::NotApplicable);
        assert!(audit.violation().is_none());
    }

    #[test]
    fn the_transcript_stays_within_its_cap_and_says_where_it_was_cut() {
        let path = unique_dir().join("run.jsonl");
        let cap = 16 * 1024;
        let mut audit =
            RunAudit::open_with_cap(RuntimeKind::Mock, "m", "max", Path::new("mock.exe"), Some(&path), cap).expect("audit");
        let line = "x".repeat(1024);
        for _ in 0..100 {
            audit.observe_stdout(&line);
        }
        audit.finish(Some(0), false, &["stderr line".to_string()]);
        let text = fs::read_to_string(&path).expect("transcript");
        assert!((text.len() as u64) <= cap, "{} bytes", text.len());
        assert!(text.contains("stdout_truncated"));
        assert!(text.contains("stderr_tail"));
        assert!(text.lines().last().expect("end record").contains("\"event\":\"end\""));
        for record in text.lines() {
            serde_json::from_str::<Value>(record).expect("every record is one JSON value");
        }
        audit.finish(Some(0), false, &[]);
        assert_eq!(fs::read_to_string(&path).expect("transcript"), text, "the end record is written once");
    }

    #[test]
    fn credentials_are_redacted_and_ordinary_words_are_not() {
        let redacted = redact("key sk-ant-api03-ABCDEFGH12345678 and Bearer abc.DEF-123=");
        assert!(!redacted.contains("ABCDEFGH") && !redacted.contains("abc.DEF"), "{redacted}");
        assert!(redacted.contains("[REDACTED]"));
        assert_eq!(redact("task-1234567890123456789"), "task-1234567890123456789");
    }
}
