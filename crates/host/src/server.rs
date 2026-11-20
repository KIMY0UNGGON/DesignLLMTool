//! Host lifecycle: acceptance, the lifecycle thread (sweeps, event pump, idle exit), and shutdown.
//!
//! Idle exit (managed mode only): the host exits when no client is connected and the core lists no active
//! job for the startup or idle grace period. The final check and the closing flag are set under the same
//! lock that admits clients, so no client can attach between the decision and the exit.

use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use designllm_bridge::pipe::{self, Pipe};
use designllm_contracts::{DltError, DltResult, ErrorCode};
use designllm_core::Workspace;

use crate::conn;
use crate::jobs;
use crate::shared::{lock, Shared};
use crate::{HostMode, HostOptions, HostReady};

/// Accept wait slice. Shutdown is noticed at this period.
const ACCEPT_SLICE: Duration = Duration::from_millis(200);
/// How long shutdown waits for connection threads to finish their current frame.
const CONNECTION_DRAIN: Duration = Duration::from_secs(5);

/// Serves `workspace` until host.stop, an idle exit (managed), or a fatal accept error. Returns Ok after a
/// normal stop. Active jobs are canceled and their process trees ended before this returns.
pub(crate) fn serve(workspace: Workspace, mode: HostMode, options: HostOptions) -> DltResult<()> {
    let root = workspace.root().to_path_buf();
    let pipe_name = pipe::pipe_name_for(&root)?;
    // Creating the first instance is the ownership test: a second host for this name gets WORKSPACE_LOCKED.
    let first = pipe::create_server_instance(&pipe_name, true)?;
    let shared = Arc::new(Shared::new(workspace, pipe_name.clone(), mode, options));

    jobs::recover(&shared);
    if let Some(hook) = shared.options.on_ready.clone() {
        hook(&HostReady {
            pid: shared.pid,
            pipe_name: pipe_name.clone(),
            mode,
            workspace_root: root,
        });
    }

    let lifecycle = {
        let shared = Arc::clone(&shared);
        thread::Builder::new()
            .name("designllm-host-lifecycle".into())
            .spawn(move || lifecycle_loop(shared))
            .map_err(|e| DltError::new(ErrorCode::Io, format!("cannot start the lifecycle thread: {e}")))?
    };

    let result = accept_loop(&shared, first, &pipe_name);

    shared.begin_shutdown();
    jobs::stop_all(&shared);
    let _ = lifecycle.join();
    let drain_started = Instant::now();
    while shared.clients() > 0 && drain_started.elapsed() < CONNECTION_DRAIN {
        thread::sleep(Duration::from_millis(20));
    }
    result
}

fn accept_loop(shared: &Arc<Shared>, mut current: Pipe, name: &str) -> DltResult<()> {
    loop {
        if shared.is_stopping() {
            return Ok(());
        }
        match current.wait_for_client(ACCEPT_SLICE) {
            Ok(true) => {
                // Create the next instance before serving this client, so a new client never sees "not running".
                let next = pipe::create_server_instance(name, false)?;
                let client = std::mem::replace(&mut current, next);
                match client.client_is_current_user() {
                    Ok(true) => {}
                    // Another user, or an identity that cannot be read: refuse (fail closed) and drop the pipe.
                    Ok(false) | Err(_) => continue,
                }
                if !shared.admit() {
                    // Closing: the client sees the pipe end and reconnects, which starts a fresh host if needed.
                    continue;
                }
                let shared_for_conn = Arc::clone(shared);
                let spawned = thread::Builder::new()
                    .name("designllm-host-connection".into())
                    .spawn(move || conn::serve_connection(shared_for_conn, client));
                if spawned.is_err() {
                    shared.release();
                }
            }
            Ok(false) => {}
            Err(e) if pipe::is_no_data(&e) => {
                // A previous client is still disconnecting from this instance. Replace the instance.
                current = pipe::create_server_instance(name, false)?;
            }
            Err(e) => return Err(DltError::from(e)),
        }
    }
}

fn lifecycle_loop(shared: Arc<Shared>) {
    let mut idle_since: Option<Instant> = None;
    loop {
        if shared.is_stopping() {
            return;
        }
        thread::sleep(shared.options.tick);
        jobs::sweep(&shared);
        shared.pump_events();
        if shared.mode == HostMode::Managed && idle_exit_due(&shared, &mut idle_since) {
            return;
        }
    }
}

/// True when the managed host should exit now. The closing flag is set under the admission lock, so this
/// only returns true while no client is connected and none can connect afterwards.
fn idle_exit_due(shared: &Arc<Shared>, idle_since: &mut Option<Instant>) -> bool {
    let (clients, ever_connected) = {
        let life = lock(&shared.life);
        (life.clients, life.ever_connected)
    };
    if clients > 0 {
        *idle_since = None;
        return false;
    }
    if shared.jobs.running_count() > 0 || core_has_active_jobs(shared) {
        *idle_since = None;
        return false;
    }
    let since = *idle_since.get_or_insert_with(Instant::now);
    let grace = if ever_connected { shared.options.idle_grace } else { shared.options.startup_grace };
    if since.elapsed() < grace {
        return false;
    }
    let mut life = lock(&shared.life);
    if life.clients > 0 {
        *idle_since = None;
        return false;
    }
    life.closing = true;
    drop(life);
    shared.stopping.store(true, std::sync::atomic::Ordering::SeqCst);
    true
}

/// True when the core lists an active job, or when the active list cannot be read. An unreadable state is
/// treated as active, so the host does not exit while it may hold work.
fn core_has_active_jobs(shared: &Arc<Shared>) -> bool {
    match shared.core_call(|ws| ws.active_jobs()) {
        Ok(jobs) => !jobs.is_empty(),
        Err(e) if e.code == ErrorCode::UnsupportedOperation => false,
        Err(_) => true,
    }
}
