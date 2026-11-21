//! Named pipe transport (Windows only).
//!
//! - Current-user DACL: owner, group, and the single ACE are the current user's SID; the DACL is
//!   protected (no inherited ACEs). No NULL security descriptor is used.
//! - `FILE_FLAG_FIRST_PIPE_INSTANCE` on the first instance only, so a second host cannot claim the name.
//! - `PIPE_REJECT_REMOTE_CLIENTS`, and every accepted client must run as the same user SID.
//! - Overlapped I/O with a deadline on every operation. A timed-out operation is cancelled and its
//!   completion is collected before its buffers are released. There is no FlushFileBuffers call.

use std::ffi::{c_void, OsString};
use std::io;
use std::os::windows::ffi::OsStringExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use designllm_contracts::{sha256, DltError, DltResult, ErrorCode};
use windows_sys::Win32::Foundation::{GetLastError, LocalFree, FILETIME, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::{
    EqualSid, GetTokenInformation, TokenUser, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, ReadFile, WriteFile, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, OPEN_EXISTING,
    PIPE_ACCESS_DUPLEX, SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT,
};
use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, GetNamedPipeClientProcessId, WaitNamedPipeW,
    PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
};
use windows_sys::Win32::System::Threading::{
    CreateEventW, GetCurrentProcess, GetProcessTimes, OpenProcess, OpenProcessToken, QueryFullProcessImageNameW, WaitForSingleObject,
    PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};

// Win32 constants with fixed values, declared locally where the feature set may not export them.
const GENERIC_READ: u32 = 0x8000_0000;
const GENERIC_WRITE: u32 = 0x4000_0000;
const ERROR_FILE_NOT_FOUND: u32 = 2;
const ERROR_ACCESS_DENIED: u32 = 5;
const ERROR_BROKEN_PIPE: u32 = 109;
const ERROR_NO_DATA: u32 = 232;
const ERROR_PIPE_BUSY: u32 = 231;
const ERROR_OPERATION_ABORTED: u32 = 995;
const ERROR_IO_PENDING: u32 = 997;
const ERROR_PIPE_CONNECTED: u32 = 535;
const WAIT_OBJECT_0: u32 = 0;
/// Pipe buffer size in each direction. Frames larger than this are transferred in chunks.
const PIPE_BUFFER_BYTES: u32 = 64 * 1024;
/// Largest single ReadFile/WriteFile call.
const MAX_IO_CHUNK: usize = 64 * 1024;

/// Pipe name prefix.
pub const PIPE_PREFIX: &str = r"\\.\pipe\";

/// Outcome of one bounded read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadOutcome {
    /// `n` bytes were copied into the buffer. `0` is possible and means nothing was available.
    Data(usize),
    /// The peer closed the pipe.
    Eof,
    /// Nothing arrived before the deadline. No bytes were consumed.
    TimedOut,
}

/// Reason a client could not connect.
#[derive(Debug)]
pub enum ConnectError {
    /// No pipe instance exists under this name: no host is serving this workspace.
    NotRunning,
    Other(io::Error),
}

fn last_code() -> u32 {
    unsafe { GetLastError() }
}

fn os_error(code: u32) -> io::Error {
    io::Error::from_raw_os_error(code as i32)
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Auto-reset-free manual event used for one overlapped operation.
struct Event(OwnedHandle);

impl Event {
    fn new() -> io::Result<Self> {
        let h = unsafe { CreateEventW(null(), 1, 0, null()) };
        if h.is_null() {
            return Err(io::Error::last_os_error());
        }
        Ok(Event(unsafe { OwnedHandle::from_raw_handle(h as RawHandle) }))
    }

    fn raw(&self) -> HANDLE {
        self.0.as_raw_handle() as HANDLE
    }
}

fn overlapped_for(event: &Event) -> OVERLAPPED {
    OVERLAPPED { hEvent: event.raw(), ..Default::default() }
}

/// Waits up to `timeout` for the event. Returns true when it was signalled.
fn wait_event(event: HANDLE, timeout: Duration) -> bool {
    let ms = timeout.as_millis().min(u32::MAX as u128 - 1) as u32;
    unsafe { WaitForSingleObject(event, ms) == WAIT_OBJECT_0 }
}

/// Result of an operation after it was cancelled or completed.
enum Settled {
    Bytes(usize),
    Cancelled,
    Eof,
}

/// Cancels a pending overlapped operation and waits for its completion, so the buffer can be released.
/// If the operation completed first, its transferred byte count is returned.
fn cancel_and_settle(handle: HANDLE, ov: &OVERLAPPED) -> io::Result<Settled> {
    unsafe { CancelIoEx(handle, ov) };
    let mut transferred = 0u32;
    if unsafe { GetOverlappedResult(handle, ov, &mut transferred, 1) } != 0 {
        return Ok(Settled::Bytes(transferred as usize));
    }
    match last_code() {
        ERROR_OPERATION_ABORTED => Ok(Settled::Cancelled),
        ERROR_BROKEN_PIPE => Ok(Settled::Eof),
        other => Err(os_error(other)),
    }
}

/// One end of a connected or listening named pipe. Safe to share between threads: each operation uses
/// its own event and OVERLAPPED structure.
pub struct Pipe {
    handle: OwnedHandle,
}

impl Pipe {
    fn from_raw(handle: HANDLE) -> Self {
        Pipe { handle: unsafe { OwnedHandle::from_raw_handle(handle as RawHandle) } }
    }

    pub fn raw(&self) -> HANDLE {
        self.handle.as_raw_handle() as HANDLE
    }

    /// Reads at most one chunk, waiting at most `timeout`.
    pub fn read_some(&self, buf: &mut [u8], timeout: Duration) -> io::Result<ReadOutcome> {
        if buf.is_empty() {
            return Ok(ReadOutcome::Data(0));
        }
        let event = Event::new()?;
        let mut ov = overlapped_for(&event);
        let want = buf.len().min(MAX_IO_CHUNK) as u32;
        let ok = unsafe { ReadFile(self.raw(), buf.as_mut_ptr(), want, null_mut(), &mut ov) };
        if ok == 0 {
            let code = last_code();
            if code == ERROR_BROKEN_PIPE {
                return Ok(ReadOutcome::Eof);
            }
            if code != ERROR_IO_PENDING {
                return Err(os_error(code));
            }
            if !wait_event(event.raw(), timeout) {
                return Ok(match cancel_and_settle(self.raw(), &ov)? {
                    Settled::Bytes(n) => ReadOutcome::Data(n),
                    Settled::Cancelled => ReadOutcome::TimedOut,
                    Settled::Eof => ReadOutcome::Eof,
                });
            }
        }
        let mut transferred = 0u32;
        if unsafe { GetOverlappedResult(self.raw(), &ov, &mut transferred, 0) } == 0 {
            return match last_code() {
                ERROR_BROKEN_PIPE => Ok(ReadOutcome::Eof),
                other => Err(os_error(other)),
            };
        }
        Ok(ReadOutcome::Data(transferred as usize))
    }

    /// Writes the whole buffer within `timeout`. A timeout can leave a partial frame in the pipe, so the
    /// caller must close the connection after an error.
    pub fn write_all(&self, mut buf: &[u8], timeout: Duration) -> io::Result<()> {
        let deadline = Instant::now() + timeout;
        while !buf.is_empty() {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "pipe write deadline exceeded"));
            }
            let chunk = &buf[..buf.len().min(MAX_IO_CHUNK)];
            let event = Event::new()?;
            let mut ov = overlapped_for(&event);
            let ok = unsafe { WriteFile(self.raw(), chunk.as_ptr(), chunk.len() as u32, null_mut(), &mut ov) };
            if ok == 0 {
                let code = last_code();
                if code != ERROR_IO_PENDING {
                    return Err(os_error(code));
                }
                if !wait_event(event.raw(), remaining) {
                    return match cancel_and_settle(self.raw(), &ov)? {
                        Settled::Bytes(n) => {
                            buf = &buf[n..];
                            continue;
                        }
                        Settled::Cancelled => {
                            Err(io::Error::new(io::ErrorKind::TimedOut, "pipe write deadline exceeded"))
                        }
                        Settled::Eof => Err(io::Error::new(io::ErrorKind::BrokenPipe, "pipe closed by peer")),
                    };
                }
            }
            let mut written = 0u32;
            if unsafe { GetOverlappedResult(self.raw(), &ov, &mut written, 0) } == 0 {
                return Err(os_error(last_code()));
            }
            buf = &buf[written as usize..];
        }
        Ok(())
    }

    /// Server side: waits up to `timeout` for a client. `Ok(false)` means the timeout passed without one.
    pub fn wait_for_client(&self, timeout: Duration) -> io::Result<bool> {
        let event = Event::new()?;
        let mut ov = overlapped_for(&event);
        if unsafe { ConnectNamedPipe(self.raw(), &mut ov) } != 0 {
            return Ok(true);
        }
        match last_code() {
            ERROR_PIPE_CONNECTED => return Ok(true),
            ERROR_IO_PENDING => {}
            other => return Err(os_error(other)),
        }
        if wait_event(event.raw(), timeout) {
            let mut transferred = 0u32;
            return match unsafe { GetOverlappedResult(self.raw(), &ov, &mut transferred, 0) } {
                0 => Err(os_error(last_code())),
                _ => Ok(true),
            };
        }
        match cancel_and_settle(self.raw(), &ov)? {
            Settled::Bytes(_) => Ok(true),
            Settled::Cancelled => Ok(false),
            Settled::Eof => Err(io::Error::new(io::ErrorKind::BrokenPipe, "client vanished during connect")),
        }
    }

    /// Server side: the process id of the connected client.
    pub fn client_process_id(&self) -> DltResult<u32> {
        let mut pid = 0u32;
        if unsafe { GetNamedPipeClientProcessId(self.raw(), &mut pid) } == 0 {
            return Err(DltError::new(ErrorCode::Io, format!("client pid unavailable: {}", os_error(last_code()))));
        }
        Ok(pid)
    }

    /// Server side: the full path of the connected client's executable. Callers compare it with the expected file.
    pub fn client_image_path(&self) -> DltResult<PathBuf> {
        let pid = self.client_process_id()?;
        let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if process.is_null() {
            return Err(DltError::new(ErrorCode::Io, format!("cannot open client process {pid}")));
        }
        let process = unsafe { OwnedHandle::from_raw_handle(process as RawHandle) };
        let mut buffer = vec![0u16; 32_768];
        let mut len = buffer.len() as u32;
        let ok = unsafe {
            QueryFullProcessImageNameW(process.as_raw_handle() as HANDLE, PROCESS_NAME_WIN32, buffer.as_mut_ptr(), &mut len)
        };
        if ok == 0 {
            return Err(DltError::new(ErrorCode::Io, format!("cannot read the image of client process {pid}: {}", os_error(last_code()))));
        }
        Ok(PathBuf::from(OsString::from_wide(&buffer[..len as usize])))
    }

    /// Server side: the connected client's process id and creation time (FILETIME ticks). Together they name one process even
    /// after its id is reused.
    pub fn client_process_start(&self) -> DltResult<(u32, u64)> {
        let pid = self.client_process_id()?;
        let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if process.is_null() {
            return Err(DltError::new(ErrorCode::Io, format!("cannot open client process {pid}")));
        }
        let process = unsafe { OwnedHandle::from_raw_handle(process as RawHandle) };
        let mut times = [FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 }; 4];
        let [created, exited, kernel, user] = &mut times;
        if unsafe { GetProcessTimes(process.as_raw_handle() as HANDLE, created, exited, kernel, user) } == 0 {
            return Err(DltError::new(ErrorCode::Io, format!("cannot read the times of client process {pid}: {}", os_error(last_code()))));
        }
        Ok((pid, (u64::from(times[0].dwHighDateTime) << 32) | u64::from(times[0].dwLowDateTime)))
    }

    /// Server side: true when the connected client runs as the same user SID as this process.
    /// Any failure to inspect the client is reported as an error, which callers treat as rejection.
    pub fn client_is_current_user(&self) -> DltResult<bool> {
        let pid = self.client_process_id()?;
        let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if process.is_null() {
            return Err(DltError::new(ErrorCode::Io, format!("cannot open client process {pid}")));
        }
        let process = unsafe { OwnedHandle::from_raw_handle(process as RawHandle) };
        let mut token: HANDLE = null_mut();
        if unsafe { OpenProcessToken(process.as_raw_handle() as HANDLE, TOKEN_QUERY, &mut token) } == 0 {
            return Err(DltError::new(ErrorCode::Io, "cannot open client token"));
        }
        let token = unsafe { OwnedHandle::from_raw_handle(token as RawHandle) };
        let theirs = token_user(token.as_raw_handle() as HANDLE)?;
        let mine = current_token_user()?;
        Ok(unsafe { EqualSid(theirs.sid(), mine.sid()) } != 0)
    }

    /// Server side: releases the client end. Unread bytes are discarded, so only call this after the
    /// last response has been read or abandoned.
    pub fn disconnect(&self) {
        unsafe { DisconnectNamedPipe(self.raw()) };
    }

    /// Cancels every pending overlapped operation on this handle (used during shutdown).
    pub fn cancel_pending(&self) {
        unsafe { CancelIoEx(self.raw(), null()) };
    }
}

/// A TOKEN_USER copied into an 8-byte-aligned buffer; SID pointers stay valid while this value lives.
struct TokenUserBuf {
    buf: Vec<u64>,
}

impl TokenUserBuf {
    fn sid(&self) -> windows_sys::Win32::Security::PSID {
        unsafe { (*(self.buf.as_ptr() as *const TOKEN_USER)).User.Sid }
    }
}

fn token_user(token: HANDLE) -> DltResult<TokenUserBuf> {
    let mut needed: u32 = 0;
    unsafe { GetTokenInformation(token, TokenUser, null_mut(), 0, &mut needed) };
    if needed == 0 {
        return Err(DltError::new(ErrorCode::Io, format!("token size query failed: {}", os_error(last_code()))));
    }
    let mut buf = vec![0u64; (needed as usize).div_ceil(8)];
    if unsafe { GetTokenInformation(token, TokenUser, buf.as_mut_ptr() as *mut c_void, needed, &mut needed) } == 0 {
        return Err(DltError::new(ErrorCode::Io, format!("token query failed: {}", os_error(last_code()))));
    }
    Ok(TokenUserBuf { buf })
}

fn current_token_user() -> DltResult<TokenUserBuf> {
    let mut token: HANDLE = null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(DltError::new(ErrorCode::Io, "cannot open current process token"));
    }
    let token = unsafe { OwnedHandle::from_raw_handle(token as RawHandle) };
    token_user(token.as_raw_handle() as HANDLE)
}

static USER_SID: OnceLock<Mutex<Option<String>>> = OnceLock::new();

/// SDDL string of the current user SID, e.g. `S-1-5-21-...`. Cached after the first success.
pub fn current_user_sid() -> DltResult<String> {
    let cache = USER_SID.get_or_init(|| Mutex::new(None));
    let mut slot = cache.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(sid) = slot.as_ref() {
        return Ok(sid.clone());
    }
    let user = current_token_user()?;
    let mut text: *mut u16 = null_mut();
    if unsafe { ConvertSidToStringSidW(user.sid(), &mut text) } == 0 {
        return Err(DltError::new(ErrorCode::Io, "cannot convert user SID"));
    }
    let sid = unsafe {
        let mut len = 0usize;
        while *text.add(len) != 0 {
            len += 1;
        }
        let s = String::from_utf16_lossy(std::slice::from_raw_parts(text, len));
        LocalFree(text as *mut c_void);
        s
    };
    *slot = Some(sid.clone());
    Ok(sid)
}

/// Absolute workspace root used for naming. Verbatim `\\?\` prefixes are removed, and the result is
/// lowercased because NTFS paths are case-insensitive.
pub fn normalize_root(root: &Path) -> PathBuf {
    let absolute = std::fs::canonicalize(root)
        .or_else(|_| std::path::absolute(root))
        .unwrap_or_else(|_| root.to_path_buf());
    let text = absolute.to_string_lossy().to_string();
    let stripped = if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = text.strip_prefix(r"\\?\") {
        rest.to_string()
    } else {
        text
    };
    PathBuf::from(stripped.trim_end_matches(['\\', '/']).to_lowercase())
}

/// Pipe name for a workspace: `\\.\pipe\designllm-v1-<24 hex>`, derived from the current user SID and
/// the normalized root. Two users, or two workspaces, never share a name.
pub fn pipe_name_for(root: &Path) -> DltResult<String> {
    let key = format!("{}\n{}", current_user_sid()?, normalize_root(root).to_string_lossy());
    let digest = sha256(key.as_bytes());
    Ok(format!("{PIPE_PREFIX}designllm-v1-{}", &digest[..24]))
}

/// Creates one server instance with the current-user DACL. `first` adds FILE_FLAG_FIRST_PIPE_INSTANCE.
pub fn create_server_instance(name: &str, first: bool) -> DltResult<Pipe> {
    let sid = current_user_sid()?;
    let sddl = wide(&format!("O:{sid}G:{sid}D:P(A;;GA;;;{sid})"));
    let mut sd: PSECURITY_DESCRIPTOR = null_mut();
    if unsafe { ConvertStringSecurityDescriptorToSecurityDescriptorW(sddl.as_ptr(), SDDL_REVISION_1, &mut sd, null_mut()) }
        == 0
    {
        return Err(DltError::new(ErrorCode::Io, format!("cannot build pipe DACL: {}", os_error(last_code()))));
    }
    let sd_guard = LocalGuard(sd);
    let sa = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: sd_guard.0,
        bInheritHandle: 0,
    };
    let mut open_mode = PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED;
    if first {
        open_mode |= FILE_FLAG_FIRST_PIPE_INSTANCE;
    }
    let pipe_mode = PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS;
    let wname = wide(name);
    let h = unsafe {
        CreateNamedPipeW(
            wname.as_ptr(),
            open_mode,
            pipe_mode,
            PIPE_UNLIMITED_INSTANCES,
            PIPE_BUFFER_BYTES,
            PIPE_BUFFER_BYTES,
            0,
            &sa,
        )
    };
    drop(sd_guard);
    if h == INVALID_HANDLE_VALUE {
        // FILE_FLAG_FIRST_PIPE_INSTANCE fails with ERROR_ACCESS_DENIED when another host already owns the name.
        let code = last_code();
        let kind = if first && code == ERROR_ACCESS_DENIED { ErrorCode::WorkspaceLocked } else { ErrorCode::Io };
        return Err(DltError::new(kind, format!("cannot create pipe instance: {}", os_error(code))));
    }
    Ok(Pipe::from_raw(h))
}

/// Frees a LocalAlloc'd security descriptor.
struct LocalGuard(PSECURITY_DESCRIPTOR);

impl Drop for LocalGuard {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { LocalFree(self.0 as *mut c_void) };
        }
    }
}

/// Connects to the server instance for `name`. Waits up to `wait` in total when every instance is busy: a client that
/// loses the race for a freed instance tries again until the deadline.
pub fn connect_client(name: &str, wait: Duration) -> Result<Pipe, ConnectError> {
    let wname = wide(name);
    let deadline = Instant::now() + wait;
    loop {
        let h = unsafe {
            CreateFileW(
                wname.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                0,
                null(),
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED | SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
                null_mut(),
            )
        };
        if h != INVALID_HANDLE_VALUE {
            return Ok(Pipe::from_raw(h));
        }
        let code = last_code();
        if code == ERROR_FILE_NOT_FOUND {
            return Err(ConnectError::NotRunning);
        }
        if code != ERROR_PIPE_BUSY {
            return Err(ConnectError::Other(os_error(code)));
        }
        // A zero timeout would make WaitNamedPipeW use the default wait, so the deadline is checked first. Sub-millisecond
        // time rounds up to 1 ms, and the value stays below WAIT_FOREVER (u32::MAX).
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(ConnectError::Other(io::Error::new(io::ErrorKind::TimedOut, "pipe stayed busy")));
        }
        let ms = remaining.as_millis().clamp(1, u32::MAX as u128 - 1) as u32;
        if unsafe { WaitNamedPipeW(wname.as_ptr(), ms) } == 0 {
            let w = last_code();
            return Err(if w == ERROR_FILE_NOT_FOUND {
                ConnectError::NotRunning
            } else {
                ConnectError::Other(os_error(w))
            });
        }
    }
}

/// ERROR_NO_DATA is reported when a previous client is still disconnecting. The host recreates the instance.
pub fn is_no_data(err: &io::Error) -> bool {
    err.raw_os_error() == Some(ERROR_NO_DATA as i32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pipe_name_is_stable_per_root_and_case_insensitive() {
        let dir = std::env::temp_dir();
        let a = pipe_name_for(&dir).unwrap();
        let b = pipe_name_for(&dir).unwrap();
        assert_eq!(a, b);
        assert!(a.starts_with(PIPE_PREFIX));
        assert_eq!(a.len(), PIPE_PREFIX.len() + "designllm-v1-".len() + 24);
        let upper = PathBuf::from(dir.to_string_lossy().to_uppercase());
        assert_eq!(pipe_name_for(&upper).unwrap(), a);
        assert_ne!(pipe_name_for(&dir.join("other-workspace")).unwrap(), a);
    }

    #[test]
    fn verbatim_prefix_is_removed_from_normalized_root() {
        let p = normalize_root(Path::new(&std::env::temp_dir()));
        assert!(!p.to_string_lossy().starts_with(r"\\?\"));
    }

    #[test]
    fn user_sid_is_a_well_formed_sid_string() {
        let sid = current_user_sid().unwrap();
        assert!(sid.starts_with("S-1-"), "{sid}");
    }
}
