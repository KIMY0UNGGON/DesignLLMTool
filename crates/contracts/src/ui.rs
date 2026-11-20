//! P1 UI channel (docs/UI_CHANNEL.md): a GUI view publishes its state and command registry, agents
//! read them and run commands that the GUI executes. The core stores and relays these values; it
//! never interprets command ids or arguments.
use crate::{DltError, Id, Intent, Rect, Revision, Selection};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeSet;

/// Command id of the runs `ui.screenshot` queues. A registry may list it, as a `read` command.
pub const UI_SCREENSHOT_COMMAND: &str = "ui.screenshot";
/// Queue-time context contract between a current UI client and the core.
pub const UI_RUN_GUARD_VERSION: u32 = 1;
/// The largest run result (`ui.command.complete` `result`, JSON bytes) the core keeps.
pub const UI_RESULT_MAX_BYTES: usize = 1024 * 1024;

/// Permission class of a GUI command, the same classes as the MCP tools (read, change, file
/// write, agent start).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum UiPermission { Read, Change, FileWrite, AgentStart }
impl UiPermission {
    /// Whether a run granted `self` may execute a command of class `needed`: read < change <
    /// file_write, and an agent_start command only with agent_start.
    pub fn allows(self, needed: UiPermission) -> bool {
        match (self, needed) {
            (UiPermission::AgentStart, n) => n == UiPermission::AgentStart,
            (_, UiPermission::AgentStart) => false,
            (granted, n) => granted >= n,
        }
    }
}

fn object_schema() -> Value { serde_json::json!({ "type": "object" }) }
fn object() -> Value { Value::Object(Default::default()) }
fn yes() -> bool { true }

/// One entry of a view's command registry: every button, menu item and shortcut of the GUI.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct UiCommand {
    /// `[A-Za-z0-9._:-]`, 1..=128 bytes, unique in the registry.
    pub id: String,
    #[serde(default)] pub label: String,
    /// JSON Schema of `args` (an object schema). The GUI checks args when it runs the command.
    #[serde(default = "object_schema")] pub args_schema: Value,
    /// What the command changes; empty for reading and viewing commands.
    #[serde(default)] pub intents: BTreeSet<Intent>,
    pub permission: UiPermission,
    /// Whether the command can run in the current state.
    #[serde(default = "yes")] pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub shortcut: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct UiPanel {
    pub id: String,
    pub open: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub tab: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub width_px: Option<u32>,
}

/// What a GUI view shows. The core checks only that `artifact_id` exists and `revision` is not
/// ahead of it.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct UiState {
    /// GUI-defined screen name, e.g. "home", "editor".
    pub screen: String,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub artifact_id: Option<Id>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub revision: Option<Revision>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub page_index: Option<u32>,
    #[serde(default)] pub selection: Selection,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub zoom_milli: Option<u32>,
    /// Visible part of the page, page EMU.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub viewport: Option<Rect>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")] pub panels: Vec<UiPanel>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub tool: Option<String>,
    /// Anything else the GUI shows, as a JSON object.
    #[serde(default, skip_serializing_if = "Value::is_null")] pub detail: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum UiRunStatus {
    /// Waiting in the view's queue.
    Queued,
    /// Handed to the GUI by `events.poll`, not completed yet.
    Delivered,
    Completed,
    /// The GUI reported an error, or the view stopped responding.
    Failed,
    TimedOut,
}
impl UiRunStatus {
    pub fn is_pending(self) -> bool { matches!(self, Self::Queued | Self::Delivered) }
}

/// The UI state the core captured when it queued a guarded mutating run. The desktop compares
/// this snapshot with its live state immediately before invoking the command.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct UiRunExpected {
    pub state_seq: u64,
    pub screen: String,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub artifact_id: Option<Id>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub revision: Option<Revision>,
    pub selection: Selection,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub page_index: Option<u32>,
}

/// One command run requested by an agent. The GUI uses `actor` and `allowed_intents` for the
/// ChangeSets the run makes.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct UiRun {
    pub run_id: Id,
    pub view_id: Id,
    /// Present for mutating runs queued by a view that advertised `UI_RUN_GUARD_VERSION`.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub expected: Option<UiRunExpected>,
    pub command_id: String,
    pub args: Value,
    pub actor: String,
    pub permission: UiPermission,
    pub allowed_intents: BTreeSet<Intent>,
    pub status: UiRunStatus,
    // Requested timeout in the run record; remaining milliseconds in the events.poll delivery.
    pub timeout_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub error: Option<DltError>,
}

/// `ui.state.publish` params (GUI).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct UiPublishRequest {
    pub view_id: Id,
    /// The token of an earlier publish of this view; absent on the first publish.
    #[serde(default)] pub view_token: Option<String>,
    pub state: UiState,
    /// The whole registry; None keeps the previous one.
    #[serde(default)] pub commands: Option<Vec<UiCommand>>,
    /// Optional queue-time stale-context guard supported by this GUI.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub run_guard_version: Option<u32>,
    /// 100..=60000, default 10000: the view is gone this long after its last publish or poll.
    #[serde(default)] pub ttl_ms: Option<u64>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct UiPublishResult {
    pub view_id: Id,
    pub view_token: String,
    pub state_seq: u64,
    pub ttl_ms: u64,
    pub commands: usize,
    /// The queue-time guard version negotiated with the published GUI view.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub run_guard_version: Option<u32>,
}

/// `ui.state.read` result.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct UiStateView { pub view_id: Id, pub state_seq: u64, pub age_ms: u64, pub ttl_ms: u64, pub state: UiState }
/// `ui.command.list` result.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct UiCommandList { pub view_id: Id, pub state_seq: u64, pub commands: Vec<UiCommand> }

/// `ui.command.run` params (agent).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct UiRunRequest {
    /// Default: the most recently published live view.
    #[serde(default)] pub view_id: Option<Id>,
    pub command_id: String,
    #[serde(default = "object")] pub args: Value,
    pub actor: String,
    /// The class the calling tool grants; it must allow the command's class.
    pub permission: UiPermission,
    #[serde(default)] pub allowed_intents: BTreeSet<Intent>,
    /// 1..=120000, default 15000.
    #[serde(default)] pub timeout_ms: Option<u64>,
    /// Refused (REVISION_CONFLICT) when the view's state_seq differs.
    #[serde(default)] pub expected_state_seq: Option<u64>,
}
/// `ui.screenshot` params (agent).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct UiScreenshotRequest { #[serde(default)] pub view_id: Option<Id>, pub actor: String, #[serde(default)] pub timeout_ms: Option<u64> }
/// `ui.command.complete` params (GUI): exactly one of `result` and `error`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct UiCompleteRequest { pub view_id: Id, pub view_token: String, pub run_id: Id, #[serde(default)] pub result: Option<Value>, #[serde(default)] pub error: Option<DltError> }

/// Command id of the synthetic input runs (docs/UI_CHANNEL.md "시험용 입력"). A view lists it only when the window was started
/// for UI tests. Its input can do whatever a person at the window can, so a registry must give it the `agent_start` class and
/// every intent.
pub const UI_INPUT_COMMAND: &str = "ui.input";
/// Command id of the screen-coordinate read (`UiLayout`) a UI-test view lists with `ui.input`.
pub const UI_LAYOUT_COMMAND: &str = "ui.layout";

/// A pointer button of a synthetic input event.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum UiPointerButton { #[default] Primary, Secondary, Middle }

/// The unit of a synthetic wheel delta.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum UiWheelUnit { #[default] Point, Line }

/// Modifier keys held during a synthetic event.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct UiInputModifiers {
    #[serde(default)] pub ctrl: bool,
    #[serde(default)] pub shift: bool,
    #[serde(default)] pub alt: bool,
}

fn one() -> u8 { 1 }
fn drag_steps() -> u32 { 8 }

/// One synthetic input event. Coordinates are window logical points (egui points), origin at the top left of the window's
/// content. `key` is an egui key name (`Enter`, `Tab`, `Delete`, `F2`, `A`, `ArrowLeft`, ...).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum UiInputEvent {
    Move { x: f32, y: f32 },
    Press { x: f32, y: f32, #[serde(default)] button: UiPointerButton, #[serde(default)] modifiers: UiInputModifiers },
    Release { x: f32, y: f32, #[serde(default)] button: UiPointerButton, #[serde(default)] modifiers: UiInputModifiers },
    /// Move, then `count` (1..=3) press and release pairs, one event per frame.
    Click { x: f32, y: f32, #[serde(default)] button: UiPointerButton, #[serde(default = "one")] count: u8, #[serde(default)] modifiers: UiInputModifiers },
    /// Move to `from`, press, `steps` (1..=120) moves to `to`, release at `to`, one frame each.
    Drag { from: [f32; 2], to: [f32; 2], #[serde(default = "drag_steps")] steps: u32, #[serde(default)] button: UiPointerButton, #[serde(default)] modifiers: UiInputModifiers },
    /// Move to (x, y), then one wheel event. Positive dy moves the content down (egui's sign).
    Wheel { x: f32, y: f32, #[serde(default)] dx: f32, #[serde(default)] dy: f32, #[serde(default)] unit: UiWheelUnit, #[serde(default)] modifiers: UiInputModifiers },
    KeyDown { key: String, #[serde(default)] modifiers: UiInputModifiers },
    KeyUp { key: String, #[serde(default)] modifiers: UiInputModifiers },
    /// KeyDown, then KeyUp in the next frame.
    Key { key: String, #[serde(default)] modifiers: UiInputModifiers },
    Text { text: String },
    ImeEnabled,
    ImePreedit { text: String },
    ImeCommit { text: String },
    ImeDisabled,
    /// Frames without input.
    Wait { frames: u32 },
}

/// One step of a `ui.input` run: an event and the frames until the next step (1..=600, default 1: the next frame).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct UiInputStep {
    #[serde(flatten)] pub event: UiInputEvent,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub after_frames: Option<u32>,
}

/// The JSON Schema of `ui.input` args, for the registry entry of a UI-test view.
pub fn ui_input_args_schema() -> Value { serde_json::to_value(schemars::schema_for!(UiInputRequest)).unwrap_or_else(|_| object_schema()) }

/// `ui.input` args.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct UiInputRequest {
    /// 1..=1000 steps, played in order into the window's next frames.
    pub events: Vec<UiInputStep>,
    /// After the last step, wait until the window has no request in flight (RPC, page render) for two frames. Default true.
    #[serde(default = "yes")] pub settle: bool,
    /// Limit of the settle wait, 1..=60000 ms (default 5000), never past the run's own time.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub settle_timeout_ms: Option<u64>,
    /// Paths the window's file pickers return, in order, while the run plays (at most 16), in place of the native dialog.
    /// Each must be an absolute path to a local file without links or reparse points (the agent path rules); a picker
    /// opened with none left counts as cancelled.
    #[serde(default, skip_serializing_if = "Vec::is_empty")] pub dialog_answers: Vec<String>,
}

/// The frames one step went into (window frame numbers, counted from the window's start).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct UiInputStepFrames { pub index: usize, pub first_frame: u64, pub last_frame: u64 }

/// `ui.input` result.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct UiInputResult {
    pub command: String,
    pub steps: Vec<UiInputStepFrames>,
    /// The last frame of the run: the last input frame, or the last settle frame.
    pub end_frame: u64,
    /// Whether the settle wait saw the window idle before its limit; false when `settle` was off or the limit passed.
    pub settled: bool,
    /// The window's state at the end, as `ui.state.read` would show it.
    pub state: UiState,
    /// The notice the window shows at the end.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub notice: Option<String>,
    /// Queued dialog answers no picker took.
    #[serde(default)] pub dialog_answers_left: u32,
    /// What was cut from `state` to keep the result under the run result limit.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub truncated: Option<String>,
}

/// A rectangle in window logical points.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct UiPointRect { pub x: f32, pub y: f32, pub width: f32, pub height: f32 }

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct UiLayoutPanel { pub id: String, pub rect: UiPointRect, #[serde(default, skip_serializing_if = "Option::is_none")] pub tab: Option<String> }
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct UiLayoutPage { pub page_index: u32, pub rect: UiPointRect, pub visible: bool }
/// A node's frame on the board. `visible`: it meets the board's rectangle.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct UiLayoutNode { pub node_id: Id, pub page_index: u32, pub rect: UiPointRect, pub visible: bool }
/// A table cell's frame on the board, row and column from 0.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct UiLayoutCell { pub table_id: Id, pub cell_id: Id, pub row: u32, pub column: u32, pub rect: UiPointRect, pub visible: bool }
/// A board memo or title card.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct UiLayoutMemo { pub memo_id: String, pub page_index: u32, pub rect: UiPointRect, pub visible: bool }
/// A layer list row as the last frame drew it: a page row (`page_index`) or a node row (`node_id`, with its page).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct UiLayoutRow {
    #[serde(default, skip_serializing_if = "Option::is_none")] pub node_id: Option<Id>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub page_index: Option<u32>,
    pub label: String,
    pub rect: UiPointRect,
    /// Inside the list's scrolled view.
    pub visible: bool,
}
/// A button, tab or menu item the last frame drew, by its label or hover text; `command_id` when it is a registered command's.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct UiLayoutControl {
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub command_id: Option<String>,
    pub rect: UiPointRect,
    pub enabled: bool,
    pub visible: bool,
    /// A person's approval (a model run, discarding held edits, applying an agent's proposal, accepting export losses):
    /// a `ui.input` press on it ends the run with HUMAN_ONLY.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")] pub human_only: bool,
}

/// `ui.layout` result: where the window drew things in its last frame, in window logical points.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct UiLayout {
    /// The window frame the rows and controls come from.
    pub frame: u64,
    pub pixels_per_point: f32,
    pub window: UiPointRect,
    pub panels: Vec<UiLayoutPanel>,
    pub pages: Vec<UiLayoutPage>,
    pub nodes: Vec<UiLayoutNode>,
    pub cells: Vec<UiLayoutCell>,
    pub memos: Vec<UiLayoutMemo>,
    pub layer_rows: Vec<UiLayoutRow>,
    pub controls: Vec<UiLayoutControl>,
    /// Some lists or labels were cut to keep the result under the run result limit.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")] pub truncated: bool,
    /// What was cut, when `truncated`.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub truncated_reason: Option<String>,
    /// The text caret (or IME cursor) the last frame showed, when a text field or an inline edit has it.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub caret: Option<UiPointRect>,
}

#[cfg(test)]
mod input_tests {
    use super::*;

    #[test]
    fn input_steps_take_defaults_and_keep_their_type_tag() {
        let request: UiInputRequest = serde_json::from_value(serde_json::json!({ "events": [
            { "type": "click", "x": 10.0, "y": 20.0 },
            { "type": "drag", "from": [1.0, 2.0], "to": [3.0, 4.0], "button": "middle", "after_frames": 3 },
            { "type": "key", "key": "Delete", "modifiers": { "ctrl": true } },
            { "type": "ime_commit", "text": "한" },
            { "type": "ime_enabled" },
        ] })).expect("a request");
        assert!(request.settle && request.settle_timeout_ms.is_none());
        assert_eq!(request.events[0].event, UiInputEvent::Click { x: 10.0, y: 20.0, button: UiPointerButton::Primary, count: 1, modifiers: UiInputModifiers::default() });
        assert_eq!(request.events[1].after_frames, Some(3));
        assert!(matches!(request.events[1].event, UiInputEvent::Drag { steps: 8, button: UiPointerButton::Middle, .. }));
        assert!(matches!(&request.events[2].event, UiInputEvent::Key { key, modifiers } if key == "Delete" && modifiers.ctrl && !modifiers.shift));
        assert_eq!(request.events[4].event, UiInputEvent::ImeEnabled);
        let back = serde_json::to_value(&request.events[3]).expect("json");
        assert_eq!(back, serde_json::json!({ "type": "ime_commit", "text": "한" }));
        assert!(serde_json::from_value::<UiInputStep>(serde_json::json!({ "type": "teleport" })).is_err());
    }
}
