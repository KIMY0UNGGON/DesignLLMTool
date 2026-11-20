//! Process ownership (Windows): a kill-on-close job object per attempt, suspended spawn, and assignment
//! before the first instruction runs. A grandchild created by the runtime is in the same job, so
//! terminating the job ends exactly the tree this attempt started. Nothing is found by name or by
//! scanning all processes.

#![cfg(windows)]

use std::ffi::c_void;
use std::io;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};
use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::ptr::null;

use crate::detect::{apply_env, EnvChange};

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectAssociateCompletionPortInformation, JobObjectBasicProcessIdList,
    JobObjectExtendedLimitInformation, QueryInformationJobObject, SetInformationJobObject, TerminateJobObject,
    JOBOBJECT_ASSOCIATE_COMPLETION_PORT, JOBOBJECT_BASIC_PROCESS_ID_LIST, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_ACTIVE_PROCESS, JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOB_OBJECT_LIMIT_PROCESS_MEMORY,
};
use windows_sys::Win32::System::Threading::{
    OpenProcess, OpenThread, ResumeThread, CREATE_NEW_PROCESS_GROUP, CREATE_SUSPENDED, PROCESS_QUERY_LIMITED_INFORMATION,
    PROCESS_SET_QUOTA, PROCESS_TERMINATE, THREAD_SUSPEND_RESUME,
};

/// `CREATE_NO_WINDOW`: console children get no visible console window.
pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;
/// `SYNCHRONIZE` access right for waiting on the process.
const SYNCHRONIZE: u32 = 0x0010_0000;
/// Largest number of processes one attempt may create (fork bomb guard).
pub const ACTIVE_PROCESS_LIMIT: u32 = 256;
/// Job message of a process that exceeded `ProcessMemoryLimit` (`JOB_OBJECT_MSG_PROCESS_MEMORY_LIMIT`). windows-sys names it under
/// `Win32::System::SystemServices`, which this crate does not enable, so the value is written here.
const JOB_OBJECT_MSG_PROCESS_MEMORY_LIMIT: u32 = 9;

// Completion port calls (kernel32). windows-sys names them under `Win32::System::IO`, which this crate does not enable.
extern "system" {
    fn CreateIoCompletionPort(
        filehandle: HANDLE,
        existingcompletionport: HANDLE,
        completionkey: usize,
        numberofconcurrentthreads: u32,
    ) -> HANDLE;
    fn GetQueuedCompletionStatus(
        completionport: HANDLE,
        lpnumberofbytestransferred: *mut u32,
        lpcompletionkey: *mut usize,
        lpoverlapped: *mut *mut c_void,
        dwmilliseconds: u32,
    ) -> i32;
}

fn last_error() -> io::Error {
    io::Error::last_os_error()
}

/// A new completion port that receives the notifications of `job`.
fn completion_port_of(job: &OwnedHandle) -> io::Result<OwnedHandle> {
    let port = unsafe { CreateIoCompletionPort(INVALID_HANDLE_VALUE, std::ptr::null_mut(), 0, 1) };
    if port.is_null() {
        return Err(last_error());
    }
    let port = unsafe { OwnedHandle::from_raw_handle(port as RawHandle) };
    let association = JOBOBJECT_ASSOCIATE_COMPLETION_PORT {
        CompletionKey: std::ptr::null_mut(),
        CompletionPort: port.as_raw_handle() as HANDLE,
    };
    let ok = unsafe {
        SetInformationJobObject(
            job.as_raw_handle() as HANDLE,
            JobObjectAssociateCompletionPortInformation,
            &association as *const JOBOBJECT_ASSOCIATE_COMPLETION_PORT as *const c_void,
            std::mem::size_of::<JOBOBJECT_ASSOCIATE_COMPLETION_PORT>() as u32,
        )
    };
    if ok == 0 {
        return Err(last_error());
    }
    Ok(port)
}

/// A job object with kill-on-close and an active-process limit.
pub struct JobObject {
    handle: OwnedHandle,
    /// Receives the job's notifications. Only a job with a memory limit has one.
    port: Option<OwnedHandle>,
}

impl JobObject {
    pub fn create() -> io::Result<Self> {
        Self::create_limited(None)
    }

    /// [`JobObject::create`], and each process of the job also gets at most `process_memory_limit` bytes of committed memory.
    /// A process that would commit past the cap gets a failed allocation. The job does not end it: the process decides what a
    /// failed allocation means. The caller learns of the cap from [`JobObject::memory_limit_reported`].
    pub fn create_limited(process_memory_limit: Option<usize>) -> io::Result<Self> {
        let h = unsafe { CreateJobObjectW(null(), null()) };
        if h.is_null() {
            return Err(last_error());
        }
        let handle = unsafe { OwnedHandle::from_raw_handle(h as RawHandle) };
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        info.BasicLimitInformation.LimitFlags =
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_ACTIVE_PROCESS | JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION;
        info.BasicLimitInformation.ActiveProcessLimit = ACTIVE_PROCESS_LIMIT;
        if let Some(limit) = process_memory_limit {
            info.BasicLimitInformation.LimitFlags |= JOB_OBJECT_LIMIT_PROCESS_MEMORY;
            info.ProcessMemoryLimit = limit;
        }
        let ok = unsafe {
            SetInformationJobObject(
                handle.as_raw_handle() as HANDLE,
                JobObjectExtendedLimitInformation,
                &info as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION as *const c_void,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if ok == 0 {
            return Err(last_error());
        }
        let port = match process_memory_limit {
            Some(_) => Some(completion_port_of(&handle)?),
            None => None,
        };
        Ok(JobObject { handle, port })
    }

    /// True when a process of this job exceeded the job's memory limit and the report is still queued. The report is the signal
    /// of a limit hit: a failed allocation fails before it commits, so the peak of the process stays below the cap and cannot
    /// tell the limit from a crash. A job without a memory limit reports false. Each report is taken once.
    pub fn memory_limit_reported(&self) -> bool {
        let Some(port) = self.port.as_ref() else {
            return false;
        };
        let mut reported = false;
        loop {
            let (mut message, mut key, mut overlapped) = (0u32, 0usize, std::ptr::null_mut::<c_void>());
            let queued = unsafe {
                GetQueuedCompletionStatus(port.as_raw_handle() as HANDLE, &mut message, &mut key, &mut overlapped, 0)
            };
            if queued == 0 {
                return reported;
            }
            reported |= message == JOB_OBJECT_MSG_PROCESS_MEMORY_LIMIT;
        }
    }

    fn raw(&self) -> HANDLE {
        self.handle.as_raw_handle() as HANDLE
    }

    /// Adds a process to the job. Its children inherit membership.
    pub fn assign(&self, process: HANDLE) -> io::Result<()> {
        if unsafe { AssignProcessToJobObject(self.raw(), process) } == 0 {
            return Err(last_error());
        }
        Ok(())
    }

    /// Ends every process in the job with `exit_code`.
    pub fn terminate(&self, exit_code: u32) -> io::Result<()> {
        if unsafe { TerminateJobObject(self.raw(), exit_code) } == 0 {
            return Err(last_error());
        }
        Ok(())
    }

    /// Largest committed memory, in bytes, that one process of the job has reached (`PeakProcessMemoryUsed`).
    pub fn peak_process_memory(&self) -> io::Result<usize> {
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        let mut returned = 0u32;
        let ok = unsafe {
            QueryInformationJobObject(
                self.raw(),
                JobObjectExtendedLimitInformation,
                &mut info as *mut JOBOBJECT_EXTENDED_LIMIT_INFORMATION as *mut c_void,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                &mut returned,
            )
        };
        if ok == 0 {
            return Err(last_error());
        }
        Ok(info.PeakProcessMemoryUsed)
    }

    /// Process ids currently in the job.
    pub fn process_ids(&self) -> io::Result<Vec<u32>> {
        const CAPACITY: usize = 256;
        let header = std::mem::size_of::<JOBOBJECT_BASIC_PROCESS_ID_LIST>();
        let words = (header + CAPACITY * std::mem::size_of::<usize>()).div_ceil(std::mem::size_of::<usize>());
        let mut buffer = vec![0usize; words];
        let mut returned = 0u32;
        let ok = unsafe {
            QueryInformationJobObject(
                self.raw(),
                JobObjectBasicProcessIdList,
                buffer.as_mut_ptr() as *mut c_void,
                (words * std::mem::size_of::<usize>()) as u32,
                &mut returned,
            )
        };
        if ok == 0 {
            return Err(last_error());
        }
        let list = buffer.as_ptr() as *const JOBOBJECT_BASIC_PROCESS_ID_LIST;
        let count = unsafe { (*list).NumberOfProcessIdsInList } as usize;
        let ids_ptr = unsafe { std::ptr::addr_of!((*list).ProcessIdList) } as *const usize;
        let ids = (0..count.min(CAPACITY)).map(|i| unsafe { *ids_ptr.add(i) } as u32).collect();
        Ok(ids)
    }
}

/// Resumes every thread owned by `pid`. Used once, right after a suspended spawn.
fn resume_threads_of(pid: u32) -> io::Result<usize> {
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Err(last_error());
    }
    let snapshot = unsafe { OwnedHandle::from_raw_handle(snapshot as RawHandle) };
    let mut entry: THREADENTRY32 = unsafe { std::mem::zeroed() };
    entry.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;
    let mut resumed = 0usize;
    let mut more = unsafe { Thread32First(snapshot.as_raw_handle() as HANDLE, &mut entry) } != 0;
    while more {
        if entry.th32OwnerProcessID == pid {
            let thread = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID) };
            if !thread.is_null() {
                let previous = unsafe { ResumeThread(thread) };
                unsafe { CloseHandle(thread) };
                if previous == u32::MAX {
                    return Err(last_error());
                }
                resumed += 1;
            }
        }
        more = unsafe { Thread32Next(snapshot.as_raw_handle() as HANDLE, &mut entry) } != 0;
    }
    if resumed == 0 {
        return Err(io::Error::other("no thread found for the suspended process"));
    }
    Ok(resumed)
}

/// Starts `program` suspended and assigns it to a new job. The child has not run yet: the caller resumes it with
/// [`resume_child`] once the job is known. On any failure the child is killed before the error is returned.
pub fn spawn_suspended_in_job(program: &Path, args: &[String], cwd: &Path) -> io::Result<(Child, JobObject)> {
    spawn_suspended(program, args, cwd, None, &[])
}

/// [`spawn_suspended_in_job`] with environment changes for the child (see [`crate::detect::RuntimeInfo::env`]).
pub fn spawn_suspended_in_job_with_env(
    program: &Path,
    args: &[String],
    cwd: &Path,
    env: &[EnvChange],
) -> io::Result<(Child, JobObject)> {
    spawn_suspended(program, args, cwd, None, env)
}

/// [`spawn_suspended_in_job`] whose job caps each process's committed memory (see [`JobObject::create_limited`]).
pub fn spawn_suspended_in_limited_job(
    program: &Path,
    args: &[String],
    cwd: &Path,
    process_memory_limit: Option<usize>,
) -> io::Result<(Child, JobObject)> {
    spawn_suspended(program, args, cwd, process_memory_limit, &[])
}

fn spawn_suspended(
    program: &Path,
    args: &[String],
    cwd: &Path,
    process_memory_limit: Option<usize>,
    env: &[EnvChange],
) -> io::Result<(Child, JobObject)> {
    let mut command = Command::new(program);
    command
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(CREATE_SUSPENDED | CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP);
    apply_env(&mut command, env);
    let mut child = command.spawn()?;
    let pid = child.id();
    let attached = (|| -> io::Result<JobObject> {
        let job = JobObject::create_limited(process_memory_limit)?;
        let access = PROCESS_SET_QUOTA | PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE;
        let process = unsafe { OpenProcess(access, 0, pid) };
        if process.is_null() {
            return Err(last_error());
        }
        let process = unsafe { OwnedHandle::from_raw_handle(process as RawHandle) };
        job.assign(process.as_raw_handle() as HANDLE)?;
        Ok(job)
    })();
    match attached {
        Ok(job) => Ok((child, job)),
        Err(e) => {
            let _ = child.kill();
            let _ = child.wait();
            Err(e)
        }
    }
}

/// Resumes the threads of a child that [`spawn_suspended_in_job`] started. From this call on, the child runs.
pub fn resume_child(child: &Child) -> io::Result<()> {
    resume_threads_of(child.id()).map(|_| ())
}

/// Starts `program` suspended, assigns it to a new job, then resumes it. The child never runs outside
/// the job. On any failure the child is killed before the error is returned.
pub fn spawn_in_job(program: &Path, args: &[String], cwd: &Path) -> io::Result<(Child, JobObject)> {
    let (mut child, job) = spawn_suspended_in_job(program, args, cwd)?;
    if let Err(e) = resume_child(&child) {
        let _ = job.terminate(1);
        let _ = child.kill();
        let _ = child.wait();
        return Err(e);
    }
    Ok((child, job))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_job_has_no_processes() {
        let job = JobObject::create().expect("job");
        assert!(job.process_ids().expect("query").is_empty());
    }

    #[test]
    fn a_job_without_a_memory_limit_reports_no_memory_limit() {
        let job = JobObject::create().expect("job");
        assert!(!job.memory_limit_reported());
    }

    #[test]
    fn a_limited_job_reports_no_memory_limit_for_a_child_that_stays_within_it() {
        let program = Path::new(r"C:\Windows\System32\cmd.exe");
        let args = ["/c".to_string(), "exit".to_string(), "0".to_string()];
        let (mut child, job) =
            spawn_suspended_in_limited_job(program, &args, Path::new(r"C:\Windows\System32"), Some(1 << 30)).expect("spawn");
        resume_child(&child).expect("resume");
        assert!(child.wait().expect("wait").success());
        assert!(!job.memory_limit_reported());
    }

    #[test]
    fn a_job_child_receives_the_environment_changes() {
        use std::io::Read;
        let program = Path::new(r"C:\Windows\System32\cmd.exe");
        let args = ["/c".to_string(), "set".to_string(), "CODEX_MANAGED".to_string()];
        let env: Vec<EnvChange> = vec![("CODEX_MANAGED_BY_NPM", Some("1".into())), ("CODEX_MANAGED_BY_PNPM", None)];
        let (mut child, _job) =
            spawn_suspended_in_job_with_env(program, &args, Path::new(r"C:\Windows\System32"), &env).expect("spawn");
        drop(child.stdin.take());
        resume_child(&child).expect("resume");
        let mut out = String::new();
        child.stdout.take().expect("stdout").read_to_string(&mut out).expect("read");
        assert!(child.wait().expect("wait").success(), "{out}");
        assert!(out.contains("CODEX_MANAGED_BY_NPM=1"), "{out}");
        assert!(!out.contains("CODEX_MANAGED_BY_PNPM="), "{out}");
    }

    #[test]
    fn a_limited_job_keeps_the_peak_of_its_child_after_the_child_exits() {
        let program = Path::new(r"C:\Windows\System32\cmd.exe");
        let args = ["/c".to_string(), "exit".to_string(), "0".to_string()];
        let (mut child, job) =
            spawn_suspended_in_limited_job(program, &args, Path::new(r"C:\Windows\System32"), Some(1 << 30)).expect("spawn");
        resume_child(&child).expect("resume");
        assert!(child.wait().expect("wait").success());
        let peak = job.peak_process_memory().expect("peak after the exit");
        assert!(peak > 0, "the peak of an exited child is {peak} bytes");
    }
}
