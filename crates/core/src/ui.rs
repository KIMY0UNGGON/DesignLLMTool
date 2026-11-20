//! UI channel (docs/UI_CHANNEL.md): GUI views publish their state and command registry, agents
//! read them and queue command runs; the GUI receives runs through `events.poll` and completes
//! them. Session state, in memory only. The core checks shapes, liveness and permission classes;
//! it never interprets command ids or arguments.
use crate::errors::{err, invalid, not_found, ErrExt};
use crate::{store, to_value, Workspace};
use designllm_contracts::*;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeSet, HashMap, VecDeque};
use std::time::{Duration, Instant};

const DEFAULT_VIEW_TTL_MS: u64 = 10_000;
const MIN_VIEW_TTL_MS: u64 = 100;
const MAX_VIEW_TTL_MS: u64 = 60_000;
const DEFAULT_RUN_TIMEOUT_MS: u64 = 15_000;
const MAX_RUN_TIMEOUT_MS: u64 = 120_000;
const MAX_STATE_BYTES: usize = 256 * 1024;
const MAX_REGISTRY_BYTES: usize = 1024 * 1024;
const MAX_COMMANDS: usize = 2_000;
const MAX_ARGS_BYTES: usize = 256 * 1024;
const MAX_RESULT_BYTES: usize = 1024 * 1024;
const MAX_PENDING_PER_VIEW: usize = 32;
const MAX_LIVE_VIEWS: usize = 16;
const MAX_FINISHED_RUNS: usize = 256;
const MAX_ID_BYTES: usize = 128;
const MAX_TEXT_CHARS: usize = 256;

/// `ui.state.read` / `ui.command.list` params.
#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct ViewParams { #[serde(default)] pub view_id: Option<String> }
/// `ui.command.get` params.
#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct RunIdParams { pub run_id: String }

struct View {
    token: String,
    state: UiState,
    commands: Vec<UiCommand>,
    /// `state` and `commands` as JSON, to tell a change from a heartbeat.
    fingerprint: (Value, Value, Option<u32>),
    state_seq: u64,
    /// A view must advertise the guard before Core can queue mutating runs for it.
    run_guard_version: Option<u32>,
    ttl: Duration,
    published: Instant,
    /// Last publish or poll with the token.
    seen: Instant,
    /// Publish order among views; the default view is the latest.
    order: u64,
}

struct Run { run: UiRun, deadline: Instant, created: u64 }

#[derive(Default)]
pub(crate) struct UiChannel {
    views: HashMap<String, View>,
    runs: HashMap<String, Run>,
    /// Ended runs, oldest first; only the last MAX_FINISHED_RUNS stay readable.
    finished: VecDeque<String>,
    /// One counter for publish order, state_seq and run order, so a state_seq never repeats.
    counter: u64,
}

fn unavailable(message: impl Into<String>) -> DltError { err(ErrorCode::Unavailable, message) }
fn ms(d: Duration) -> u64 { u64::try_from(d.as_millis()).unwrap_or(u64::MAX) }
fn json_len(v: &impl Serialize) -> usize { serde_json::to_vec(v).map(|b| b.len()).unwrap_or(usize::MAX) }
fn name<T: Serialize>(v: &T) -> String { serde_json::to_value(v).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default() }

fn unknown_outcome(run: &UiRun, reason: &str) -> DltError {
    let mut error = unavailable(format!("{reason}; command {} may already have had side effects", run.command_id));
    error.retryable = false;
    error.diagnostics.push(Diagnostic {
        code: "ui.outcome_unknown".into(),
        message: "The GUI stopped reporting after delivery. The command may have changed a document, written a file, or started a job; inspect its effects before retrying.".into(),
        node_id: None,
        blocking: false,
        params: Default::default(),
    });
    error
}

fn check_view_id(id: &str) -> DltResult<()> {
    if id.is_empty() || id.len() > MAX_ID_BYTES || id.chars().any(char::is_control) {
        return Err(invalid(format!("view_id must be 1..={MAX_ID_BYTES} bytes without control characters")));
    }
    Ok(())
}
fn check_command_id(id: &str) -> DltResult<()> {
    if id.is_empty() || id.len() > MAX_ID_BYTES || !id.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'-')) {
        return Err(invalid(format!("command id {id:?} must be 1..={MAX_ID_BYTES} characters of A-Z a-z 0-9 . _ : -")));
    }
    Ok(())
}
fn check_text(what: &str, text: &str, required: bool) -> DltResult<()> {
    if required && text.trim().is_empty() { return Err(invalid(format!("{what} is empty"))); }
    if text.chars().count() > MAX_TEXT_CHARS { return Err(invalid(format!("{what} is longer than {MAX_TEXT_CHARS} characters"))); }
    Ok(())
}
fn run_timeout(requested: Option<u64>) -> DltResult<u64> {
    let t = requested.unwrap_or(DEFAULT_RUN_TIMEOUT_MS);
    if t == 0 || t > MAX_RUN_TIMEOUT_MS { return Err(invalid(format!("timeout_ms must be 1..={MAX_RUN_TIMEOUT_MS}"))); }
    Ok(t)
}
fn check_registry(commands: &[UiCommand]) -> DltResult<()> {
    if commands.len() > MAX_COMMANDS { return Err(invalid(format!("the command registry has {} commands; at most {MAX_COMMANDS}", commands.len()))); }
    let mut seen = BTreeSet::new();
    for c in commands {
        check_command_id(&c.id)?;
        if !seen.insert(c.id.as_str()) { return Err(invalid(format!("command id {:?} is registered twice", c.id))); }
        check_text(&format!("label of {}", c.id), &c.label, false)?;
        if let Some(s) = &c.shortcut { check_text(&format!("shortcut of {}", c.id), s, false)?; }
        if !c.args_schema.is_object() { return Err(invalid(format!("args_schema of {} must be a JSON object (a JSON Schema)", c.id))); }
        if c.id == UI_SCREENSHOT_COMMAND && c.permission != UiPermission::Read { return Err(invalid(format!("{UI_SCREENSHOT_COMMAND} is a read command"))); }
    }
    let size = json_len(&commands);
    if size > MAX_REGISTRY_BYTES { return Err(invalid(format!("the command registry is {size} bytes; at most {MAX_REGISTRY_BYTES}"))); }
    Ok(())
}
fn check_state(state: &UiState) -> DltResult<()> {
    check_text("state.screen", &state.screen, true)?;
    if !(state.detail.is_null() || state.detail.is_object()) { return Err(invalid("state.detail must be a JSON object")); }
    if state.zoom_milli.is_some_and(|z| z == 0 || z > 100_000) { return Err(invalid("state.zoom_milli must be 1..=100000")); }
    if state.viewport.is_some_and(|v| v.width <= 0 || v.height <= 0) { return Err(invalid("state.viewport must have positive width and height")); }
    let size = json_len(state);
    if size > MAX_STATE_BYTES { return Err(invalid(format!("state is {size} bytes; at most {MAX_STATE_BYTES}"))); }
    Ok(())
}

impl UiChannel {
    fn next(&mut self) -> u64 { self.counter += 1; self.counter }

    /// Views past their ttl are removed; only runs that were not delivered have a known non-execution outcome.
    fn sweep(&mut self, now: Instant) {
        let gone: Vec<(String, u64)> = self.views.iter().filter(|(_, v)| now.duration_since(v.seen) >= v.ttl).map(|(id, v)| (id.clone(), ms(v.ttl))).collect();
        for (id, ttl) in gone {
            self.views.remove(&id);
            self.end_pending(|r| r.run.view_id == id, |r| match r.run.status {
                UiRunStatus::Queued => (
                    UiRunStatus::Failed,
                    unavailable(format!("GUI view {} expired before command {} was delivered; the command was not run", r.run.view_id, r.run.command_id)),
                ),
                UiRunStatus::Delivered => (
                    UiRunStatus::Failed,
                    unknown_outcome(&r.run, &format!("GUI view {} stopped responding after delivery (no publish or poll for {ttl} ms)", r.run.view_id)),
                ),
                _ => unreachable!("only queued or delivered runs are pending"),
            });
        }
        self.end_pending(|r| r.deadline <= now, |r| match r.run.status {
            UiRunStatus::Queued => (
                UiRunStatus::TimedOut,
                unavailable(format!("command {} timed out before delivery; it was not run", r.run.command_id)),
            ),
            UiRunStatus::Delivered => (
                UiRunStatus::Failed,
                unknown_outcome(&r.run, &format!("the GUI did not report the outcome of command {} within {} ms", r.run.command_id, r.run.timeout_ms)),
            ),
            _ => unreachable!("only queued or delivered runs are pending"),
        });
    }
    fn end_pending(&mut self, pick: impl Fn(&Run) -> bool, outcome: impl Fn(&Run) -> (UiRunStatus, DltError)) {
        let mut ids: Vec<(u64, String)> = self.runs.iter().filter(|(_, r)| r.run.status.is_pending() && pick(r)).map(|(id, r)| (r.created, id.clone())).collect();
        ids.sort();
        for (_, id) in ids {
            if let Some(r) = self.runs.get_mut(&id) {
                let (status, e) = outcome(r);
                r.run.status = status;
                r.run.error = Some(e);
            }
            self.ended(id);
        }
    }
    fn ended(&mut self, run_id: String) {
        self.finished.push_back(run_id);
        while self.finished.len() > MAX_FINISHED_RUNS {
            if let Some(old) = self.finished.pop_front() { self.runs.remove(&old); }
        }
    }

    /// A live view by id, or the most recently published one.
    fn view(&self, view_id: Option<&str>) -> DltResult<(&String, &View)> {
        match view_id {
            Some(id) => self.views.get_key_value(id).ok_or_else(|| unavailable(format!("GUI view {id} is not live"))),
            None => self.views.iter().max_by_key(|(_, v)| v.order).ok_or_else(|| unavailable("no GUI view is live; the app window publishes its state while it is open")),
        }
    }
    /// The GUI's own view, checked by its token; the call counts as a sign of life.
    fn own_view(&mut self, view_id: &str, token: &str, now: Instant) -> DltResult<&mut View> {
        let view = self.views.get_mut(view_id).ok_or_else(|| unavailable(format!("GUI view {view_id} is not live (its ttl passed); publish its state again")))?;
        if view.token != token { return Err(invalid(format!("view_token does not belong to view {view_id}"))); }
        view.seen = now;
        Ok(view)
    }

    fn publish(&mut self, req: UiPublishRequest, now: Instant) -> DltResult<UiPublishResult> {
        self.sweep(now);
        check_view_id(&req.view_id)?;
        let ttl_ms = req.ttl_ms.unwrap_or(DEFAULT_VIEW_TTL_MS);
        if !(MIN_VIEW_TTL_MS..=MAX_VIEW_TTL_MS).contains(&ttl_ms) { return Err(invalid(format!("ttl_ms must be {MIN_VIEW_TTL_MS}..={MAX_VIEW_TTL_MS}"))); }
        check_state(&req.state)?;
        if let Some(commands) = &req.commands { check_registry(commands)?; }
        if let Some(view) = self.views.get(&req.view_id) {
            if req.view_token.as_deref() != Some(view.token.as_str()) {
                return Err(invalid(format!("view {} is live and belongs to another client; pass its view_token", req.view_id)));
            }
        } else if self.views.len() >= MAX_LIVE_VIEWS {
            return Err(unavailable(format!("{MAX_LIVE_VIEWS} GUI views are live already")));
        }
        let order = self.next();
        let ttl = Duration::from_millis(ttl_ms);
        let state_value = to_value(&req.state)?;
        let run_guard_version = req.run_guard_version.filter(|version| *version == UI_RUN_GUARD_VERSION);
        if let Some(view) = self.views.get_mut(&req.view_id) {
            let commands = req.commands.unwrap_or_else(|| view.commands.clone());
            let fingerprint = (state_value, to_value(&commands)?, run_guard_version);
            // A heartbeat (same state, same registry) keeps state_seq.
            if fingerprint != view.fingerprint {
                self.counter += 1;
                view.state_seq = self.counter;
            }
            view.state = req.state;
            view.commands = commands;
            view.fingerprint = fingerprint;
            view.run_guard_version = run_guard_version;
            view.ttl = ttl;
            view.published = now;
            view.seen = now;
            view.order = order;
            return Ok(UiPublishResult {
                view_id: req.view_id,
                view_token: view.token.clone(),
                state_seq: view.state_seq,
                ttl_ms,
                commands: view.commands.len(),
                run_guard_version: view.run_guard_version,
            });
        }
        let commands = req.commands.unwrap_or_default();
        let fingerprint = (state_value, to_value(&commands)?, run_guard_version);
        let state_seq = self.next();
        let token = new_id();
        let count = commands.len();
        self.views.insert(req.view_id.clone(), View {
            token: token.clone(),
            state: req.state,
            commands,
            fingerprint,
            state_seq,
            run_guard_version,
            ttl,
            published: now,
            seen: now,
            order,
        });
        Ok(UiPublishResult { view_id: req.view_id, view_token: token, state_seq, ttl_ms, commands: count, run_guard_version })
    }

    fn state_view(&mut self, view_id: Option<&str>, now: Instant) -> DltResult<UiStateView> {
        self.sweep(now);
        let (id, v) = self.view(view_id)?;
        Ok(UiStateView { view_id: id.clone(), state_seq: v.state_seq, age_ms: ms(now.duration_since(v.published)), ttl_ms: ms(v.ttl), state: v.state.clone() })
    }
    fn command_list(&mut self, view_id: Option<&str>, now: Instant) -> DltResult<UiCommandList> {
        self.sweep(now);
        let (id, v) = self.view(view_id)?;
        Ok(UiCommandList { view_id: id.clone(), state_seq: v.state_seq, commands: v.commands.clone() })
    }

    fn run(&mut self, req: UiRunRequest, now: Instant) -> DltResult<UiRun> {
        self.sweep(now);
        check_text("actor", &req.actor, true)?;
        let timeout_ms = run_timeout(req.timeout_ms)?;
        if !req.args.is_object() { return Err(invalid("args must be a JSON object")); }
        let size = json_len(&req.args);
        if size > MAX_ARGS_BYTES { return Err(invalid(format!("args are {size} bytes; at most {MAX_ARGS_BYTES}"))); }
        let (view_id, view) = self.view(req.view_id.as_deref())?;
        if let Some(expected) = req.expected_state_seq {
            if expected != view.state_seq {
                return Err(err(ErrorCode::RevisionConflict, format!("view {view_id} changed (state_seq {}, expected {expected}); read it again", view.state_seq)));
            }
        }
        let command = view.commands.iter().find(|c| c.id == req.command_id)
            .ok_or_else(|| not_found(format!("view {view_id} has no command {:?}; ui.command.list names its commands", req.command_id)).ids([req.command_id.clone()]))?;
        if req.command_id == "ai.cancel" {
            let has_identifier = |key: &str| req.args.get(key).and_then(Value::as_str).is_some_and(|value| !value.trim().is_empty());
            if !has_identifier("job_id") || !has_identifier("attempt_id") {
                return Err(invalid("ai.cancel requires non-empty job_id and attempt_id"));
            }
        }
        if !command.enabled { return Err(invalid(format!("command {} is disabled in the view's current state", command.id)).ids([command.id.clone()])); }
        if !req.permission.allows(command.permission) {
            return Err(invalid(format!("command {} is a {} command; this call grants {}", command.id, name(&command.permission), name(&req.permission))).ids([command.id.clone()]));
        }
        let missing: Vec<Intent> = command.intents.iter().copied().filter(|i| !req.allowed_intents.contains(i)).collect();
        if !missing.is_empty() {
            return Err(err(ErrorCode::ContentLocked, format!("command {} changes {missing:?}, which allowed_intents does not include", command.id)).ids([command.id.clone()]));
        }
        let state_relative_read = matches!(req.command_id.as_str(), "select.next" | "select.previous");
        let state_bound_command = state_relative_read || req.command_id == "ai.cancel";
        let requires_run_guard = command.permission != UiPermission::Read || !command.intents.is_empty() || state_bound_command;
        if requires_run_guard && view.run_guard_version != Some(UI_RUN_GUARD_VERSION) {
            let message = if state_relative_read {
                format!("view {view_id} has not advertised the UI run-state guard; state-relative selection commands are refused")
            } else if req.command_id == "ai.cancel" {
                format!("view {view_id} has not advertised the UI run-state guard; state-bound job cancellation is refused")
            } else {
                format!("view {view_id} has not advertised the UI run-state guard; mutating commands are refused")
            };
            return Err(err(
                ErrorCode::UnsupportedOperation,
                message,
            ));
        }
        if state_bound_command && req.expected_state_seq.is_none() {
            let message = if state_relative_read {
                "expected_state_seq is required for state-relative selection commands"
            } else {
                "expected_state_seq is required for job cancellation"
            };
            return Err(invalid(message));
        }
        if matches!(req.command_id.as_str(), "canvas.interaction.set" | "canvas.interaction.remove") && req.expected_state_seq.is_none() {
            return Err(invalid("expected_state_seq is required for index-based Canvas interaction commands"));
        }
        let view_id = view_id.clone();
        let expected = requires_run_guard.then(|| UiRunExpected {
            state_seq: view.state_seq,
            screen: view.state.screen.clone(),
            artifact_id: view.state.artifact_id.clone(),
            revision: view.state.revision,
            selection: view.state.selection.clone(),
            page_index: view.state.page_index,
        });
        self.queue(view_id, req.command_id, req.args, req.actor, req.permission, req.allowed_intents, expected, timeout_ms, now)
    }
    fn screenshot(&mut self, req: UiScreenshotRequest, now: Instant) -> DltResult<UiRun> {
        self.sweep(now);
        check_text("actor", &req.actor, true)?;
        let timeout_ms = run_timeout(req.timeout_ms)?;
        let (view_id, view) = self.view(req.view_id.as_deref())?;
        if view.commands.iter().any(|c| c.id == UI_SCREENSHOT_COMMAND && !c.enabled) { return Err(invalid(format!("view {view_id} has {UI_SCREENSHOT_COMMAND} disabled"))); }
        let view_id = view_id.clone();
        self.queue(view_id, UI_SCREENSHOT_COMMAND.into(), json!({}), req.actor, UiPermission::Read, BTreeSet::new(), None, timeout_ms, now)
    }
    #[allow(clippy::too_many_arguments)]
    fn queue(&mut self, view_id: String, command_id: String, args: Value, actor: String, permission: UiPermission, allowed_intents: BTreeSet<Intent>, expected: Option<UiRunExpected>, timeout_ms: u64, now: Instant) -> DltResult<UiRun> {
        let pending = self.runs.values().filter(|r| r.run.view_id == view_id && r.run.status.is_pending()).count();
        if pending >= MAX_PENDING_PER_VIEW { return Err(unavailable(format!("view {view_id} has {pending} commands waiting; try again when they end"))); }
        let created = self.next();
        let run = UiRun { run_id: new_id(), view_id, expected, command_id, args, actor, permission, allowed_intents, status: UiRunStatus::Queued, timeout_ms, result: None, error: None };
        self.runs.insert(run.run_id.clone(), Run { run: run.clone(), deadline: now + Duration::from_millis(timeout_ms), created });
        Ok(run)
    }

    /// `events.poll` of a view: its queued runs, oldest first, each handed out once.
    fn deliver(&mut self, view_id: &str, token: &str, now: Instant) -> DltResult<Vec<UiRun>> {
        self.sweep(now);
        self.own_view(view_id, token, now)?;
        let mut queued: Vec<(u64, String)> = self.runs.iter().filter(|(_, r)| r.run.view_id == view_id && r.run.status == UiRunStatus::Queued).map(|(id, r)| (r.created, id.clone())).collect();
        queued.sort();
        let mut out = Vec::with_capacity(queued.len());
        for (_, id) in queued {
            if let Some(r) = self.runs.get_mut(&id) {
                r.run.status = UiRunStatus::Delivered;
                let mut delivered = r.run.clone();
                // A GUI waiting for consent or preview readiness only has the time left after delivery.
                delivered.timeout_ms = ms(r.deadline.saturating_duration_since(now));
                out.push(delivered);
            }
        }
        Ok(out)
    }

    fn command_of(&self, run_id: &str, view_id: &str) -> Option<&str> {
        self.runs.get(run_id).filter(|r| r.run.view_id == view_id).map(|r| r.run.command_id.as_str())
    }

    fn complete(&mut self, req: UiCompleteRequest, now: Instant) -> DltResult<UiRun> {
        self.sweep(now);
        self.own_view(&req.view_id, &req.view_token, now)?;
        let entry = self.runs.get_mut(&req.run_id).filter(|r| r.run.view_id == req.view_id)
            .ok_or_else(|| not_found(format!("run {} of view {} is unknown or expired", req.run_id, req.view_id)))?;
        match entry.run.status {
            UiRunStatus::Delivered => {}
            UiRunStatus::Queued => return Err(invalid(format!("run {} was not handed out yet; events.poll with the view delivers it", req.run_id))),
            ended => return Err(invalid(format!("run {} already ended ({})", req.run_id, name(&ended)))),
        }
        let (result, error) = match (req.result, req.error) {
            (Some(result), None) => {
                let size = json_len(&result);
                if size > MAX_RESULT_BYTES { return Err(invalid(format!("result is {size} bytes; at most {MAX_RESULT_BYTES}"))); }
                (Some(result), None)
            }
            (None, Some(error)) => (None, Some(error)),
            _ => return Err(invalid("pass exactly one of result (not null) and error")),
        };
        entry.run.status = if error.is_some() { UiRunStatus::Failed } else { UiRunStatus::Completed };
        entry.run.result = result;
        entry.run.error = error;
        let run = entry.run.clone();
        self.ended(req.run_id);
        Ok(run)
    }

    fn get(&mut self, run_id: &str, now: Instant) -> DltResult<UiRun> {
        self.sweep(now);
        self.runs.get(run_id).map(|r| r.run.clone())
            .ok_or_else(|| not_found(format!("run {run_id} is unknown or expired (the last {MAX_FINISHED_RUNS} ended runs are kept)")))
    }
}

impl Workspace {
    pub(crate) fn ui_publish(&mut self, req: UiPublishRequest) -> DltResult<Value> {
        match (&req.state.artifact_id, req.state.revision) {
            (Some(id), revision) => {
                let row = store::artifact_row(&self.conn, id)?;
                if let Some(r) = revision.filter(|r| *r > row.current_revision) {
                    return Err(invalid(format!("state.revision {r} is ahead of artifact {id} (revision {})", row.current_revision)));
                }
            }
            (None, Some(_)) => return Err(invalid("state.revision needs state.artifact_id")),
            (None, None) => {}
        }
        to_value(&self.ui.publish(req, Instant::now())?)
    }
    pub(crate) fn ui_state_read(&mut self, p: ViewParams) -> DltResult<Value> { to_value(&self.ui.state_view(p.view_id.as_deref(), Instant::now())?) }
    pub(crate) fn ui_command_list(&mut self, p: ViewParams) -> DltResult<Value> { to_value(&self.ui.command_list(p.view_id.as_deref(), Instant::now())?) }
    pub(crate) fn ui_command_get(&mut self, p: RunIdParams) -> DltResult<Value> { to_value(&self.ui.get(&p.run_id, Instant::now())?) }
    pub(crate) fn ui_command_run(&mut self, req: UiRunRequest) -> DltResult<Value> {
        let run = self.ui.run(req, Instant::now())?;
        self.ui_event("ui.command.queued", &run)?;
        to_value(&run)
    }
    pub(crate) fn ui_screenshot(&mut self, req: UiScreenshotRequest) -> DltResult<Value> {
        let run = self.ui.screenshot(req, Instant::now())?;
        self.ui_event("ui.command.queued", &run)?;
        to_value(&run)
    }
    pub(crate) fn ui_command_complete(&mut self, req: UiCompleteRequest) -> DltResult<Value> {
        if let (Some(result), Some(UI_SCREENSHOT_COMMAND)) = (&req.result, self.ui.command_of(&req.run_id, &req.view_id)) {
            self.check_screenshot(result)?;
        }
        let run = self.ui.complete(req, Instant::now())?;
        self.ui_event("ui.command.finished", &run)?;
        to_value(&run)
    }
    /// `events.poll` with `view_id` + `view_token`: the view's queued runs.
    pub(crate) fn ui_deliver(&mut self, view_id: &str, token: &str) -> DltResult<Vec<UiRun>> { self.ui.deliver(view_id, token, Instant::now()) }

    /// A screenshot result names a stored PNG asset and its pixel size.
    fn check_screenshot(&self, result: &Value) -> DltResult<()> {
        let shape = || invalid(format!("a {UI_SCREENSHOT_COMMAND} result is {{\"asset\": AssetRef, \"width_px\": n, \"height_px\": n}}; import the PNG with asset.import first"));
        let asset: AssetRef = result.get("asset").cloned().and_then(|a| serde_json::from_value(a).ok()).ok_or_else(shape)?;
        let stored = self.base.get_ref(&asset.sha256).ok_or_else(|| not_found(format!("screenshot asset {} is not in the workspace; import it with asset.import first", asset.sha256)))?;
        if stored != asset { return Err(invalid(format!("screenshot asset {} metadata differs from the stored asset", asset.sha256))); }
        if stored.mime != "image/png" { return Err(invalid(format!("a screenshot is image/png, not {}", stored.mime))); }
        for key in ["width_px", "height_px"] {
            if !result.get(key).and_then(Value::as_u64).is_some_and(|v| v > 0) { return Err(shape()); }
        }
        Ok(())
    }
    /// Durable wake-up for subscribed GUIs and polling agents. No args or results: a replayed event
    /// never carries a command to run.
    fn ui_event(&mut self, event: &str, run: &UiRun) -> DltResult<()> {
        let mut detail = json!({ "run_id": run.run_id, "view_id": run.view_id, "command_id": run.command_id, "actor": run.actor });
        if !run.status.is_pending() { detail["status"] = json!(run.status); }
        store::insert_event(&self.conn, None, None, event, &detail)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::UiPermission::{AgentStart, Change, FileWrite, Read};

    #[test]
    fn permission_classes_are_ordered_and_agent_start_stands_alone() {
        for (granted, needed, ok) in [
            (Read, Read, true), (Read, Change, false), (Change, Read, true), (Change, Change, true), (Change, FileWrite, false),
            (FileWrite, Read, true), (FileWrite, FileWrite, true), (FileWrite, AgentStart, false), (AgentStart, AgentStart, true),
            (AgentStart, Read, false), (AgentStart, FileWrite, false),
        ] {
            assert_eq!(granted.allows(needed), ok, "{granted:?} -> {needed:?}");
        }
    }
}
