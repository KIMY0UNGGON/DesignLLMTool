//! designllm-runtimes: launches the AI agent CLIs for app jobs.
//!
//! - [`detect`]: finds Claude, AGY and Codex executables and checks the capabilities their adapters use.
//!   `detect::logged_in` checks account availability for the desktop picker without a model turn.
//! - [`build_invocation`]: exact model and effort, the job-bound MCP configuration, and the prompt. Nothing
//!   is substituted: an unsupported effort or an unknown runtime is an error.
//! - [`start`] / [`start_with_transcript`]: runs the invocation inside a kill-on-close job object (Windows) or its own
//!   process group (macOS, Linux).
//!   [`Attempt::kill_tree`] ends exactly the processes that attempt started.
//! - [`audit`]: checks the model the CLI reports against the requested one (Claude: the init line, Codex: a
//!   `session_configured` event, AGY: ACP options). A mismatch ends the attempt. Each attempt can keep
//!   a bounded transcript. AGY confirms both model and effort before it receives the request.
//!
//! The host owns scheduling and calls these functions outside the core lock. Stdout is progress text
//! only; it is never parsed into changes.

mod agy;
pub mod attempt;
pub mod audit;
pub mod detect;
pub mod invocation;
pub mod progress;
#[cfg(windows)]
pub mod win;
#[cfg(unix)]
pub mod unix;

/// Process ownership on this platform: a Windows job object, or a Unix process group. Same API on both.
pub mod job {
    #[cfg(unix)]
    pub use crate::unix::*;
    #[cfg(windows)]
    pub use crate::win::*;
}

pub use attempt::{start, start_with_transcript, start_with_transcript_watched, Attempt, JobWatch, WaitOutcome, MAX_LINE_BYTES};
pub use audit::{model_matches, redact, ModelCheck, RunAudit, TRANSCRIPT_CAP_BYTES};
pub use detect::{
    detect, find_on_path, EffortSource, RuntimeConfig, RuntimeInfo, RuntimeKind, CODEX_EFFORTS, DETECT_TIMEOUT,
    MOCK_EFFORTS,
};
pub use invocation::{
    build as build_invocation, claude_mcp_config, prompt_text, validate_effort, validate_model, Invocation,
    LaunchSpec, McpCommand, MCP_SERVER_NAME,
};
pub use progress::ProgressBuffer;
