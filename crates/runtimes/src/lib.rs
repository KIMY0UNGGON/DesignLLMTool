//! designllm-runtimes: launches the AI agent CLIs for app jobs.
//!
//! - [`detect`]: finds `claude.exe` / `codex.exe` (or a configured mock agent) and reads `--version` and
//!   `--help`. No model call and no login state is read.
//! - [`build_invocation`]: exact model and effort, the job-bound MCP configuration, and the prompt. Nothing
//!   is substituted: an unsupported effort or an unknown runtime is an error.
//! - [`start`] / [`start_with_transcript`]: runs the invocation inside a kill-on-close job object.
//!   [`Attempt::kill_tree`] ends exactly the processes that attempt started.
//! - [`audit`]: checks the model the CLI reports against the requested one (Claude: the init line, Codex: a
//!   `session_configured` event). A mismatch ends the attempt. Each attempt can keep a bounded transcript.
//!   Effort is not reported by either CLI; it is only checked as passed on the command line.
//!
//! The host owns scheduling and calls these functions outside the core lock. Stdout is progress text
//! only; it is never parsed into changes.

#[cfg(not(windows))]
compile_error!("designllm-runtimes uses Windows job objects; it targets Windows only");

pub mod attempt;
pub mod audit;
pub mod detect;
pub mod invocation;
pub mod progress;
#[cfg(windows)]
pub mod win;

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
