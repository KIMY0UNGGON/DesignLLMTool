//! Process ownership (macOS and Linux): each attempt runs in its own process group. The child becomes the leader of a
//! new group before it executes the program, so every process it starts is in that group unless it leaves on purpose
//! (`setsid`). Ending the job sends SIGKILL to the whole group, and dropping the job does the same (kill-on-close).
//! On Linux the child also gets SIGKILL when this process dies (`PR_SET_PDEATHSIG`).
//!
//! The API mirrors `win.rs`, so callers use [`crate::job`] on every platform. There is no suspended start: the group
//! exists before the program's first instruction, so [`resume_child`] has nothing to do.

#![cfg(unix)]

use std::io;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};

use crate::detect::{apply_env, EnvChange};

/// Windows creation flag kept for the shared API. Unix children have no console window, so it is unused.
pub const CREATE_NO_WINDOW: u32 = 0;
/// Largest number of processes one attempt may create on Windows. Unix has no per-group limit; kept for the shared API.
pub const ACTIVE_PROCESS_LIMIT: u32 = 256;

/// The process group of one attempt.
pub struct JobObject {
    /// The group id: the pid of the leader. `None` until a process is assigned.
    pgid: std::sync::Mutex<Option<libc::pid_t>>,
}

impl JobObject {
    pub fn create() -> io::Result<Self> {
        Ok(JobObject { pgid: std::sync::Mutex::new(None) })
    }

    /// [`JobObject::create`]. The memory cap is applied at spawn time (see [`spawn_suspended_in_limited_job`]).
    pub fn create_limited(_process_memory_limit: Option<usize>) -> io::Result<Self> {
        Self::create()
    }

    /// Unix cannot tell a failed allocation under `RLIMIT_AS` from other failures after the fact: always false.
    pub fn memory_limit_reported(&self) -> bool {
        false
    }

    fn group(&self) -> Option<libc::pid_t> {
        *self.pgid.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Records `pid` as the leader of this job's group. The process must have been started in its own group.
    pub fn assign(&self, pid: u32) -> io::Result<()> {
        let pid = pid as libc::pid_t;
        let pgid = unsafe { libc::getpgid(pid) };
        if pgid < 0 {
            return Err(io::Error::last_os_error());
        }
        if pgid != pid {
            return Err(io::Error::other(format!("process {pid} is not the leader of its own process group")));
        }
        *self.pgid.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(pgid);
        Ok(())
    }

    /// Ends every process in the group. The exit code is not used: the processes end by SIGKILL.
    pub fn terminate(&self, _exit_code: u32) -> io::Result<()> {
        let Some(pgid) = self.group() else {
            return Ok(());
        };
        if unsafe { libc::killpg(pgid, libc::SIGKILL) } != 0 {
            let err = io::Error::last_os_error();
            // ESRCH: no process is left in the group.
            if err.raw_os_error() != Some(libc::ESRCH) {
                return Err(err);
            }
        }
        Ok(())
    }

    /// Largest resident memory, in bytes, of any child this process has waited for. Unix keeps no per-group peak.
    pub fn peak_process_memory(&self) -> io::Result<usize> {
        let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
        if unsafe { libc::getrusage(libc::RUSAGE_CHILDREN, &mut usage) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // ru_maxrss is in kilobytes on Linux and in bytes on macOS.
        let raw = usage.ru_maxrss.max(0) as usize;
        Ok(if cfg!(target_os = "macos") { raw } else { raw * 1024 })
    }

    /// Process ids currently in the group.
    pub fn process_ids(&self) -> io::Result<Vec<u32>> {
        match self.group() {
            Some(pgid) => group_members(pgid),
            None => Ok(Vec::new()),
        }
    }
}

impl Drop for JobObject {
    fn drop(&mut self) {
        // Kill-on-close, as the Windows job object does.
        let _ = self.terminate(1);
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn group_members(pgid: libc::pid_t) -> io::Result<Vec<u32>> {
    let mut ids = Vec::new();
    for entry in std::fs::read_dir("/proc")? {
        let Ok(entry) = entry else { continue };
        let Some(pid) = entry.file_name().to_str().and_then(|name| name.parse::<u32>().ok()) else { continue };
        let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else { continue };
        // Fields after the command name: state (3), ppid (4), pgrp (5).
        let pgrp = stat
            .rsplit_once(')')
            .and_then(|(_, rest)| rest.split_whitespace().nth(2))
            .and_then(|field| field.parse::<libc::pid_t>().ok());
        if pgrp == Some(pgid) {
            ids.push(pid);
        }
    }
    Ok(ids)
}

#[cfg(target_os = "macos")]
fn group_members(pgid: libc::pid_t) -> io::Result<Vec<u32>> {
    // <libproc.h>: PROC_PGRP_ONLY lists the processes of one process group.
    const PROC_PGRP_ONLY: u32 = 2;
    let mut buffer = vec![0 as libc::pid_t; 1024];
    let bytes = unsafe {
        libc::proc_listpids(
            PROC_PGRP_ONLY,
            pgid as u32,
            buffer.as_mut_ptr() as *mut libc::c_void,
            (buffer.len() * std::mem::size_of::<libc::pid_t>()) as libc::c_int,
        )
    };
    if bytes < 0 {
        return Err(io::Error::last_os_error());
    }
    let count = bytes as usize / std::mem::size_of::<libc::pid_t>();
    Ok(buffer[..count.min(buffer.len())].iter().filter(|&&pid| pid > 0).map(|&pid| pid as u32).collect())
}

/// Starts `program` as the leader of a new process group and records the group in a new job. The child runs at once;
/// [`resume_child`] is kept for the shared API. On any failure the child is killed before the error is returned.
pub fn spawn_suspended_in_job(program: &Path, args: &[String], cwd: &Path) -> io::Result<(Child, JobObject)> {
    spawn_grouped(program, args, cwd, None, &[])
}

/// [`spawn_suspended_in_job`] with environment changes for the child (see [`crate::detect::RuntimeInfo::env`]).
pub fn spawn_suspended_in_job_with_env(
    program: &Path,
    args: &[String],
    cwd: &Path,
    env: &[EnvChange],
) -> io::Result<(Child, JobObject)> {
    spawn_grouped(program, args, cwd, None, env)
}

/// [`spawn_suspended_in_job`] whose child gets at most `process_memory_limit` bytes of address space (`RLIMIT_AS`).
pub fn spawn_suspended_in_limited_job(
    program: &Path,
    args: &[String],
    cwd: &Path,
    process_memory_limit: Option<usize>,
) -> io::Result<(Child, JobObject)> {
    spawn_grouped(program, args, cwd, process_memory_limit, &[])
}

/// Puts the processes `command` starts into a new process group led by the child, and, on Linux, ends the child when
/// this process dies.
pub fn own_process_group(command: &mut Command) {
    command.process_group(0);
    #[cfg(any(target_os = "linux", target_os = "android"))]
    unsafe {
        command.pre_exec(|| {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
            Ok(())
        });
    }
}

fn spawn_grouped(
    program: &Path,
    args: &[String],
    cwd: &Path,
    process_memory_limit: Option<usize>,
    env: &[EnvChange],
) -> io::Result<(Child, JobObject)> {
    let mut command = Command::new(program);
    command.args(args).current_dir(cwd).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    apply_env(&mut command, env);
    own_process_group(&mut command);
    if let Some(limit) = process_memory_limit {
        let limit = limit as libc::rlim_t;
        unsafe {
            command.pre_exec(move || {
                let cap = libc::rlimit { rlim_cur: limit, rlim_max: limit };
                // macOS does not enforce RLIMIT_AS; a refusal there is not an error.
                let _ = libc::setrlimit(libc::RLIMIT_AS, &cap);
                Ok(())
            });
        }
    }
    let mut child = command.spawn()?;
    let job = JobObject::create()?;
    if let Err(e) = job.assign(child.id()) {
        let _ = child.kill();
        let _ = child.wait();
        return Err(e);
    }
    Ok((child, job))
}

/// Kept for the shared API: a Unix child is never suspended.
pub fn resume_child(_child: &Child) -> io::Result<()> {
    Ok(())
}

/// Starts `program` in a new job (see [`spawn_suspended_in_job`]).
pub fn spawn_in_job(program: &Path, args: &[String], cwd: &Path) -> io::Result<(Child, JobObject)> {
    spawn_suspended_in_job(program, args, cwd)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn a_new_job_has_no_processes() {
        let job = JobObject::create().expect("job");
        assert!(job.process_ids().expect("query").is_empty());
    }

    #[test]
    fn a_job_child_receives_the_environment_changes() {
        use std::io::Read;
        let args = ["-c".to_string(), "env".to_string()];
        let env: Vec<EnvChange> = vec![("CODEX_MANAGED_BY_NPM", Some("1".into())), ("CODEX_MANAGED_BY_PNPM", None)];
        let (mut child, _job) =
            spawn_suspended_in_job_with_env(Path::new("/bin/sh"), &args, Path::new("/"), &env).expect("spawn");
        drop(child.stdin.take());
        resume_child(&child).expect("resume");
        let mut out = String::new();
        child.stdout.take().expect("stdout").read_to_string(&mut out).expect("read");
        assert!(child.wait().expect("wait").success(), "{out}");
        assert!(out.contains("CODEX_MANAGED_BY_NPM=1"), "{out}");
        assert!(!out.contains("CODEX_MANAGED_BY_PNPM="), "{out}");
    }

    #[test]
    fn terminating_the_job_ends_the_grandchildren_too() {
        let args = ["-c".to_string(), "sleep 30 & sleep 30; wait".to_string()];
        let (mut child, job) = spawn_in_job(Path::new("/bin/sh"), &args, Path::new("/")).expect("spawn");
        let deadline = Instant::now() + Duration::from_secs(5);
        while job.process_ids().unwrap().len() < 3 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(job.process_ids().unwrap().len() >= 2, "{:?}", job.process_ids());
        job.terminate(1).unwrap();
        child.wait().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !job.process_ids().unwrap().is_empty() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(job.process_ids().unwrap().is_empty(), "{:?}", job.process_ids());
    }
}
