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
