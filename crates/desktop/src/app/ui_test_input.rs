//! Synthetic input for UI tests (task 69, docs/UI_CHANNEL.md "시험용 입력"). A window started with `DESIGNLLM_UI_TEST_INPUT`
//! lists two more commands in its registry: `ui.input` and `ui.layout`. Without it they are not listed (the core refuses
//! a run of them as an unknown command) and a run that reaches the window anyway is refused here.
//!
//! An agent's `ui.input` events are played into the raw input of the window's next frames at the top of
//! `App::raw_input_hook` (`inject_ui_test_input`), so they take the path a person's input takes: the hook, then
//! `App::update`. Nothing is sent to the operating system.
//!
//! - Attribution (81): while a run plays, settles, and for its settle time after it ends, the window's own requests that
//!   name the `desktop` actor carry the run's actor (`pin_ui_test_request`), and an inline edit the run starts is owned by
//!   `<run actor>:inline:<view>`. A frame that also carries a person's input is not pinned.
//! - Approvals stay a person's: while an approval is open (a model run, closing over held edits, export losses) no run
//!   starts, and a synthetic event during one, or a press on a control drawn as human only, ends the run with HUMAN_ONLY
//!   before the event reaches the window.
//! - File pickers: a run may queue `dialog_answers`; checked by the agent path rules before any file is read, they stand in
//!   for the native dialogs while the run plays, and the core requests that carry them are marked `agent_path`.
//!
//! `ui.layout` reads where the last frame drew pages, nodes, table cells, memos, layer rows and controls.

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use designllm_contracts::{
    ArtifactBody, Diagnostic, DltError, ErrorCode, Intent, Node, UiCommand, UiInputEvent, UiInputModifiers, UiInputRequest,
    UiInputResult, UiInputStepFrames, UiLayout, UiLayoutCell, UiLayoutControl, UiLayoutMemo, UiLayoutNode, UiLayoutPage,
    UiLayoutPanel, UiLayoutRow, UiPermission, UiPointRect, UiPointerButton, UiRun, UiWheelUnit, UI_INPUT_COMMAND,
    UI_LAYOUT_COMMAND,
};
use eframe::egui::{self, Context, Event, ImeEvent, Key, Modifiers, MouseWheelUnit, PointerButton, Pos2, RawInput, Rect, Vec2};
use serde_json::{json, Value};

use super::{board::LeftTab, DesktopApp};
use crate::{board as board_geometry, commands, frame, ui_record};

/// The refusal of a model run in a UI-test window (task 69): such a window runs only the mock runtime.
pub(super) const REAL_RUN_REFUSED: &str = "시험용 입력 창에서는 실제 모델 실행기를 쓰지 않습니다. mock 실행기만 시작합니다";
/// A UI-test window asks the runner CLIs for their model lists (68-1) only with this set to 1.
const UI_TEST_CLI_MODELS_ENV: &str = "DESIGNLLM_UI_TEST_CLI_MODELS";

/// `DESIGNLLM_UI_TEST_INPUT=1` (or `on`) turns the synthetic input on in a visible window, `offscreen` in a window placed
/// outside every monitor (as the smoke capture, task 65) whose native dialogs are refused.
pub(crate) const UI_TEST_INPUT_ENV: &str = "DESIGNLLM_UI_TEST_INPUT";
/// U02: a UI-test window's egui zoom factor, 1.0, 1.25, 1.5, 1.75 or 2.0 (points per pixel at 100 % display scale).
const UI_TEST_SCALE_ENV: &str = "DESIGNLLM_UI_TEST_SCALE";

const MAX_STEPS: usize = 1000;
const MAX_FRAMES: usize = 6000;
const MAX_TEXT_CHARS: usize = 4096;
const MAX_GAP_FRAMES: u32 = 600;
const MAX_CLICKS: u8 = 3;
const MAX_DRAG_STEPS: u32 = 120;
const MAX_DIALOG_ANSWERS: usize = 16;
const COORDINATE_LIMIT: f32 = 100_000.0;
const DEFAULT_SETTLE_MS: u64 = 5_000;
const MAX_SETTLE_MS: u64 = 60_000;
/// Frames in a row without a request in flight that end a settle wait.
const QUIET_FRAMES: u32 = 2;
/// How often a UI-test window polls the core for runs (an ordinary window polls every 2 s).
pub(super) const TEST_EVENT_POLL: Duration = Duration::from_millis(200);
/// The layout lists are cut at these lengths so that a result stays under the core's 1 MiB run result limit.
const MAX_LAYOUT_NODES: usize = 1500;
const MAX_LAYOUT_CELLS: usize = 1500;
const MAX_LAYOUT_ROWS: usize = 1000;
const MAX_LAYOUT_CONTROLS: usize = 1000;
const MAX_LAYOUT_MEMOS: usize = 500;
/// Diagnostic code of a run ended at a person's approval.
pub(crate) const HUMAN_ONLY: &str = "HUMAN_ONLY";

/// How the synthetic input was turned on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum UiTestMode {
    Visible,
    Offscreen,
}

pub(crate) fn ui_test_mode_from_env() -> Option<UiTestMode> {
    mode_of(std::env::var(UI_TEST_INPUT_ENV).ok()?.as_str())
}

fn mode_of(value: &str) -> Option<UiTestMode> {
    match value.trim() {
        "1" | "on" => Some(UiTestMode::Visible),
        "offscreen" => Some(UiTestMode::Offscreen),
        _ => None,
    }
}

/// The zoom factor of a UI-test window (`DESIGNLLM_UI_TEST_SCALE`); None when the input is off or the value is not one of
/// the three scales.
pub(crate) fn ui_test_scale(mode: Option<UiTestMode>, value: Option<&str>) -> Option<f32> {
    mode?;
    test_scale(value?)
}

pub(crate) fn ui_test_scale_from_env() -> Option<f32> {
    ui_test_scale(ui_test_mode_from_env(), std::env::var(UI_TEST_SCALE_ENV).ok().as_deref())
}

fn test_scale(value: &str) -> Option<f32> {
    let scale = value.trim().parse::<f32>().ok()?;
    [1.0, 1.25, 1.5, 1.75, 2.0].contains(&scale).then_some(scale)
}

/// How a launch shows its window: the UI-test mode (never with a smoke capture), whether the window is on screen, and
/// whether native dialogs are refused (`lib.rs run_app`).
pub(crate) fn launch_mode(smoke: bool, value: Option<&str>) -> (Option<UiTestMode>, bool, bool) {
    let mode = if smoke { None } else { value.and_then(mode_of) };
    let visible = !smoke && mode != Some(UiTestMode::Offscreen);
    (mode, visible, !visible)
}

/// Every intent: a `ui.input` run may change whatever a person at the window can.
const ALL_INTENTS: [Intent; 3] = [Intent::Content, Intent::Design, Intent::Data];

/// One frame's worth of a run: the events it adds and the modifiers it holds. `step` is None for a frame left empty
/// between steps.
#[derive(Debug, PartialEq)]
struct PlannedFrame {
    step: Option<usize>,
    events: Vec<Event>,
    modifiers: Option<Modifiers>,
}

struct Playing {
    run_id: String,
    actor: String,
    /// The video insertion that was under way when the run started (a person's): not the run's flow.
    video_before: Option<String>,
    /// The run must be completed before this.
    deadline: Instant,
    /// The settle limit; None when the run does not settle.
    settle: Option<Duration>,
    settle_until: Option<Instant>,
    quiet: u32,
    steps: Vec<UiInputStepFrames>,
}

/// The actor a finished run keeps for the window's late requests (a debounced commit), until `until`.
struct Pin {
    actor: String,
    until: Instant,
}

/// The synthetic input of a UI-test window.
#[derive(Default)]
pub(crate) struct UiTestInput {
    /// Frames the window started, counted in `raw_input_hook`.
    frame: u64,
    plan: VecDeque<PlannedFrame>,
    playing: Option<Playing>,
    /// After a run: its actor for the requests its input still causes.
    after: Option<Pin>,
    /// The frame being drawn carries a person's input (a visible test window): its requests are not pinned.
    human_frame: bool,
    /// The layer rows of the frame being drawn, and of the last finished frame.
    rows: Vec<UiLayoutRow>,
    last_rows: Vec<UiLayoutRow>,
    last_controls: Vec<ui_record::Control>,
    last_frame: u64,
    /// The caret (IME cursor rectangle) the last frame showed.
    last_caret: Option<Rect>,
    /// A flow a run started that outlives it (a video insertion): its token and the run's actor. Its requests keep that actor
    /// and the agent path rules for as long as the flow lives, whatever the settle time.
    flow: Option<(String, String)>,
    /// Whether `flow` is still the window's flow this frame.
    flow_alive: bool,
    /// The frame in which the window last refused a model run (`refuse_ui_test_run`).
    refused_frame: Option<u64>,
}

impl UiTestInput {
    /// A layer list row the sidebar drew this frame (`clip`: the list's visible part).
    pub(super) fn note_layer_row(&mut self, node_id: Option<&str>, page_index: Option<u32>, label: &str, rect: Rect, clip: Rect) {
        self.rows.push(UiLayoutRow {
            node_id: node_id.map(str::to_owned),
            page_index,
            label: label.to_owned(),
            rect: point_rect(rect),
            visible: clip.intersects(rect),
        });
    }

    /// Tests: the human-only controls of the last frame by label and widget id (to give one the keyboard focus).
    #[cfg(test)]
    pub(super) fn human_only_ids(&self) -> Vec<(String, egui::Id)> {
        self.last_controls.iter().filter(|control| control.human_only).map(|control| (control.label.clone(), control.id)).collect()
    }

    /// Whether a run is playing or settling.
    pub(super) fn busy(&self) -> bool {
        self.playing.is_some()
    }

    /// The actor the window's requests take now: a playing run's, or a finished run's within its settle time; none in a
    /// frame that carries a person's input.
    fn pinned_actor(&self, now: Instant) -> Option<&str> {
        if self.human_frame {
            return None;
        }
        match (&self.playing, &self.after) {
            (Some(playing), _) => Some(playing.actor.as_str()),
            (None, Some(pin)) if now < pin.until => Some(pin.actor.as_str()),
            _ => self.flow.as_ref().filter(|_| self.flow_alive).map(|(_, actor)| actor.as_str()),
        }
    }
}

/// In a run's context (`pinned_actor`), a request of the window that names the `desktop` actor (a ChangeSet's `actor`, a
/// reservation's `owner`, which the ChangeSet that follows must match) names the run's actor instead, and a request that
/// takes a file path is marked for the agent path rules: the path came from the run (a queued dialog answer, typed text).
pub(super) fn pin_ui_test_request(test: Option<&UiTestInput>, method: &str, params: &mut Value) {
    let Some(actor) = test.and_then(|test| test.pinned_actor(Instant::now())) else {
        return;
    };
    for key in ["actor", "owner"] {
        if let Some(slot) = params.get_mut(key).filter(|slot| slot.as_str() == Some("desktop")) {
            *slot = Value::String(actor.to_owned());
        }
    }
    if designllm_contracts::local_path::AGENT_PATH_METHODS.contains(&method) {
        if let Some(object) = params.as_object_mut() {
            object.insert(designllm_contracts::local_path::AGENT_PATH_PARAM.into(), Value::Bool(true));
        }
    }
}

/// Why a path the run gave (a dialog answer, a file the run's flow is about to read) may not be used: the agent path rules
/// (66c, `designllm_contracts::local_path::agent_file_refusal`), checked before any file is opened.
pub(crate) fn dialog_answer_refusal(raw: &str) -> Option<String> {
    designllm_contracts::local_path::agent_file_refusal(raw)
}

/// Whether an event is input a person gives (not a focus or window notice).
fn is_person_input(event: &Event) -> bool {
    matches!(
        event,
        Event::PointerButton { .. } | Event::Key { .. } | Event::Text(_) | Event::Ime(_) | Event::MouseWheel { .. } | Event::Paste(_)
            | Event::Copy | Event::Cut
    )
}

fn human_only_error(message: impl Into<String>) -> DltError {
    let mut error = DltError::new(ErrorCode::ContentLocked, format!("{HUMAN_ONLY}: {}", message.into()));
    error.diagnostics.push(Diagnostic {
        code: HUMAN_ONLY.into(),
        message: "사람만 누를 수 있는 승인입니다. 합성 입력은 이 승인을 대신할 수 없습니다".into(),
        node_id: None,
        blocking: true,
        params: Default::default(),
    });
    error
}

pub(super) fn is_ui_test_command(id: &str) -> bool {
    matches!(id, UI_INPUT_COMMAND | UI_LAYOUT_COMMAND)
}

fn point_rect(rect: Rect) -> UiPointRect {
    UiPointRect { x: rect.min.x, y: rect.min.y, width: rect.width(), height: rect.height() }
}

fn modifiers(m: UiInputModifiers) -> Modifiers {
    Modifiers { alt: m.alt, ctrl: m.ctrl, shift: m.shift, mac_cmd: false, command: m.ctrl }
}

fn button(button: UiPointerButton) -> PointerButton {
    match button {
        UiPointerButton::Primary => PointerButton::Primary,
        UiPointerButton::Secondary => PointerButton::Secondary,
        UiPointerButton::Middle => PointerButton::Middle,
    }
}

fn point(x: f32, y: f32) -> Result<Pos2, String> {
    if x.is_finite() && y.is_finite() && x.abs() <= COORDINATE_LIMIT && y.abs() <= COORDINATE_LIMIT {
        Ok(Pos2::new(x, y))
    } else {
        Err(format!("좌표 ({x}, {y})는 ±{COORDINATE_LIMIT} 안의 수여야 합니다"))
    }
}

fn key(name: &str) -> Result<Key, String> {
    Key::from_name(name).ok_or_else(|| format!("알 수 없는 키 이름입니다: {name}"))
}

fn text(value: &str) -> Result<String, String> {
    if value.chars().count() > MAX_TEXT_CHARS || value.chars().any(char::is_control) {
        return Err(format!("text는 제어 문자 없이 {MAX_TEXT_CHARS}자 이하여야 합니다"));
    }
    Ok(value.to_owned())
}

fn key_event(key: Key, pressed: bool, held: Modifiers) -> Event {
    Event::Key { key, physical_key: Some(key), pressed, repeat: false, modifiers: held }
}

/// The frames of one event, each a list of egui events with the modifiers held.
fn frames_of(event: &UiInputEvent) -> Result<Vec<(Vec<Event>, Option<Modifiers>)>, String> {
    let pointer = |pos: Pos2, kind: UiPointerButton, pressed: bool, held: Modifiers| Event::PointerButton {
        pos,
        button: button(kind),
        pressed,
        modifiers: held,
    };
    Ok(match event {
        UiInputEvent::Move { x, y } => vec![(vec![Event::PointerMoved(point(*x, *y)?)], None)],
        UiInputEvent::Press { x, y, button, modifiers: m } | UiInputEvent::Release { x, y, button, modifiers: m } => {
            let pressed = matches!(event, UiInputEvent::Press { .. });
            let (pos, held) = (point(*x, *y)?, modifiers(*m));
            vec![(vec![Event::PointerMoved(pos), pointer(pos, *button, pressed, held)], Some(held))]
        }
        UiInputEvent::Click { x, y, button, count, modifiers: m } => {
            if !(1..=MAX_CLICKS).contains(count) {
                return Err(format!("click count는 1~{MAX_CLICKS}입니다"));
            }
            let (pos, held) = (point(*x, *y)?, modifiers(*m));
            let mut frames = vec![(vec![Event::PointerMoved(pos)], Some(held))];
            for _ in 0..*count {
                frames.push((vec![pointer(pos, *button, true, held)], Some(held)));
                frames.push((vec![pointer(pos, *button, false, held)], Some(held)));
            }
            frames
        }
        UiInputEvent::Drag { from, to, steps, button, modifiers: m } => {
            if !(1..=MAX_DRAG_STEPS).contains(steps) {
                return Err(format!("drag steps는 1~{MAX_DRAG_STEPS}입니다"));
            }
            let (start, end, held) = (point(from[0], from[1])?, point(to[0], to[1])?, modifiers(*m));
            let mut frames = vec![(vec![Event::PointerMoved(start)], Some(held)), (vec![pointer(start, *button, true, held)], Some(held))];
            for step in 1..=*steps {
                frames.push((vec![Event::PointerMoved(start + (end - start) * (step as f32 / *steps as f32))], Some(held)));
            }
            frames.push((vec![pointer(end, *button, false, held)], Some(held)));
            frames
        }
        UiInputEvent::Wheel { x, y, dx, dy, unit, modifiers: m } => {
            if !dx.is_finite() || !dy.is_finite() || dx.abs() > COORDINATE_LIMIT || dy.abs() > COORDINATE_LIMIT {
                return Err("wheel dx·dy는 유한한 수여야 합니다".into());
            }
            let (pos, held) = (point(*x, *y)?, modifiers(*m));
            let unit = match unit {
                UiWheelUnit::Point => MouseWheelUnit::Point,
                UiWheelUnit::Line => MouseWheelUnit::Line,
            };
            vec![
                (vec![Event::PointerMoved(pos)], Some(held)),
                (vec![Event::MouseWheel { unit, delta: Vec2::new(*dx, *dy), modifiers: held }], Some(held)),
            ]
        }
        UiInputEvent::KeyDown { key: name, modifiers: m } => vec![(vec![key_event(key(name)?, true, modifiers(*m))], Some(modifiers(*m)))],
        UiInputEvent::KeyUp { key: name, modifiers: m } => vec![(vec![key_event(key(name)?, false, modifiers(*m))], Some(modifiers(*m)))],
        UiInputEvent::Key { key: name, modifiers: m } => {
            let (key, held) = (key(name)?, modifiers(*m));
            vec![(vec![key_event(key, true, held)], Some(held)), (vec![key_event(key, false, held)], Some(held))]
        }
        UiInputEvent::Text { text: value } => vec![(vec![Event::Text(text(value)?)], None)],
        UiInputEvent::ImeEnabled => vec![(vec![Event::Ime(ImeEvent::Enabled)], None)],
        UiInputEvent::ImePreedit { text: value } => vec![(vec![Event::Ime(ImeEvent::Preedit(text(value)?))], None)],
        UiInputEvent::ImeCommit { text: value } => vec![(vec![Event::Ime(ImeEvent::Commit(text(value)?))], None)],
        UiInputEvent::ImeDisabled => vec![(vec![Event::Ime(ImeEvent::Disabled)], None)],
        UiInputEvent::Wait { frames } => {
            if !(1..=MAX_GAP_FRAMES).contains(frames) {
                return Err(format!("wait frames는 1~{MAX_GAP_FRAMES}입니다"));
            }
            (0..*frames).map(|_| (Vec::new(), None)).collect()
        }
    })
}

/// The frames of a request, in play order. Each step's frames follow one another; `after_frames` - 1 empty frames follow
/// a step.
fn plan(request: &UiInputRequest) -> Result<VecDeque<PlannedFrame>, String> {
    if request.events.is_empty() || request.events.len() > MAX_STEPS {
        return Err(format!("events는 1~{MAX_STEPS}개입니다"));
    }
    let mut frames = VecDeque::new();
    for (index, step) in request.events.iter().enumerate() {
        let gap = step.after_frames.unwrap_or(1);
        if !(1..=MAX_GAP_FRAMES).contains(&gap) {
            return Err(format!("{index}번째 단계: after_frames는 1~{MAX_GAP_FRAMES}입니다"));
        }
        let wait = matches!(step.event, UiInputEvent::Wait { .. });
        for (events, modifiers) in frames_of(&step.event).map_err(|message| format!("{index}번째 단계: {message}"))? {
            frames.push_back(PlannedFrame { step: (!wait).then_some(index), events, modifiers });
        }
        frames.extend((1..gap).map(|_| PlannedFrame { step: None, events: Vec::new(), modifiers: None }));
        if frames.len() > MAX_FRAMES {
            return Err(format!("한 실행의 프레임은 {MAX_FRAMES}개 이하입니다"));
        }
    }
    Ok(frames)
}

/// The unique command a control label (a command's name, or its hover text with the shortcut) stands for.
fn command_labels() -> BTreeMap<String, Option<&'static str>> {
    let mut labels: BTreeMap<String, Option<&'static str>> = BTreeMap::new();
    for spec in commands::COMMANDS {
        let texts: BTreeSet<String> = [spec.label.to_owned(), super::chrome::tooltip_for(spec.id)].into_iter().collect();
        for text in texts {
            labels.entry(text).and_modify(|id| *id = None).or_insert(Some(spec.id));
        }
    }
    labels
}

fn refusal(code: ErrorCode, message: impl Into<String>) -> DltError {
    DltError::new(code, message)
}

impl DesktopApp {
    pub(crate) fn with_ui_test_input(mut self, mode: Option<UiTestMode>) -> Self {
        self.ui_test = mode.map(|_| UiTestInput::default());
        // The runner menu asks no CLI in a UI-test window (68-1 checks set DESIGNLLM_UI_TEST_CLI_MODELS=1): the fixed lists answer.
        if mode.is_some() && std::env::var(UI_TEST_CLI_MODELS_ENV).as_deref() != Ok("1") {
            self.runner_models = super::runner_models::RunnerModels::with_lookup(super::runner_models::fixed_lookup);
        }
        self
    }

    /// Why the window may not send `method` now: a UI-test window starts only the mock runtime (task 69 r3), whoever asked
    /// (synthetic input, a person, an agent's run). Model lists are no run (`with_ui_test_input`).
    pub(super) fn ui_test_run_refusal(&self, method: &str, params: &Value) -> Option<&'static str> {
        self.ui_test.as_ref()?;
        let runtime = match method {
            "job.start" => params.get("runtime").and_then(Value::as_str).map(str::to_owned),
            "job.resume" => {
                let id = params.get("job_id").and_then(Value::as_str);
                self.job.iter().chain(self.parked_jobs.iter()).find(|job| Some(job.id.as_str()) == id).map(|job| job.runtime.clone())
            }
            _ => return None,
        };
        (runtime.as_deref() != Some("mock")).then_some(REAL_RUN_REFUSED)
    }

    /// Refuses a model run the window was about to start (`ui_test_run_refusal`): the notice says why, and an agent's run that
    /// asked for it fails with the same reason.
    pub(super) fn refuse_ui_test_run(&mut self, reason: &str) {
        if let Some(run) = self.run_context.as_mut() {
            run.mutation_attempted = true;
            if run.await_rpc_reply {
                run.enqueue_error = Some(DltError::new(ErrorCode::UnsupportedOperation, reason));
            }
        }
        if let Some(test) = self.ui_test.as_mut() {
            test.refused_frame = Some(test.frame);
        }
        self.set_error(reason.to_owned());
    }

    /// A caller that reports its own failure after a refused model run (the home box) puts the reason back on top.
    pub(super) fn repeat_ui_test_refusal(&mut self) {
        if self.ui_test.as_ref().is_some_and(|test| test.refused_frame == Some(test.frame)) {
            self.set_error(REAL_RUN_REFUSED.to_owned());
        }
    }

    /// Why a file the run's context gave may not be read now (the agent path rules again, at the moment of reading). None
    /// outside a run's context, where the path is the person's.
    pub(super) fn ui_test_path_refusal(&self, path: Option<&Path>) -> Option<String> {
        self.ui_test.as_ref()?.pinned_actor(Instant::now())?;
        let path = path?;
        dialog_answer_refusal(&path.to_string_lossy())
    }

    /// The registry entries of a UI-test window (none otherwise).
    pub(super) fn ui_test_commands(&self) -> Vec<UiCommand> {
        if self.ui_test.is_none() {
            return Vec::new();
        }
        vec![
            UiCommand {
                id: UI_INPUT_COMMAND.into(),
                label: "시험용 입력".into(),
                args_schema: designllm_contracts::ui_input_args_schema(),
                intents: ALL_INTENTS.into_iter().collect(),
                permission: UiPermission::AgentStart,
                enabled: true,
                shortcut: None,
            },
            UiCommand {
                id: UI_LAYOUT_COMMAND.into(),
                label: "화면 좌표 읽기".into(),
                args_schema: json!({ "type": "object", "properties": {}, "additionalProperties": false }),
                intents: BTreeSet::new(),
                permission: UiPermission::Read,
                enabled: true,
                shortcut: None,
            },
        ]
    }

    /// An approval only a person gives is on screen: the model run an agent asked for, the close over held edits or
    /// drafts, the export losses, the switch away from a Figure with work of its own, the lost host, or the draft recovery.
    pub(super) fn ui_test_approval_open(&self) -> bool {
        self.has_home_confirmation()
            || self.close_confirmation_open
            || self.export_confirmation.is_some()
            || self.pending_switch.is_some()
            || self.host_link.lost
            || !self.drafts.recovered().is_empty()
    }

    /// The owner of an inline edit started now: `<run actor>:inline:<view>` in a run's context, so that its reservation and
    /// its ChangeSets are the agent's (81); None otherwise (the window's own `desktop:inline:<view>`).
    pub(super) fn ui_test_inline_actor(&self, view_id: &str) -> Option<String> {
        let actor = self.ui_test.as_ref()?.pinned_actor(Instant::now())?;
        Some(format!("{actor}:inline:{view_id}"))
    }

    /// Ends the playing run with `error`: what was not played is dropped, and the dialog stand-in is removed.
    fn abort_ui_test_run(&mut self, error: DltError) {
        let Some(test) = self.ui_test.as_mut() else { return };
        test.plan.clear();
        let Some(playing) = test.playing.take() else { return };
        test.after = playing.settle.map(|limit| Pin { actor: playing.actor.clone(), until: Instant::now() + limit });
        crate::dialogs::set_test_answers(None);
        self.complete_ui_run(&playing.run_id, None, Some(error));
    }

    /// The top of `raw_input_hook`: a new frame starts, and the next planned frame of a playing run adds its events, after
    /// what a person gave in the same frame. A synthetic event during an approval, or a press on a human-only control of the
    /// last frame, ends the run instead.
    pub(super) fn inject_ui_test_input(&mut self, ctx: &Context, input: &mut RawInput) {
        let approval = self.ui_test_approval_open();
        let focused = ctx.memory(|memory| memory.focused());
        let Some(test) = self.ui_test.as_mut() else {
            return;
        };
        test.frame += 1;
        test.rows.clear();
        test.human_frame = input.events.iter().any(is_person_input);
        ui_record::begin();
        let Some(next) = test.plan.pop_front() else {
            return;
        };
        if !next.events.is_empty() {
            // A human-only control is refused whether it is pressed or, focused, activated from the keyboard (Enter, Space, text).
            let focused_human_only = focused.is_some_and(|id| test.last_controls.iter().any(|control| control.human_only && control.id == id));
            let pressed_on_approval = next.events.iter().any(|event| match event {
                Event::PointerButton { pos, pressed: true, .. } => {
                    test.last_controls.iter().any(|control| control.human_only && control.rect.contains(*pos))
                }
                Event::Key { pressed: true, .. } | Event::Text(_) | Event::Ime(_) => focused_human_only,
                _ => false,
            });
            if approval || pressed_on_approval {
                let message = if approval {
                    "승인 대화상자가 열린 동안에는 합성 입력을 넣지 않습니다"
                } else {
                    "사람만 누를 수 있는 컨트롤을 눌렀습니다"
                };
                self.abort_ui_test_run(human_only_error(message));
                return;
            }
        }
        let frame = test.frame;
        if let (Some(index), Some(playing)) = (next.step, test.playing.as_mut()) {
            match playing.steps.last_mut() {
                Some(step) if step.index == index => step.last_frame = frame,
                _ => playing.steps.push(UiInputStepFrames { index, first_frame: frame, last_frame: frame }),
            }
        }
        if let Some(held) = next.modifiers {
            // Held with what a person holds, not in place of it.
            input.modifiers = input.modifiers | held;
        }
        if !next.events.is_empty() {
            // A person types into the window they look at.
            input.focused = true;
        }
        input.events.extend(next.events);
    }

    /// The end of `update`: the frame's rows and controls become the last frame's, a playing run takes its next frame, and
    /// a run whose frames are played completes once the window settles.
    pub(super) fn ui_test_frame_end(&mut self, ctx: &Context) {
        let idle = self.pending.is_empty();
        let video = self.video_insert.as_ref().map(|pending| pending.intent.token.clone());
        let Some(test) = self.ui_test.as_mut() else {
            return;
        };
        let mut controls = ui_record::finish();
        // egui may run the frame again (a discarded pass): that pass records too, and the last pass is what is shown.
        ui_record::begin();
        // A house control drawn as an approval is recorded twice (as a control, then as human only): keep one, marked.
        let human: Vec<(String, Rect)> = controls.iter().filter(|control| control.human_only).map(|control| (control.label.clone(), control.rect)).collect();
        controls.retain(|control| control.human_only || !human.iter().any(|(label, rect)| *label == control.label && *rect == control.rect));
        test.last_controls = controls;
        test.last_rows = std::mem::take(&mut test.rows);
        test.last_frame = test.frame;
        test.last_caret = ctx.output(|output| output.ime.as_ref().map(|ime| ime.cursor_rect));
        // A video insertion the run started is the run's flow until it ends, whatever the settle time (task 69 r3, H).
        if let (Some(playing), Some(token)) = (test.playing.as_ref(), video.as_ref()) {
            if playing.video_before.as_ref() != Some(token) && test.flow.as_ref().map(|(own, _)| own) != Some(token) {
                test.flow = Some((token.clone(), playing.actor.clone()));
            }
        }
        test.flow_alive = test.flow.as_ref().is_some_and(|(token, _)| video.as_deref() == Some(token.as_str()));
        if !test.flow_alive {
            test.flow = None;
        }
        // Polls for runs come every TEST_EVENT_POLL, so the window draws at least that often.
        ctx.request_repaint_after(TEST_EVENT_POLL / 2);
        let now = Instant::now();
        if test.after.as_ref().is_some_and(|pin| now >= pin.until) {
            test.after = None;
        }
        let Some(playing) = test.playing.as_mut() else {
            return;
        };
        if now >= playing.deadline {
            let left = test.plan.len();
            let message = if left > 0 {
                format!("실행 시간 안에 입력을 다 넣지 못했습니다({left}프레임 남음). timeout_ms를 늘리거나 단계를 나눠 주세요")
            } else {
                "실행 시간 안에 화면이 안정되지 않았습니다".to_owned()
            };
            self.abort_ui_test_run(refusal(ErrorCode::Unavailable, message));
            return;
        }
        ctx.request_repaint();
        if !test.plan.is_empty() {
            return;
        }
        let settled = match playing.settle {
            None => Some(false),
            Some(limit) => {
                let until = *playing.settle_until.get_or_insert_with(|| (now + limit).min(playing.deadline));
                playing.quiet = if idle { playing.quiet + 1 } else { 0 };
                if playing.quiet >= QUIET_FRAMES {
                    Some(true)
                } else if now >= until {
                    Some(false)
                } else {
                    None
                }
            }
        };
        let Some(settled) = settled else {
            return;
        };
        let playing = test.playing.take().expect("a run plays");
        // A debounced request the input caused may still leave after the window settled: it keeps the run's actor for the
        // run's settle time (default 5 s when the run did not wait).
        let keep = playing.settle.unwrap_or(Duration::from_millis(DEFAULT_SETTLE_MS));
        test.after = Some(Pin { actor: playing.actor.clone(), until: now + keep });
        let dialog_answers_left = crate::dialogs::set_test_answers(None) as u32;
        let result = UiInputResult {
            command: UI_INPUT_COMMAND.into(),
            steps: playing.steps,
            end_frame: test.frame,
            settled,
            state: self.ui_state(),
            notice: self.notices.current(now).map(|notice| notice.text.clone()),
            dialog_answers_left,
            truncated: None,
        };
        let mut result = result;
        fit_input_result(&mut result);
        match serde_json::to_value(result) {
            Ok(value) => self.complete_ui_run(&playing.run_id, Some(value), None),
            Err(error) => self.complete_ui_run(&playing.run_id, None, Some(refusal(ErrorCode::Internal, error.to_string()))),
        }
    }

    /// Runs a delivered `ui.input` or `ui.layout`: a layout read completes at once, an input run starts playing.
    pub(super) fn execute_ui_test_run(&mut self, run: UiRun) {
        let run_id = run.run_id.clone();
        match self.start_ui_test_run(run) {
            Ok(Some(result)) => self.complete_ui_run(&run_id, Some(result), None),
            Ok(None) => {}
            Err(error) => self.complete_ui_run(&run_id, None, Some(error)),
        }
    }

    /// The checks of a UI-test run. These commands are not in `commands::COMMANDS`, so `ui_run_refusal` (which looks commands
    /// up there) does not judge them: the checks that apply to them are made here (test mode, class, intents, the
    /// queue-time state, the approvals, a lost host).
    fn start_ui_test_run(&mut self, run: UiRun) -> Result<Option<Value>, DltError> {
        if self.ui_test.is_none() {
            return Err(refusal(ErrorCode::NotFound, format!("{}: 시험용 입력으로 시작한 창이 아니라 이 명령이 없습니다", run.command_id)));
        }
        if run.command_id == UI_LAYOUT_COMMAND {
            if !run.permission.allows(UiPermission::Read) {
                return Err(refusal(ErrorCode::InvalidArgument, format!("{UI_LAYOUT_COMMAND}: 실행 권한이 부족합니다")));
            }
            let layout = self.ui_layout();
            return serde_json::to_value(layout).map(Some).map_err(|error| refusal(ErrorCode::Internal, error.to_string()));
        }
        if !run.permission.allows(UiPermission::AgentStart) {
            return Err(refusal(ErrorCode::InvalidArgument, format!("{UI_INPUT_COMMAND}: agent_start 권한이 필요합니다")));
        }
        if !ALL_INTENTS.iter().all(|intent| run.allowed_intents.contains(intent)) {
            return Err(refusal(ErrorCode::ContentLocked, format!("{UI_INPUT_COMMAND}: content·design·data를 모두 허용해야 합니다")));
        }
        if self.host_lost() {
            return Err(refusal(ErrorCode::Unavailable, "호스트와 연결이 끊겨 실행할 수 없습니다"));
        }
        if self.ui_test_approval_open() {
            return Err(human_only_error("승인 대화상자가 열려 있어 합성 입력을 시작하지 않았습니다"));
        }
        if let Some(error) = self.ui_expected_refusal(run.expected.as_ref()) {
            return Err(error);
        }
        if self.ui_test.as_ref().is_some_and(UiTestInput::busy) {
            return Err(refusal(ErrorCode::TargetBusy, "다른 시험용 입력을 넣는 중입니다"));
        }
        let request: UiInputRequest =
            serde_json::from_value(run.args).map_err(|error| refusal(ErrorCode::InvalidArgument, format!("{UI_INPUT_COMMAND} 인자: {error}")))?;
        let settle = match (request.settle, request.settle_timeout_ms) {
            (false, _) => None,
            (true, Some(ms)) if ms == 0 || ms > MAX_SETTLE_MS => {
                return Err(refusal(ErrorCode::InvalidArgument, format!("settle_timeout_ms는 1~{MAX_SETTLE_MS}입니다")));
            }
            (true, ms) => Some(Duration::from_millis(ms.unwrap_or(DEFAULT_SETTLE_MS))),
        };
        if request.dialog_answers.len() > MAX_DIALOG_ANSWERS {
            return Err(refusal(ErrorCode::InvalidArgument, format!("dialog_answers는 {MAX_DIALOG_ANSWERS}개 이하입니다")));
        }
        // Checked before any file is opened: a refused path reaches neither the window's own readers nor the core.
        if let Some(reason) = request.dialog_answers.iter().find_map(|raw| dialog_answer_refusal(raw)) {
            return Err(refusal(ErrorCode::InvalidArgument, format!("dialog_answers: {reason}")));
        }
        let frames = plan(&request).map_err(|message| refusal(ErrorCode::InvalidArgument, message))?;
        // As every run: time is left for the RPC that reports the outcome (`execute_ui_run`).
        let deadline = Instant::now() + Duration::from_millis(run.timeout_ms - (run.timeout_ms / 2).min(2_000));
        let video_before = self.video_insert.as_ref().map(|pending| pending.intent.token.clone());
        crate::dialogs::set_test_answers(Some(request.dialog_answers.iter().map(PathBuf::from).collect()));
        let test = self.ui_test.as_mut().expect("checked above");
        test.plan = frames;
        test.after = None;
        test.playing = Some(Playing {
            run_id: run.run_id,
            actor: run.actor,
            video_before,
            deadline,
            settle,
            settle_until: None,
            quiet: 0,
            steps: Vec::new(),
        });
        if let Some(ctx) = self.exit_ctx.as_ref() {
            ctx.request_repaint();
        }
        Ok(None)
    }

    /// Where the last frame drew things, in window points.
    pub(super) fn ui_layout(&self) -> UiLayout {
        let ctx = self.exit_ctx.as_ref();
        let screen = ctx.map_or(Rect::NOTHING, Context::content_rect);
        let mut layout = UiLayout {
            frame: self.ui_test.as_ref().map_or(0, |test| test.last_frame),
            pixels_per_point: ctx.map_or(1.0, Context::pixels_per_point),
            window: point_rect(screen),
            panels: Vec::new(),
            pages: Vec::new(),
            nodes: Vec::new(),
            cells: Vec::new(),
            memos: Vec::new(),
            layer_rows: self.ui_test.as_ref().map(|test| test.last_rows.clone()).unwrap_or_default(),
            controls: Vec::new(),
            truncated: false,
            truncated_reason: None,
            caret: self.ui_test.as_ref().and_then(|test| test.last_caret).map(point_rect),
        };
        if let Some(test) = self.ui_test.as_ref() {
            let labels = command_labels();
            layout.controls = test
                .last_controls
                .iter()
                .map(|control| UiLayoutControl {
                    label: control.label.clone(),
                    command_id: labels.get(&control.label).copied().flatten().map(str::to_owned),
                    rect: point_rect(control.rect),
                    enabled: control.enabled,
                    visible: control.visible,
                    human_only: control.human_only,
                })
                .collect();
        }
        let view = self.view_ref();
        if let Some(ctx) = ctx {
            let mut panel = |id: &str, rect: Option<Rect>, tab: Option<&str>| {
                if let Some(rect) = rect.filter(|rect| rect.is_positive()) {
                    layout.panels.push(UiLayoutPanel { id: id.into(), rect: point_rect(rect), tab: tab.map(str::to_owned) });
                }
            };
            let stored = |id: &str| egui::panel::PanelState::load(ctx, egui::Id::new(id)).map(|state| state.rect);
            panel("top_bar", stored("top-bar"), None);
            if self.screen_is_editor() {
                if view.left_open {
                    let docked = frame::editor_frame(screen, true, view.left_width, view.right_open, view.right_width).left_docked;
                    let rect = if docked { stored("sidebar") } else { ctx.memory(|memory| memory.area_rect(egui::Id::new("sidebar-overlay"))) };
                    let ai = view.left_tab == LeftTab::Ai;
                    panel("sidebar", rect, Some(if ai { "ai" } else { "layers" }));
                    if ai {
                        panel("ai", rect, None);
                    }
                }
                if view.right_open {
                    panel("properties", stored("properties"), None);
                }
                panel("board", self.board_rect, None);
            }
        }
        if let (Some(board_rect), Some(active), true) = (self.board_rect, self.active.as_ref(), self.screen_is_editor()) {
            let origin = board_rect.min + view.pan;
            let sizes = self.page_sizes();
            let mut page_rects: BTreeMap<u32, Rect> = BTreeMap::new();
            for slot in board_geometry::arrange(active.kind, &sizes, view.zoom) {
                let rect = board_geometry::to_screen(slot.world, origin, view.zoom);
                page_rects.insert(slot.page_index, rect);
                layout.pages.push(UiLayoutPage { page_index: slot.page_index, rect: point_rect(rect), visible: board_geometry::visible(rect, board_rect) });
            }
            let pages: Vec<(&str, &[Node])> = match self.artifact.as_ref().map(|artifact| &artifact.body) {
                Some(ArtifactBody::Canvas(document)) => document.pages.iter().map(|page| (page.id.as_str(), page.nodes.as_slice())).collect(),
                Some(ArtifactBody::Slides(document)) => document.pages.iter().map(|page| (page.id.as_str(), page.nodes.as_slice())).collect(),
                _ => Vec::new(),
            };
            let frames: BTreeMap<String, Rect> = self.node_frames(origin, view.zoom).into_iter().collect();
            fn walk<'a>(node: &'a Node, page: u32, out: &mut Vec<(&'a Node, u32)>) {
                out.push((node, page));
                node.children.iter().for_each(|child| walk(child, page, out));
            }
            let mut nodes = Vec::new();
            for (index, (_, page_nodes)) in pages.iter().enumerate() {
                page_nodes.iter().for_each(|node| walk(node, index as u32, &mut nodes));
            }
            let shown = |rect: Rect| rect.intersects(board_rect);
            for (node, page_index) in nodes {
                let Some(rect) = frames.get(&node.id).copied() else { continue };
                layout.nodes.push(UiLayoutNode { node_id: node.id.clone(), page_index, rect: point_rect(rect), visible: shown(rect) });
                if let Some(table) = node.ext.table.as_ref() {
                    for (row, cells) in table.rows.iter().enumerate() {
                        for (column, cell_id) in cells.cell_ids.iter().enumerate() {
                            if let Some(rect) = frames.get(cell_id).copied() {
                                layout.cells.push(UiLayoutCell {
                                    table_id: node.id.clone(),
                                    cell_id: cell_id.clone(),
                                    row: row as u32,
                                    column: column as u32,
                                    rect: point_rect(rect),
                                    visible: shown(rect),
                                });
                            }
                        }
                    }
                }
            }
            if let Some(ArtifactBody::Canvas(document)) = self.artifact.as_ref().map(|artifact| &artifact.body) {
                for memo in &document.board_memos {
                    let Some(index) = pages.iter().position(|(id, _)| *id == memo.page_id) else { continue };
                    let (Some(page_rect), Some(size)) = (page_rects.get(&(index as u32)), sizes.get(index)) else { continue };
                    let rect = super::page_to_screen(memo.bounds, *size, *page_rect);
                    layout.memos.push(UiLayoutMemo { memo_id: memo.id.clone(), page_index: index as u32, rect: point_rect(rect), visible: shown(rect) });
                }
            }
        }
        cut_layout(&mut layout);
        layout
    }
}

/// The JSON size a run result may reach: the core's limit less room for the completion request around it.
const RESULT_BUDGET: usize = designllm_contracts::UI_RESULT_MAX_BYTES - 64 * 1024;
const MAX_LABEL_CHARS: usize = 200;

fn json_size<T: serde::Serialize>(value: &T) -> usize {
    serde_json::to_vec(value).map_or(usize::MAX, |bytes| bytes.len())
}

fn cut_label(label: &mut String) -> bool {
    if label.chars().count() <= MAX_LABEL_CHARS {
        return false;
    }
    *label = label.chars().take(MAX_LABEL_CHARS).collect::<String>() + "…";
    true
}

/// Keeps a layout under the run result limit: labels are shortened, each list is cut at its maximum (visible entries
/// first), then the longest list is halved until the whole result fits. `truncated_reason` names what was cut.
fn cut_layout(layout: &mut UiLayout) {
    fn cut<T>(name: &str, list: &mut Vec<T>, max: usize, visible: impl Fn(&T) -> bool, cuts: &mut Vec<String>) {
        if list.len() > max {
            let before = list.len();
            list.sort_by_key(|item| !visible(item));
            list.truncate(max);
            cuts.push(format!("{name} {before}->{max}"));
        }
    }
    let mut cuts = Vec::new();
    let mut labels = layout.layer_rows.iter_mut().map(|row| cut_label(&mut row.label)).filter(|cut| *cut).count();
    labels += layout.controls.iter_mut().map(|control| cut_label(&mut control.label)).filter(|cut| *cut).count();
    if labels > 0 {
        cuts.push(format!("{labels} labels to {MAX_LABEL_CHARS} characters"));
    }
    cut("nodes", &mut layout.nodes, MAX_LAYOUT_NODES, |node| node.visible, &mut cuts);
    cut("cells", &mut layout.cells, MAX_LAYOUT_CELLS, |cell| cell.visible, &mut cuts);
    cut("layer_rows", &mut layout.layer_rows, MAX_LAYOUT_ROWS, |row| row.visible, &mut cuts);
    cut("controls", &mut layout.controls, MAX_LAYOUT_CONTROLS, |control| control.visible, &mut cuts);
    cut("memos", &mut layout.memos, MAX_LAYOUT_MEMOS, |memo| memo.visible, &mut cuts);
    cut("pages", &mut layout.pages, MAX_LAYOUT_NODES, |page| page.visible, &mut cuts);
    while json_size(layout) > RESULT_BUDGET {
        let lengths = [layout.nodes.len(), layout.cells.len(), layout.layer_rows.len(), layout.controls.len(), layout.memos.len(), layout.pages.len()];
        let Some((longest, &length)) = lengths.iter().enumerate().max_by_key(|(_, length)| **length) else { break };
        if length == 0 {
            break;
        }
        let half = length / 2;
        match longest {
            0 => cut("nodes", &mut layout.nodes, half, |node| node.visible, &mut cuts),
            1 => cut("cells", &mut layout.cells, half, |cell| cell.visible, &mut cuts),
            2 => cut("layer_rows", &mut layout.layer_rows, half, |row| row.visible, &mut cuts),
            3 => cut("controls", &mut layout.controls, half, |control| control.visible, &mut cuts),
            4 => cut("memos", &mut layout.memos, half, |memo| memo.visible, &mut cuts),
            _ => cut("pages", &mut layout.pages, half, |page| page.visible, &mut cuts),
        }
    }
    layout.truncated = !cuts.is_empty();
    layout.truncated_reason = layout.truncated.then(|| format!("cut to fit the {RESULT_BUDGET}-byte result limit: {}", cuts.join(", ")));
}

/// Keeps an input result under the run result limit: a very large selection or detail of the final state is cut.
fn fit_input_result(result: &mut UiInputResult) {
    if json_size(result) <= RESULT_BUDGET {
        return;
    }
    let ids = result.state.selection.node_ids.len();
    result.state.selection.node_ids.truncate(100);
    result.state.detail = json!({ "ui_test_input": true });
    result.steps.truncate(100);
    result.truncated = Some(format!("state cut to fit the {RESULT_BUDGET}-byte result limit: selection {ids}->100 ids, detail dropped, steps to 100"));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(events: Value) -> UiInputRequest {
        serde_json::from_value(json!({ "events": events })).expect("a request")
    }

    fn playing(actor: &str) -> Playing {
        Playing {
            run_id: "r".into(),
            actor: actor.into(),
            video_before: None,
            deadline: Instant::now() + Duration::from_secs(5),
            settle: None,
            settle_until: None,
            quiet: 0,
            steps: Vec::new(),
        }
    }

    #[test]
    fn only_the_documented_values_turn_the_input_on() {
        assert_eq!(mode_of("1"), Some(UiTestMode::Visible));
        assert_eq!(mode_of(" on "), Some(UiTestMode::Visible));
        assert_eq!(mode_of("offscreen"), Some(UiTestMode::Offscreen));
        for off in ["", "0", "true", "yes", "OFFSCREEN"] {
            assert_eq!(mode_of(off), None, "{off:?}");
        }
    }

    /// The launch's choices (`lib.rs run_app`): a smoke capture never takes the input; `offscreen` hides the window and
    /// refuses native dialogs; `1` keeps the window and its dialogs.
    #[test]
    fn the_variable_decides_the_mode_the_window_placement_and_the_dialogs() {
        assert_eq!(launch_mode(false, None), (None, true, false));
        assert_eq!(launch_mode(false, Some("1")), (Some(UiTestMode::Visible), true, false));
        assert_eq!(launch_mode(false, Some("offscreen")), (Some(UiTestMode::Offscreen), false, true));
        assert_eq!(launch_mode(true, Some("offscreen")), (None, false, true), "a smoke capture keeps its own rules");
        assert_eq!(launch_mode(false, Some("yes")), (None, true, false));
    }

    #[test]
    fn only_supported_test_scales_are_accepted_and_only_in_test_mode() {
        for value in ["1.0", "1.25", "1.5", "1.75", "2.0"] {
            assert_eq!(ui_test_scale(Some(UiTestMode::Offscreen), Some(value)), value.parse::<f32>().ok());
            assert_eq!(ui_test_scale(None, Some(value)), None, "the scale needs the test mode");
        }
        for value in ["0", "1.3", "NaN", "inf", ""] {
            assert_eq!(test_scale(value), None);
        }
    }

    #[test]
    fn steps_become_frames_in_order_with_their_gaps() {
        let frames = plan(&request(json!([
            { "type": "click", "x": 10.0, "y": 20.0, "count": 2 },
            { "type": "key", "key": "Tab", "modifiers": { "shift": true }, "after_frames": 3 },
            { "type": "wait", "frames": 2 },
            { "type": "ime_commit", "text": "한" },
        ])))
        .expect("a plan");
        let steps: Vec<Option<usize>> = frames.iter().map(|frame| frame.step).collect();
        // Click: move, press, release, press, release. Key: down, up, then two empty frames. Wait: two empty frames.
        assert_eq!(steps, [Some(0), Some(0), Some(0), Some(0), Some(0), Some(1), Some(1), None, None, None, None, Some(3)]);
        assert_eq!(frames[0].events, vec![Event::PointerMoved(Pos2::new(10.0, 20.0))]);
        assert!(matches!(frames[1].events[0], Event::PointerButton { pressed: true, button: PointerButton::Primary, .. }));
        let shift = Modifiers { shift: true, ..Modifiers::NONE };
        assert_eq!(frames[5].events, vec![key_event(Key::Tab, true, shift)]);
        assert_eq!(frames[5].modifiers, Some(shift));
        assert_eq!(frames[11].events, vec![Event::Ime(ImeEvent::Commit("한".into()))]);
    }

    #[test]
    fn a_drag_presses_moves_in_steps_and_releases() {
        let frames = plan(&request(json!([{ "type": "drag", "from": [0.0, 0.0], "to": [40.0, 20.0], "steps": 4, "button": "middle" }]))).expect("a plan");
        assert_eq!(frames.len(), 1 + 1 + 4 + 1);
        assert!(matches!(frames[1].events[0], Event::PointerButton { pressed: true, button: PointerButton::Middle, .. }));
        assert_eq!(frames[3].events, vec![Event::PointerMoved(Pos2::new(20.0, 10.0))]);
        assert!(matches!(frames[6].events[0], Event::PointerButton { pressed: false, pos, .. } if pos == Pos2::new(40.0, 20.0)));
    }

    #[test]
    fn bad_steps_are_refused_with_their_index() {
        for (events, part) in [
            (json!([]), "events"),
            (json!([{ "type": "key", "key": "NoSuchKey" }]), "알 수 없는 키"),
            (json!([{ "type": "click", "x": 1.0, "y": 1.0, "count": 4 }]), "click count"),
            (json!([{ "type": "move", "x": 1.0e9, "y": 1.0 }]), "좌표"),
            (json!([{ "type": "text", "text": "a\nb" }]), "제어 문자"),
            (json!([{ "type": "wait", "frames": 0 }]), "wait frames"),
            (json!([{ "type": "move", "x": 1.0, "y": 1.0 }, { "type": "move", "x": 1.0, "y": 1.0, "after_frames": 0 }]), "1번째 단계"),
        ] {
            let error = plan(&request(events.clone())).expect_err("refused");
            assert!(error.contains(part), "{events}: {error}");
        }
        let many: Vec<Value> = (0..=MAX_STEPS).map(|_| json!({ "type": "move", "x": 1.0, "y": 1.0 })).collect();
        assert!(plan(&request(Value::Array(many))).is_err());
        let long: Vec<Value> = (0..20).map(|_| json!({ "type": "wait", "frames": MAX_GAP_FRAMES })).collect();
        assert!(plan(&request(Value::Array(long))).expect_err("too many frames").contains("프레임"));
    }

    #[test]
    fn a_run_pins_the_window_actor_and_file_paths_but_not_a_person_frame() {
        let mut test = UiTestInput::default();
        let mut params = json!({ "actor": "desktop" });
        pin_ui_test_request(Some(&test), "changes.apply", &mut params);
        assert_eq!(params["actor"], "desktop", "no run: the window's own edit");
        test.playing = Some(playing("mcp:c1:agent"));
        pin_ui_test_request(Some(&test), "changes.apply", &mut params);
        assert_eq!(params["actor"], "mcp:c1:agent");
        let mut reserve = json!({ "owner": "desktop", "node_ids": ["n"] });
        pin_ui_test_request(Some(&test), "targets.reserve", &mut reserve);
        assert_eq!(reserve["owner"], "mcp:c1:agent", "the drag's reservation belongs to the actor of its change");
        let mut import = json!({ "path": r"C:\x.png" });
        pin_ui_test_request(Some(&test), "asset.import", &mut import);
        assert_eq!(import[designllm_contracts::local_path::AGENT_PATH_PARAM], true, "a path in a run's context is the agent's");
        let mut other = json!({ "view_id": "v" });
        pin_ui_test_request(Some(&test), "selection.set", &mut other);
        assert_eq!(other, json!({ "view_id": "v" }));
        test.human_frame = true;
        let mut person = json!({ "actor": "desktop" });
        pin_ui_test_request(Some(&test), "changes.apply", &mut person);
        assert_eq!(person["actor"], "desktop", "a frame with a person's input stays theirs");
    }

    #[test]
    fn a_finished_run_keeps_its_actor_for_late_requests_until_its_settle_time_ends() {
        let mut test = UiTestInput { after: Some(Pin { actor: "agent".into(), until: Instant::now() + Duration::from_secs(5) }), ..UiTestInput::default() };
        let mut late = json!({ "actor": "desktop" });
        pin_ui_test_request(Some(&test), "changes.apply", &mut late);
        assert_eq!(late["actor"], "agent");
        test.after = Some(Pin { actor: "agent".into(), until: Instant::now() - Duration::from_millis(1) });
        let mut later = json!({ "actor": "desktop" });
        pin_ui_test_request(Some(&test), "changes.apply", &mut later);
        assert_eq!(later["actor"], "desktop");
    }

    #[test]
    fn a_flow_the_run_started_keeps_its_actor_past_the_settle_time_while_it_lives() {
        let mut test = UiTestInput { flow: Some(("video-1".into(), "mcp:c1:agent".into())), flow_alive: true, ..UiTestInput::default() };
        let mut late = json!({ "actor": "desktop", "path": "x.mp4" });
        pin_ui_test_request(Some(&test), "asset.import", &mut late);
        assert_eq!(late["actor"], "mcp:c1:agent");
        assert_eq!(late[designllm_contracts::local_path::AGENT_PATH_PARAM], true, "the flow's files keep the agent path rules");
        test.flow_alive = false;
        let mut after = json!({ "actor": "desktop" });
        pin_ui_test_request(Some(&test), "changes.apply", &mut after);
        assert_eq!(after["actor"], "desktop", "once the flow ends the window's edits are its own");
    }

    #[test]
    fn dialog_answers_follow_the_agent_path_rules_before_any_file_is_read() {
        let dir = std::env::temp_dir().join(format!("designllm-dialog-answer-{}", designllm_contracts::new_id()));
        std::fs::create_dir_all(&dir).expect("a folder");
        let file = dir.join("picture.png");
        std::fs::write(&file, b"png").expect("a file");
        assert_eq!(dialog_answer_refusal(&file.to_string_lossy()), None);
        for bad in [
            r"\\server\share\picture.png".to_owned(),
            format!(r"\\?\{}", file.display()),
            "picture.png".to_owned(),
            dir.join("missing.png").to_string_lossy().into_owned(),
            dir.to_string_lossy().into_owned(),
            r"C:\CON".to_owned(),
        ] {
            assert!(dialog_answer_refusal(&bad).is_some(), "{bad}");
        }
        std::fs::remove_dir_all(dir).expect("cleaned");
    }

    #[test]
    fn a_command_label_or_hover_text_names_its_command_only_when_unique() {
        let labels = command_labels();
        let toggle = super::super::chrome::tooltip_for("nav.toggle_right");
        assert_eq!(labels.get(&toggle).copied().flatten(), Some("nav.toggle_right"));
    }

    #[test]
    fn every_list_at_its_maximum_still_fits_the_whole_result_limit() {
        let rect = UiPointRect { x: 0.0, y: 0.0, width: 1.0, height: 1.0 };
        let long = "가".repeat(5_000);
        let mut layout = UiLayout {
            frame: 1, pixels_per_point: 1.0, window: rect, panels: Vec::new(),
            pages: (0..3000).map(|i| UiLayoutPage { page_index: i, rect, visible: true }).collect(),
            nodes: (0..3000).map(|i| UiLayoutNode { node_id: format!("node-{i:040}"), page_index: 0, rect, visible: true }).collect(),
            cells: (0..3000).map(|i| UiLayoutCell { table_id: format!("table-{i:040}"), cell_id: format!("cell-{i:040}"), row: i, column: i, rect, visible: true }).collect(),
            memos: (0..3000).map(|i| UiLayoutMemo { memo_id: format!("memo-{i:040}"), page_index: 0, rect, visible: true }).collect(),
            layer_rows: (0..3000).map(|i| UiLayoutRow { node_id: Some(format!("n{i}")), page_index: Some(0), label: long.clone(), rect, visible: true }).collect(),
            controls: (0..3000).map(|_| UiLayoutControl { label: long.clone(), command_id: None, rect, enabled: true, visible: true, human_only: false }).collect(),
            truncated: false, truncated_reason: None, caret: None,
        };
        cut_layout(&mut layout);
        assert!(json_size(&layout) <= RESULT_BUDGET, "{}", json_size(&layout));
        let reason = layout.truncated_reason.clone().expect("a reason");
        assert!(reason.contains("labels") && reason.contains("pages"), "{reason}");
        let mut result = UiInputResult {
            command: UI_INPUT_COMMAND.into(), steps: Vec::new(), end_frame: 1, settled: true, notice: None, dialog_answers_left: 0, truncated: None,
            state: designllm_contracts::UiState { screen: "editor".into(), selection: designllm_contracts::Selection { node_ids: (0..40_000).map(|i| format!("node-{i:040}")).collect(), text_range: None }, ..Default::default() },
        };
        fit_input_result(&mut result);
        assert!(json_size(&result) <= RESULT_BUDGET && result.truncated.is_some());
    }

    #[test]
    fn a_large_layout_is_cut_and_says_so() {
        let rect = UiPointRect { x: 0.0, y: 0.0, width: 1.0, height: 1.0 };
        let mut layout = UiLayout {
            frame: 1, pixels_per_point: 1.0, window: rect, panels: Vec::new(), pages: Vec::new(),
            nodes: (0..MAX_LAYOUT_NODES + 10).map(|i| UiLayoutNode { node_id: format!("n{i}"), page_index: 0, rect, visible: i % 2 == 0 }).collect(),
            cells: Vec::new(), memos: Vec::new(), layer_rows: Vec::new(), controls: Vec::new(), truncated: false, truncated_reason: None, caret: None,
        };
        cut_layout(&mut layout);
        assert!(layout.truncated);
        assert!(layout.truncated_reason.as_deref().is_some_and(|reason| reason.contains("nodes")), "{:?}", layout.truncated_reason);
        assert_eq!(layout.nodes.len(), MAX_LAYOUT_NODES);
        assert!(layout.nodes[..(MAX_LAYOUT_NODES + 10) / 2].iter().all(|node| node.visible), "the visible nodes are kept first");
        assert!(serde_json::to_vec(&layout).expect("json").len() < 1024 * 1024);
    }
}
