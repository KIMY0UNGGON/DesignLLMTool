//! designllm-host: the single owner of a workspace's mutable state while it is open.
//!
//! [`serve`] takes a `designllm_core::Workspace` by value and serves it over the per-user named pipe:
//! GUI clients, job-bound MCP bridges, and command-line clients all connect here. The host answers
//! `system.hello` and the `host.*` methods itself. Every other request goes to the core. AI jobs run as
//! child processes outside the core lock, each with a job-bound MCP bridge back to this host.
//!
//! Lifetimes:
//! - [`HostMode::Headless`]: started by the user (`--headless`). Runs until `host.stop`.
//! - [`HostMode::Managed`]: started by the GUI (`--host`). Exits after no client and no active job remain
//!   for the grace period.
//!
//! Root application integration:
//! - `--headless`: `serve(Workspace::open(..)?, HostMode::Headless)`.
//! - `--host`: `serve(Workspace::open(..)?, HostMode::Managed)`. A `WORKSPACE_LOCKED` error means another
//!   host owns the workspace; the caller then joins it (see [`ensure_host`]).
//! - GUI start: [`ensure_host`] connects to a running host or starts a hidden managed one.

#[cfg(not(windows))]
compile_error!("designllm-host serves a Windows named pipe; it targets Windows only");

mod conn;
mod gui_identity;
mod identity;
mod jobs;
mod server;
mod shared;
mod provider;

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use designllm_bridge::{Client, ClientRole};
use designllm_contracts::{DltError, DltResult, ErrorCode};
use designllm_core::Workspace;
use designllm_runtimes::RuntimeConfig;

/// Lifetime policy of a host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostMode {
    /// User-started. Stays up without clients until `host.stop`.
    Headless,
    /// GUI-started (hidden). Exits when no client and no active job remain for the grace period.
    Managed,
}

/// Passed to [`HostOptions::on_ready`] once the pipe is listening.
#[derive(Debug, Clone)]
pub struct HostReady {
    pub pid: u32,
    pub pipe_name: String,
    pub mode: HostMode,
    pub workspace_root: PathBuf,
}

/// Tunables. [`HostOptions::default`] is the product policy; tests shorten the timings.
#[derive(Clone)]
pub struct HostOptions {
    /// Program that runtimes start for `--mcp`. `None` means the current executable.
    pub host_program: Option<PathBuf>,
    /// Executables allowed to hold the gui role besides the application next to the host (`DesignLLMTool.exe`).
    /// Tests and development only: the product sets none.
    pub gui_executables: Vec<PathBuf>,
    /// Executable locations for the runtimes.
    pub runtime: RuntimeConfig,
    /// Managed hosts: time to wait for the first client before exiting.
    pub startup_grace: Duration,
    /// Managed hosts: time with no client and no active job before exiting.
    pub idle_grace: Duration,
    /// A connection with no frame for this long is closed. Clients heartbeat every few seconds.
    pub client_idle_timeout: Duration,
    /// Longest runtime attempt. Beyond this the process tree is ended and the job fails.
    pub max_job_duration: Duration,
    /// Runtime attempts running at the same time. Queued jobs wait in the core.
    pub max_parallel_jobs: usize,
    /// Period of the lifecycle thread (job sweep, event pump, idle check).
    pub tick: Duration,
    /// Called once the pipe is listening.
    pub on_ready: Option<Arc<dyn Fn(&HostReady) + Send + Sync>>,
}

impl Default for HostOptions {
    fn default() -> Self {
        HostOptions {
            host_program: None,
            gui_executables: Vec::new(),
            runtime: RuntimeConfig::from_env(),
            startup_grace: Duration::from_secs(20),
            idle_grace: Duration::from_secs(5),
            client_idle_timeout: Duration::from_secs(30),
            max_job_duration: Duration::from_secs(45 * 60),
            max_parallel_jobs: 2,
            tick: Duration::from_millis(100),
            on_ready: None,
        }
    }
}

/// Serves `workspace` with the product policy. Returns `Ok(())` after host.stop or a managed idle exit.
/// Returns `WORKSPACE_LOCKED` when another host already serves this workspace.
pub fn serve(workspace: Workspace, mode: HostMode) -> DltResult<()> {
    server::serve(workspace, mode, HostOptions::default())
}

/// [`serve`] with explicit options.
pub fn serve_with(workspace: Workspace, mode: HostMode, options: HostOptions) -> DltResult<()> {
    server::serve(workspace, mode, options)
}

/// Starts `host_program --host --workspace <dir>` as a detached process with no console window. The host
/// outlives the caller, and it exits by itself when idle (managed mode).
pub fn spawn_managed_host(host_program: &Path, workspace: &Path) -> DltResult<Child> {
    let mut command = Command::new(host_program);
    command
        .arg("--host")
        .arg("--workspace")
        .arg(workspace)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
    command.spawn().map_err(|e| {
        DltError::new(ErrorCode::HostNotRunning, format!("cannot start the host {}: {e}", host_program.display()))
    })
}

/// Exit status of `--host` when another process owns the workspace (the application maps WORKSPACE_LOCKED to 3).
const EXIT_WORKSPACE_LOCKED: i32 = 3;

/// Makes sure a host serves `workspace`: joins a running one (headless or managed), or starts a hidden
/// managed host and waits until it accepts clients. If another process wins the start race, that host is
/// joined.
///
/// Only HOST_NOT_RUNNING from the first connection leads to a start; any other error is returned unchanged.
/// A started process that exits with a status other than WORKSPACE_LOCKED fails the call at once with
/// HOST_NOT_RUNNING. After `timeout`, WORKSPACE_LOCKED means another host owns the workspace and never became
/// reachable; HOST_NOT_RUNNING means no host did.
pub fn ensure_host(workspace: &Path, host_program: &Path, timeout: Duration) -> DltResult<()> {
    // The probe only asks whether a host answers, so it does not claim the gui role.
    let client = Client::with_role(workspace, ClientRole::Cli);
    match client.hello() {
        Ok(_) => return Ok(()),
        Err(error) if error.code == ErrorCode::HostNotRunning => {},
        Err(error) => return Err(error),
    }
    let mut child = spawn_managed_host(host_program, workspace)?;
    let deadline = Instant::now() + timeout;
    let mut owned_by_other = false;
    loop {
        match client.hello() {
            Ok(_) => return Ok(()),
            Err(error) if error.code == ErrorCode::HostNotRunning => {},
            Err(error) => return Err(error),
        }
        if let Ok(Some(status)) = child.try_wait() {
            if status.code() != Some(EXIT_WORKSPACE_LOCKED) {
                return Err(DltError::new(
                    ErrorCode::HostNotRunning,
                    format!("the host process ended with {status} before it accepted clients"),
                ));
            }
            // Another process owns the workspace. Its pipe may still be publishing, so keep trying until the deadline.
            owned_by_other = true;
        }
        if Instant::now() >= deadline {
            let code = if owned_by_other { ErrorCode::WorkspaceLocked } else { ErrorCode::HostNotRunning };
            return Err(DltError::new(code, "no host accepted the connection before the deadline"));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}
