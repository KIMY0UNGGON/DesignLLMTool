//! Unix domain socket transport (macOS and Linux), with the same API as the Windows named pipe transport.
//!
//! - The socket lives in a directory only the current user can open: `$XDG_RUNTIME_DIR` when it is set and private,
//!   otherwise `<temp>/dllm-<uid>` created with mode 0700. A directory owned by another user, or one that others can
//!   write, is refused.
//! - Ownership of the name is an exclusive `flock` on `<socket>.lock`, held for the listener's lifetime. A second host
//!   gets `WORKSPACE_LOCKED`. A stale socket file left by a crashed host is removed only while holding that lock.
//! - Every accepted client must run as the same user id (`SO_PEERCRED` on Linux, `getpeereid` on macOS).
//! - Non-blocking sockets with `poll` and a deadline on every operation.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, PoisonError, Weak};
use std::time::{Duration, Instant};

use designllm_contracts::{sha256, DltError, DltResult, ErrorCode};

/// Largest single read or write call.
const MAX_IO_CHUNK: usize = 64 * 1024;
/// Longest socket path the platform accepts (`sun_path` is 104 bytes on macOS, 108 on Linux, with the NUL).
const MAX_SOCKET_PATH: usize = 103;

/// Outcome of one bounded read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadOutcome {
    /// `n` bytes were copied into the buffer. `0` is possible and means nothing was available.
    Data(usize),
    /// The peer closed the connection.
    Eof,
    /// Nothing arrived before the deadline. No bytes were consumed.
    TimedOut,
}

/// Reason a client could not connect.
#[derive(Debug)]
pub enum ConnectError {
    /// No socket is listening under this name: no host is serving this workspace.
    NotRunning,
    Other(io::Error),
}

fn timeout_ms(timeout: Duration) -> libc::c_int {
    // Round sub-millisecond waits up, so a short deadline still polls once.
    let ms = timeout.as_micros().div_ceil(1000);
    ms.min(libc::c_int::MAX as u128) as libc::c_int
}

/// Waits up to `timeout` for `events` on `fd`. Returns the reported events, or 0 after the timeout.
fn poll_fd(fd: libc::c_int, events: libc::c_short, timeout: Duration) -> io::Result<libc::c_short> {
    let deadline = Instant::now() + timeout;
    loop {
        let mut entry = libc::pollfd { fd, events, revents: 0 };
        let remaining = deadline.saturating_duration_since(Instant::now());
        let n = unsafe { libc::poll(&mut entry, 1, timeout_ms(remaining)) };
        if n < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err);
        }
        return Ok(if n == 0 { 0 } else { entry.revents });
    }
}

/// The listening socket of one name, shared by every server instance of that name.
struct Listener {
    socket: UnixListener,
    path: PathBuf,
    /// Holds the exclusive lock that makes this process the owner of the name.
    _lock: File,
}

impl Drop for Listener {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Listeners by socket path. A weak entry lets later instances find the listener the first instance created.
fn listeners() -> &'static Mutex<HashMap<String, Weak<Listener>>> {
    static LISTENERS: OnceLock<Mutex<HashMap<String, Weak<Listener>>>> = OnceLock::new();
    LISTENERS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// One end of a connection, or a server instance waiting for one. Safe to share between threads.
pub struct Pipe {
    listener: Option<Arc<Listener>>,
    stream: OnceLock<UnixStream>,
}

impl Pipe {
    fn connected(stream: UnixStream) -> io::Result<Self> {
        stream.set_nonblocking(true)?;
        set_no_sigpipe(&stream);
        let cell = OnceLock::new();
        let _ = cell.set(stream);
        Ok(Pipe { listener: None, stream: cell })
    }

    fn stream(&self) -> io::Result<&UnixStream> {
        self.stream.get().ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "no client is connected"))
    }

    /// The socket descriptor of the connection.
    pub fn raw(&self) -> libc::c_int {
        self.stream.get().map(AsRawFd::as_raw_fd).unwrap_or(-1)
    }

    /// Reads at most one chunk, waiting at most `timeout`.
    pub fn read_some(&self, buf: &mut [u8], timeout: Duration) -> io::Result<ReadOutcome> {
        if buf.is_empty() {
            return Ok(ReadOutcome::Data(0));
        }
        let stream = self.stream()?;
        let events = poll_fd(stream.as_raw_fd(), libc::POLLIN, timeout)?;
        if events == 0 {
            return Ok(ReadOutcome::TimedOut);
        }
        let want = buf.len().min(MAX_IO_CHUNK);
        match (&*stream).read(&mut buf[..want]) {
            Ok(0) => Ok(ReadOutcome::Eof),
            Ok(n) => Ok(ReadOutcome::Data(n)),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::Interrupted => {
                Ok(ReadOutcome::Data(0))
            }
            Err(e) if matches!(e.kind(), io::ErrorKind::ConnectionReset | io::ErrorKind::BrokenPipe) => {
                Ok(ReadOutcome::Eof)
            }
            Err(e) => Err(e),
        }
    }

    /// Writes the whole buffer within `timeout`. A timeout can leave a partial frame in the socket, so the caller must
    /// close the connection after an error.
    pub fn write_all(&self, mut buf: &[u8], timeout: Duration) -> io::Result<()> {
        let stream = self.stream()?;
        let deadline = Instant::now() + timeout;
        while !buf.is_empty() {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "pipe write deadline exceeded"));
            }
            let events = poll_fd(stream.as_raw_fd(), libc::POLLOUT, remaining)?;
            if events == 0 {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "pipe write deadline exceeded"));
            }
            if events & (libc::POLLHUP | libc::POLLERR) != 0 && events & libc::POLLOUT == 0 {
                return Err(io::Error::new(io::ErrorKind::BrokenPipe, "pipe closed by peer"));
            }
            let chunk = &buf[..buf.len().min(MAX_IO_CHUNK)];
            match (&*stream).write(chunk) {
                Ok(0) => return Err(io::Error::new(io::ErrorKind::BrokenPipe, "pipe closed by peer")),
                Ok(n) => buf = &buf[n..],
                Err(e) if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// Server side: waits up to `timeout` for a client. `Ok(false)` means the timeout passed without one.
    pub fn wait_for_client(&self, timeout: Duration) -> io::Result<bool> {
        if self.stream.get().is_some() {
            return Ok(true);
        }
        let listener = self
            .listener
            .as_ref()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "not a server instance"))?;
        let events = poll_fd(listener.socket.as_raw_fd(), libc::POLLIN, timeout)?;
        if events == 0 {
            return Ok(false);
        }
        match listener.socket.accept() {
            Ok((stream, _)) => {
                stream.set_nonblocking(true)?;
                set_no_sigpipe(&stream);
                let _ = self.stream.set(stream);
                Ok(true)
            }
            Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted | io::ErrorKind::ConnectionAborted) => {
                Ok(false)
            }
            Err(e) => Err(e),
        }
    }

    /// The connected peer's process id and user id.
    fn peer(&self) -> DltResult<(u32, u32)> {
        let stream = self.stream().map_err(|e| DltError::new(ErrorCode::Io, format!("client pid unavailable: {e}")))?;
        peer_credentials(stream.as_raw_fd())
            .map_err(|e| DltError::new(ErrorCode::Io, format!("client pid unavailable: {e}")))
    }

    /// Server side: the process id of the connected client.
    pub fn client_process_id(&self) -> DltResult<u32> {
        self.peer().map(|(pid, _)| pid)
    }

    /// Server side: the full path of the connected client's executable. Callers compare it with the expected file.
    pub fn client_image_path(&self) -> DltResult<PathBuf> {
        let pid = self.client_process_id()?;
        process_image_path(pid).map_err(|e| {
            DltError::new(ErrorCode::Io, format!("cannot read the image of client process {pid}: {e}"))
        })
    }

    /// Server side: the connected client's process id and start time. Together they name one process even after its
    /// id is reused.
    pub fn client_process_start(&self) -> DltResult<(u32, u64)> {
        let pid = self.client_process_id()?;
        let started = process_start_time(pid).map_err(|e| {
            DltError::new(ErrorCode::Io, format!("cannot read the start time of client process {pid}: {e}"))
        })?;
        Ok((pid, started))
    }

    /// Server side: true when the connected client runs as the same user id as this process.
    /// Any failure to inspect the client is reported as an error, which callers treat as rejection.
    pub fn client_is_current_user(&self) -> DltResult<bool> {
        let (_, uid) = self.peer()?;
        Ok(uid == unsafe { libc::geteuid() })
    }

    /// Server side: releases the client end. Unread bytes are discarded.
    pub fn disconnect(&self) {
        if let Some(stream) = self.stream.get() {
            let _ = stream.shutdown(std::net::Shutdown::Both);
        }
    }

    /// Wakes every pending operation on this connection (used during shutdown). The connection cannot be used after.
    pub fn cancel_pending(&self) {
        self.disconnect();
    }
}

#[cfg(target_os = "macos")]
fn set_no_sigpipe(stream: &UnixStream) {
    let on: libc::c_int = 1;
    unsafe {
        libc::setsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_NOSIGPIPE,
            &on as *const libc::c_int as *const libc::c_void,
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        );
    }
}

#[cfg(not(target_os = "macos"))]
fn set_no_sigpipe(_stream: &UnixStream) {
    // Linux: Rust programs ignore SIGPIPE, so a write to a closed socket returns EPIPE.
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn peer_credentials(fd: libc::c_int) -> io::Result<(u32, u32)> {
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let rc = unsafe {
        libc::getsockopt(fd, libc::SOL_SOCKET, libc::SO_PEERCRED, &mut cred as *mut libc::ucred as *mut libc::c_void, &mut len)
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    if cred.pid <= 0 {
        return Err(io::Error::other("the peer has no process id"));
    }
    Ok((cred.pid as u32, cred.uid))
}

#[cfg(target_os = "macos")]
fn peer_credentials(fd: libc::c_int) -> io::Result<(u32, u32)> {
    let mut uid: libc::uid_t = 0;
    let mut gid: libc::gid_t = 0;
    if unsafe { libc::getpeereid(fd, &mut uid, &mut gid) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let mut pid: libc::pid_t = 0;
    let mut len = std::mem::size_of::<libc::pid_t>() as libc::socklen_t;
    let rc = unsafe {
        libc::getsockopt(fd, libc::SOL_LOCAL, libc::LOCAL_PEERPID, &mut pid as *mut libc::pid_t as *mut libc::c_void, &mut len)
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    if pid <= 0 {
        return Err(io::Error::other("the peer has no process id"));
    }
    Ok((pid as u32, uid))
}

/// The executable of process `pid`.
#[cfg(any(target_os = "linux", target_os = "android"))]
pub fn process_image_path(pid: u32) -> io::Result<PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/exe"))
}

/// The executable of process `pid`.
#[cfg(target_os = "macos")]
pub fn process_image_path(pid: u32) -> io::Result<PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    let mut buffer = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    let n = unsafe { libc::proc_pidpath(pid as libc::c_int, buffer.as_mut_ptr() as *mut libc::c_void, buffer.len() as u32) };
    if n <= 0 {
        return Err(io::Error::last_os_error());
    }
    buffer.truncate(n as usize);
    Ok(PathBuf::from(std::ffi::OsString::from_vec(buffer)))
}

/// The start time of process `pid`, in clock ticks since boot (Linux).
#[cfg(any(target_os = "linux", target_os = "android"))]
pub fn process_start_time(pid: u32) -> io::Result<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"))?;
    // The command name is in parentheses and may contain spaces: fields are counted after the last ')'.
    let rest = stat.rsplit_once(')').map(|(_, rest)| rest).ok_or_else(|| io::Error::other("malformed stat"))?;
    // After ')' the fields start at 3 (state); starttime is field 22.
    rest.split_whitespace()
        .nth(22 - 3)
        .and_then(|field| field.parse().ok())
        .ok_or_else(|| io::Error::other("malformed stat"))
}

/// The start time of process `pid`, in microseconds since the epoch (macOS).
#[cfg(target_os = "macos")]
pub fn process_start_time(pid: u32) -> io::Result<u64> {
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    let n = unsafe {
        libc::proc_pidinfo(pid as libc::c_int, libc::PROC_PIDTBSDINFO, 0, &mut info as *mut libc::proc_bsdinfo as *mut libc::c_void, size)
    };
    if n != size {
        return Err(io::Error::last_os_error());
    }
    Ok(info.pbi_start_tvsec * 1_000_000 + info.pbi_start_tvusec)
}

/// The current user's id as a stable text, `uid-<n>`. It plays the role of the Windows user SID in names.
pub fn current_user_sid() -> DltResult<String> {
    Ok(format!("uid-{}", unsafe { libc::geteuid() }))
}

/// Absolute workspace root used for naming. Symbolic links are resolved. macOS file systems are case-insensitive by
/// default, so the root is lowercased there; Linux keeps letter case.
pub fn normalize_root(root: &Path) -> PathBuf {
    let absolute = std::fs::canonicalize(root)
        .or_else(|_| std::path::absolute(root))
        .unwrap_or_else(|_| root.to_path_buf());
    let text = absolute.to_string_lossy().to_string();
    let trimmed = if text.len() > 1 { text.trim_end_matches('/').to_string() } else { text };
    if cfg!(target_os = "macos") {
        PathBuf::from(trimmed.to_lowercase())
    } else {
        PathBuf::from(trimmed)
    }
}

/// True when `dir` is a directory owned by the current user that no other user can write or enter.
fn is_private_dir(dir: &Path) -> bool {
    match std::fs::symlink_metadata(dir) {
        Ok(meta) => meta.is_dir() && meta.uid() == unsafe { libc::geteuid() } && meta.mode() & 0o077 == 0,
        Err(_) => false,
    }
}

/// The directory that holds this user's sockets.
fn socket_dir() -> DltResult<PathBuf> {
    if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from) {
        if runtime.is_absolute() && is_private_dir(&runtime) {
            return Ok(runtime);
        }
    }
    let dir = std::env::temp_dir().join(format!("dllm-{}", unsafe { libc::geteuid() }));
    match std::fs::DirBuilder::new().mode(0o700).create(&dir) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => {
            return Err(DltError::new(ErrorCode::Io, format!("cannot create the socket folder {}: {e}", dir.display())))
        }
    }
    if !is_private_dir(&dir) {
        return Err(DltError::new(
            ErrorCode::Io,
            format!("the socket folder {} is not a private folder of the current user", dir.display()),
        ));
    }
    Ok(dir)
}

/// Socket path for a workspace: `<socket dir>/designllm-v1-<24 hex>.sock`, derived from the user id and the normalized
/// root. Two users, or two workspaces, never share a name.
pub fn pipe_name_for(root: &Path) -> DltResult<String> {
    let key = format!("{}\n{}", current_user_sid()?, normalize_root(root).to_string_lossy());
    let digest = sha256(key.as_bytes());
    let dir = socket_dir()?;
    let mut path = dir.join(format!("designllm-v1-{}.sock", &digest[..24]));
    if path.as_os_str().len() > MAX_SOCKET_PATH {
        path = dir.join(format!("{}.sock", &digest[..24]));
    }
    if path.as_os_str().len() > MAX_SOCKET_PATH {
        return Err(DltError::new(
            ErrorCode::Io,
            format!("the socket path {} is too long; set XDG_RUNTIME_DIR or TMPDIR to a shorter folder", path.display()),
        ));
    }
    Ok(path.to_string_lossy().into_owned())
}

fn lock_listeners() -> std::sync::MutexGuard<'static, HashMap<String, Weak<Listener>>> {
    listeners().lock().unwrap_or_else(PoisonError::into_inner)
}

/// Binds the socket for `name` and takes ownership of the name.
fn bind_listener(name: &str) -> DltResult<Arc<Listener>> {
    let path = PathBuf::from(name);
    let lock_path = PathBuf::from(format!("{name}.lock"));
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_CLOEXEC)
        .open(&lock_path)
        .map_err(|e| DltError::new(ErrorCode::Io, format!("cannot open {}: {e}", lock_path.display())))?;
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let err = io::Error::last_os_error();
        let kind = if err.kind() == io::ErrorKind::WouldBlock { ErrorCode::WorkspaceLocked } else { ErrorCode::Io };
        return Err(DltError::new(kind, format!("cannot create pipe instance: {err}")));
    }
    // This process owns the name now: a socket file still there was left by a host that ended without cleanup.
    match std::fs::remove_file(&path) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(DltError::new(ErrorCode::Io, format!("cannot remove the stale socket {}: {e}", path.display()))),
    }
    let socket = UnixListener::bind(&path)
        .map_err(|e| DltError::new(ErrorCode::Io, format!("cannot create pipe instance: {e}")))?;
    socket.set_nonblocking(true).map_err(DltError::from)?;
    Ok(Arc::new(Listener { socket, path, _lock: lock }))
}

/// Creates one server instance. `first` takes ownership of the name (`WORKSPACE_LOCKED` when another host holds it);
/// later instances share the listener the first one created.
pub fn create_server_instance(name: &str, first: bool) -> DltResult<Pipe> {
    let mut registry = lock_listeners();
    registry.retain(|_, weak| weak.strong_count() > 0);
    let listener = match registry.get(name).and_then(Weak::upgrade) {
        Some(existing) if !first => existing,
        Some(_) => {
            return Err(DltError::new(ErrorCode::WorkspaceLocked, "cannot create pipe instance: this process already serves the name"))
        }
        None if first => {
            let listener = bind_listener(name)?;
            registry.insert(name.to_string(), Arc::downgrade(&listener));
            listener
        }
        None => return Err(DltError::new(ErrorCode::Io, "cannot create pipe instance: the listener is closed")),
    };
    Ok(Pipe { listener: Some(listener), stream: OnceLock::new() })
}

/// Connects to the host listening on `name`. Waits up to `wait` in total while the listen queue is full.
pub fn connect_client(name: &str, wait: Duration) -> Result<Pipe, ConnectError> {
    let deadline = Instant::now() + wait;
    loop {
        match UnixStream::connect(name) {
            Ok(stream) => return Pipe::connected(stream).map_err(ConnectError::Other),
            Err(e) if matches!(e.kind(), io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused) => {
                return Err(ConnectError::NotRunning)
            }
            Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted) => {
                if Instant::now() >= deadline {
                    return Err(ConnectError::Other(io::Error::new(io::ErrorKind::TimedOut, "pipe stayed busy")));
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(e) => return Err(ConnectError::Other(e)),
        }
    }
}

/// Windows reports ERROR_NO_DATA while a previous client is still disconnecting. Unix sockets have no such state.
pub fn is_no_data(_err: &io::Error) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pipe_name_is_stable_per_root() {
        let dir = std::env::temp_dir();
        let a = pipe_name_for(&dir).unwrap();
        assert_eq!(a, pipe_name_for(&dir).unwrap());
        assert!(a.ends_with(".sock"), "{a}");
        assert!(a.len() <= MAX_SOCKET_PATH, "{a}");
        assert_ne!(pipe_name_for(&dir.join("other-workspace")).unwrap(), a);
    }

    #[test]
    fn a_second_owner_is_refused_and_a_client_round_trips() {
        let root = std::env::temp_dir().join(format!("dllm-pipe-test-{}", std::process::id()));
        let name = pipe_name_for(&root).unwrap();
        let first = create_server_instance(&name, true).unwrap();
        let refused = create_server_instance(&name, true).err().expect("second owner");
        assert_eq!(refused.code, ErrorCode::WorkspaceLocked);
        let client = std::thread::spawn({
            let name = name.clone();
            move || {
                let pipe = connect_client(&name, Duration::from_secs(2)).ok().expect("connect");
                pipe.write_all(b"ping", Duration::from_secs(2)).unwrap();
                let mut buf = [0u8; 4];
                let mut got = 0;
                while got < 4 {
                    match pipe.read_some(&mut buf[got..], Duration::from_secs(2)).unwrap() {
                        ReadOutcome::Data(n) => got += n,
                        other => panic!("{other:?}"),
                    }
                }
                assert_eq!(&buf, b"pong");
            }
        });
        assert!(first.wait_for_client(Duration::from_secs(2)).unwrap());
        assert!(first.client_is_current_user().unwrap());
        assert_eq!(first.client_process_id().unwrap(), std::process::id());
        assert!(first.client_process_start().is_ok());
        assert!(first.client_image_path().is_ok());
        let mut buf = [0u8; 4];
        let mut got = 0;
        while got < 4 {
            match first.read_some(&mut buf[got..], Duration::from_secs(2)).unwrap() {
                ReadOutcome::Data(n) => got += n,
                other => panic!("{other:?}"),
            }
        }
        assert_eq!(&buf, b"ping");
        first.write_all(b"pong", Duration::from_secs(2)).unwrap();
        client.join().unwrap();
        assert_eq!(first.read_some(&mut buf, Duration::from_secs(2)).unwrap(), ReadOutcome::Eof);
        drop(first);
        assert!(matches!(connect_client(&name, Duration::from_millis(100)), Err(ConnectError::NotRunning)));
    }
}
