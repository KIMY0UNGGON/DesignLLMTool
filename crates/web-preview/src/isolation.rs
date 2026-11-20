//! Execution limits of the preview (A19): the Content-Security-Policy every preview response carries, the file
//! lookup that serves the preview origin, the process memory cap, and the folder hold that marks a tree as in use.
//!
//! The host serves the preview origin itself (WebResourceRequested) instead of a WebView2 virtual host mapping:
//! WebView2 raises no WebResourceRequested for a mapped host, so a mapped document could not carry the policy header.
//!
//! Scripts run: running the project is what the preview is for. What the policy takes away is reach outside the
//! preview origin (connections, frames, form posts, `<base>`, plugins). Navigation away and new windows are refused
//! by the host's event handlers (`host.rs`), not by this policy.

use std::path::{Path, PathBuf};

/// Policy sent with every response of the preview origin. `worker-src blob:` admits only workers that inherit this
/// policy and refuses service workers: every preview shares one origin and one profile, so a registered service
/// worker would outlive its project and sit in front of the next one.
pub const PREVIEW_CONTENT_SECURITY_POLICY: &str = "default-src 'self' data: blob:; \
script-src 'self' 'unsafe-inline' 'unsafe-eval' blob:; style-src 'self' 'unsafe-inline' data: blob:; \
connect-src 'self' data: blob:; worker-src blob:; frame-src 'none'; child-src 'none'; object-src 'none'; \
base-uri 'none'; form-action 'none'; frame-ancestors 'none'";

/// Browser arguments of every preview WebView2 environment: the preview needs no network, because the host answers
/// the preview origin itself (`host.rs`). They close the paths outside the policy and the host's request handler:
/// - `--proxy-server` to a closed loopback port with `--proxy-bypass-list=<-loopback>` (no implicit bypass): every
///   HTTP(S) connection the network stack would make goes to a proxy that does not answer. WebView2 documents both
///   flags (learn.microsoft.com/microsoft-edge/webview2/concepts/webview-features-flags, "proxy-server",
///   "proxy-bypass-list"); `<-loopback>` is Chromium's proxy bypass rule syntax (net/proxy_resolution).
/// - `--host-resolver-rules="MAP * ~NOTFOUND"`: no host name resolves, so DNS prefetch, preconnect and any lookup send
///   no query (Chromium network switch `host-resolver-rules`, services/network/public/cpp/network_switches.cc).
/// - `--webrtc-ip-handling-policy=disable_non_proxied_udp` (the value of Chromium's / Edge's `WebRtcIPHandling` policy):
///   WebRTC may not send UDP and may use TCP only through the (dead) proxy, so STUN/TURN reach no address. Measured on
///   runtime 157.0.4341.60: without it a page sent 12 STUN/TURN UDP packets and opened 1 TURN TCP connection to local
///   listeners; with it, none. The `--force-webrtc-ip-handling-policy` spelling had no effect on that runtime.
/// WebView2 warns that browser flags can change between runtime versions; `session_tests` checks their effect with the
/// runtime the tests run against. Same-origin pages, inline scripts and the host's own responses do not use them.
pub(crate) const PREVIEW_BROWSER_ARGUMENTS: &str = "--proxy-server=http://127.0.0.1:9 --proxy-bypass-list=<-loopback> \
--host-resolver-rules=\"MAP * ~NOTFOUND\" --webrtc-ip-handling-policy=disable_non_proxied_udp";

/// Second layer behind `PREVIEW_BROWSER_ARGUMENTS`, never the only one: removes the WebRTC constructors from every
/// preview document before its scripts run. A page could still reach them in ways a script cannot close (for example
/// a context this script does not run in), which is why the browser arguments are the actual block. The `about:blank`
/// window of an empty frame is not such a context: on runtime 157.0.4341.60 it has no constructors either
/// (`session_tests::an_empty_frame_has_no_webrtc_constructors_and_reaches_no_address`).
pub(crate) const REMOVE_WEBRTC_SCRIPT: &str = "(function () {
  var names = ['RTCPeerConnection', 'webkitRTCPeerConnection', 'RTCDataChannel', 'RTCSessionDescription', 'RTCIceCandidate',
    'RTCRtpSender', 'RTCRtpReceiver', 'RTCRtpTransceiver', 'RTCDtlsTransport', 'RTCIceTransport', 'RTCSctpTransport',
    'RTCCertificate', 'RTCPeerConnectionIceEvent', 'RTCDataChannelEvent', 'RTCTrackEvent', 'RTCDTMFSender', 'RTCStatsReport',
    'RTCError', 'RTCErrorEvent', 'RTCEncodedAudioFrame', 'RTCEncodedVideoFrame'];
  for (var i = 0; i < names.length; i++) {
    try { Object.defineProperty(window, names[i], { value: undefined, writable: false, configurable: false }); } catch (e) {}
  }
})();";

/// Committed memory of the `--preview-web` process and every WebView2 process it starts, together.
pub const PREVIEW_MEMORY_LIMIT_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// Message prefixes of the errors that end a preview for a stated reason. The desktop maps them to its own text.
pub const PREVIEW_UNRESPONSIVE: &str = "preview renderer stopped responding";
pub const PREVIEW_MEMORY_LIMIT: &str = "preview exceeded its memory limit";
pub const PREVIEW_PROCESS_EXITED: &str = "preview process ended unexpectedly";

/// Largest file the host reads to serve it (the video poster source is at most `MAX_VIDEO_INPUT_BYTES`).
pub(crate) const MAX_PREVIEW_FILE_BYTES: u64 = 256 * 1024 * 1024;

fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn percent_decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let high = hex(*bytes.get(i + 1)?)?;
            let low = hex(*bytes.get(i + 2)?)?;
            out.push(high << 4 | low);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// The file under `folder` (already canonical) that a preview-origin URL names, or None when the URL leaves the
/// folder, names something other than a regular file, or carries a segment Windows would reinterpret.
/// A path ending in `/` names that folder's `index.html`.
pub(crate) fn preview_file(folder: &Path, origin: &str, uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix(origin)?;
    let rest = rest.split(['?', '#']).next().unwrap_or_default();
    let decoded = percent_decode(rest)?;
    let mut path = folder.to_path_buf();
    let segments: Vec<&str> = decoded.split('/').collect();
    for (index, segment) in segments.iter().enumerate() {
        if segment.is_empty() {
            if index + 1 == segments.len() {
                path.push("index.html");
                continue;
            }
            return None;
        }
        if *segment == "." || *segment == ".." || segment.chars().any(|c| c.is_control() || matches!(c, '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|')) {
            return None;
        }
        path.push(segment);
    }
    // `folder` is canonical, so a canonical path below it is inside it (symbolic links out of the tree are not).
    let resolved = std::fs::canonicalize(&path).ok()?;
    (resolved.starts_with(folder) && resolved.is_file()).then_some(resolved)
}

/// Content type of a served file. With `nosniff`, a type the page needs must be named here; anything else is bytes.
pub(crate) fn content_type(path: &Path) -> &'static str {
    let extension = path.extension().and_then(|e| e.to_str()).unwrap_or_default().to_ascii_lowercase();
    match extension.as_str() {
        "html" | "htm" => "text/html",
        "xhtml" | "xht" => "application/xhtml+xml",
        "svg" => "image/svg+xml",
        "xml" => "application/xml",
        "txt" | "md" | "csv" => "text/plain",
        "css" => "text/css",
        "js" | "mjs" => "text/javascript",
        "json" | "map" => "application/json",
        "wasm" => "application/wasm",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "bmp" => "image/bmp",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "ogg" => "audio/ogg",
        _ => "application/octet-stream",
    }
}

/// Response headers of every served file, in the `CreateWebResourceResponse` form.
pub(crate) fn response_headers(mime: &str) -> String {
    format!("Content-Type: {mime}\r\nContent-Security-Policy: {PREVIEW_CONTENT_SECURITY_POLICY}\r\nX-Content-Type-Options: nosniff\r\nCache-Control: no-store")
}

/// Holds `folder` open without delete sharing for the life of the returned handle. While it is held the folder
/// cannot be renamed or removed, which is how `web.materialize` tells a tree in use from an old one.
#[cfg(windows)]
pub(crate) fn hold_folder(folder: &Path) -> std::io::Result<std::fs::File> {
    use std::os::windows::fs::OpenOptionsExt;
    const FILE_SHARE_READ: u32 = 0x1;
    const FILE_SHARE_WRITE: u32 = 0x2;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    std::fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(folder)
}

#[cfg(windows)]
pub(crate) use job::Containment;

/// Job Object of the `--preview-web` process. The process joins it before WebView2 starts, so the browser and every
/// process it starts are in it from creation. The job is never left; closing it (process exit) ends them all.
#[cfg(windows)]
mod job {
    use super::PREVIEW_MEMORY_LIMIT_BYTES;
    use designllm_contracts::{DltError, DltResult, ErrorCode};
    use std::sync::OnceLock;
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
    use windows::Win32::System::IO::{CreateIoCompletionPort, GetQueuedCompletionStatus, OVERLAPPED};
    use windows::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectAssociateCompletionPortInformation, JobObjectBasicProcessIdList,
        JobObjectExtendedLimitInformation, QueryInformationJobObject, SetInformationJobObject, JOBOBJECT_ASSOCIATE_COMPLETION_PORT,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_JOB_MEMORY, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcess, TerminateProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE};

    const JOB_OBJECT_MSG_PROCESS_MEMORY_LIMIT: u32 = 9;
    const JOB_OBJECT_MSG_JOB_MEMORY_LIMIT: u32 = 10;
    const LISTED_PROCESSES: usize = 256;

    pub(crate) struct Containment {
        job: HANDLE,
        port: HANDLE,
    }

    // The handles are process-wide kernel handles, used only through thread-safe Win32 calls.
    unsafe impl Send for Containment {}
    unsafe impl Sync for Containment {}

    static CONTAINMENT: OnceLock<Result<Containment, String>> = OnceLock::new();

    fn create() -> Result<Containment, String> {
        unsafe {
            let job = CreateJobObjectW(None, PCWSTR::null()).map_err(|e| format!("CreateJobObject: {e}"))?;
            let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_JOB_MEMORY | JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            limits.JobMemoryLimit = PREVIEW_MEMORY_LIMIT_BYTES as usize;
            let port = CreateIoCompletionPort(INVALID_HANDLE_VALUE, None, 0, 1).map_err(|e| format!("CreateIoCompletionPort: {e}"))?;
            let associate = JOBOBJECT_ASSOCIATE_COMPLETION_PORT { CompletionKey: std::ptr::null_mut(), CompletionPort: port };
            let set = SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &limits as *const _ as *const core::ffi::c_void,
                std::mem::size_of_val(&limits) as u32,
            )
            .and_then(|_| {
                SetInformationJobObject(
                    job,
                    JobObjectAssociateCompletionPortInformation,
                    &associate as *const _ as *const core::ffi::c_void,
                    std::mem::size_of_val(&associate) as u32,
                )
            })
            .and_then(|_| AssignProcessToJobObject(job, GetCurrentProcess()));
            if let Err(e) = set {
                let _ = CloseHandle(port);
                let _ = CloseHandle(job);
                return Err(format!("{e}"));
            }
            Ok(Containment { job, port })
        }
    }

    impl Containment {
        /// Joins this process to the preview job (once per process). Refuses to run the preview unbounded.
        pub(crate) fn enter() -> DltResult<&'static Containment> {
            match CONTAINMENT.get_or_init(create) {
                Ok(containment) => Ok(containment),
                Err(message) => Err(DltError::new(
                    ErrorCode::Internal,
                    format!("the preview process could not be given its memory limit ({message}); the preview was not started"),
                )),
            }
        }

        /// True once any process of the job has tried to commit past the limit. Does not wait and does not allocate.
        pub(crate) fn memory_limit_reached(&self) -> bool {
            let mut reached = false;
            loop {
                let mut message = 0u32;
                let mut key = 0usize;
                let mut overlapped: *mut OVERLAPPED = std::ptr::null_mut();
                if unsafe { GetQueuedCompletionStatus(self.port, &mut message, &mut key, &mut overlapped, 0) }.is_err() {
                    return reached;
                }
                reached |= matches!(message, JOB_OBJECT_MSG_JOB_MEMORY_LIMIT | JOB_OBJECT_MSG_PROCESS_MEMORY_LIMIT);
            }
        }

        /// Ends every process of the job except this one and waits (bounded) for them to go. Uses no heap: the job is
        /// at its memory limit when this runs.
        pub(crate) fn end_other_processes(&self) {
            #[repr(C)]
            struct IdList {
                assigned: u32,
                listed: u32,
                ids: [usize; LISTED_PROCESSES],
            }
            let mut list = IdList { assigned: 0, listed: 0, ids: [0; LISTED_PROCESSES] };
            let queried = unsafe {
                QueryInformationJobObject(
                    Some(self.job),
                    JobObjectBasicProcessIdList,
                    &mut list as *mut _ as *mut core::ffi::c_void,
                    std::mem::size_of::<IdList>() as u32,
                    None,
                )
            };
            if queried.is_err() {
                return;
            }
            let own = std::process::id() as usize;
            for &pid in list.ids.iter().take((list.listed as usize).min(LISTED_PROCESSES)) {
                if pid == own {
                    continue;
                }
                unsafe {
                    if let Ok(process) = OpenProcess(PROCESS_TERMINATE | PROCESS_SYNCHRONIZE, false, pid as u32) {
                        let _ = TerminateProcess(process, 1);
                        let _ = WaitForSingleObject(process, 5_000);
                        let _ = CloseHandle(process);
                    }
                }
            }
        }
    }
}

/// Hidden runs of `crate::run`, the `--preview-web` entry point. Each runs in a child process (this test binary,
/// re-entered through the ignored `child_session` test) because `run` joins its process to the preview job for good.
#[cfg(all(test, windows))]
mod session_tests {
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    const PAGE_ENV: &str = "DLT_PREVIEW_CHILD_PAGE";
    const INPUT_ENV: &str = "DLT_PREVIEW_CHILD_INPUT";
    const KEEP_WEBRTC_ENV: &str = "DLT_PREVIEW_CHILD_KEEP_WEBRTC";
    const RESULT: &str = "DLT-CHILD-RESULT:";

    const READY_PAGE: &str = "<!doctype html><title>ok</title><script>window.chrome.webview.postMessage('designllm.preview.ready');</script>";
    const LOOP_PAGE: &str = "<!doctype html><title>loop</title><script>setTimeout(function () { for (;;) {} }, 300);</script>";
    const BOMB_PAGE: &str = "<!doctype html><title>bomb</title><script>
var keep = [];
setTimeout(function () {
  try { for (;;) { var block = new Uint8Array(64 * 1024 * 1024); block.fill(1); keep.push(block); } }
  catch (error) { window.chrome.webview.postMessage('bomb:' + error); }
}, 300);
</script>";

    /// Entered only as a child of `run_child`.
    #[test]
    #[ignore = "child process of the hidden run tests below"]
    fn child_session() {
        let page = PathBuf::from(std::env::var_os(PAGE_ENV).expect("page"));
        if std::env::var_os(INPUT_ENV).is_some() {
            crate::host::SIMULATE_INPUT.store(true, std::sync::atomic::Ordering::SeqCst);
        }
        if std::env::var_os(KEEP_WEBRTC_ENV).is_some() {
            crate::host::KEEP_WEBRTC.store(true, std::sync::atomic::Ordering::SeqCst);
        }
        let result = crate::run(&page, None, true);
        println!("{RESULT}{}", match result { Ok(()) => "ok".to_string(), Err(e) => format!("{:?}|{}", e.code, e.message) });
    }

    /// Entered only as a child of `run_smoke_child`: an in-process `smoke`, printing the page's messages.
    #[test]
    #[ignore = "child process of the WebRTC tests below"]
    fn child_smoke() {
        let page = PathBuf::from(std::env::var_os(PAGE_ENV).expect("page"));
        if std::env::var_os(KEEP_WEBRTC_ENV).is_some() {
            crate::host::KEEP_WEBRTC.store(true, std::sync::atomic::Ordering::SeqCst);
        }
        let result = crate::smoke(&page, Duration::from_secs(60));
        println!(
            "{RESULT}{}",
            match result { Ok(report) => format!("ok|{}|browser:{}", report.messages.join("|"), report.browser_version), Err(e) => format!("{:?}|{}", e.code, e.message) }
        );
    }

    fn run_child(name: &str, html: &str, input: bool) -> (String, Duration) {
        run_test_child("child_session", name, html, &[(INPUT_ENV, input)])
    }

    fn run_test_child(entry: &str, name: &str, html: &str, flags: &[(&str, bool)]) -> (String, Duration) {
        let root = std::env::temp_dir().join(format!("dlm-isolation-run-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let page_dir = root.join("page");
        std::fs::create_dir_all(&page_dir).unwrap();
        std::fs::write(page_dir.join("index.html"), html).unwrap();
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([format!("isolation::session_tests::{entry}").as_str(), "--exact", "--ignored", "--nocapture", "--test-threads=1"])
            .env(PAGE_ENV, page_dir.join("index.html"))
            .env("DESIGNLLM_WEBVIEW2_USER_DATA", root.join("user-data"))
            .env_remove(INPUT_ENV)
            .env_remove(KEEP_WEBRTC_ENV)
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        for (flag, on) in flags {
            if *on {
                command.env(flag, "1");
            }
        }
        let started = Instant::now();
        let mut child = command.spawn().unwrap();
        while child.try_wait().unwrap().is_none() {
            if started.elapsed() > Duration::from_secs(150) {
                let _ = child.kill();
                panic!("{name}: the hidden run did not end");
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        let elapsed = started.elapsed();
        let output = child.wait_with_output().unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        // The harness prints `test ... ` on the same line before the child's own output.
        let line = stdout.lines().find_map(|l| l.split_once(RESULT).map(|(_, r)| r)).unwrap_or_else(|| panic!("{name}: no result: {stdout}")).to_string();
        remove_with_retry(&root);
        (line, elapsed)
    }

    fn remove_with_retry(root: &Path) {
        for _ in 0..40 {
            if std::fs::remove_dir_all(root).is_ok() || !root.exists() {
                return;
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    }

    #[test]
    fn a_contained_preview_process_still_runs_a_normal_page() {
        let (result, _) = run_child("ready", READY_PAGE, false);
        assert_eq!(result, "ok");
    }

    #[test]
    fn a_page_that_stops_responding_closes_the_preview_with_the_reason() {
        let (result, elapsed) = run_child("loop", LOOP_PAGE, true);
        assert!(result.starts_with("Unavailable|") && result.contains(super::PREVIEW_UNRESPONSIVE), "{result}");
        println!("unresponsive detected after {elapsed:?}");
    }

    /// The WebRTC attempt below runs in the page's own window.
    const PAGE_WINDOW: &str = "window";
    /// The WebRTC attempt below runs in the `about:blank` window of an empty frame that the page adds. `frame-src 'none'`
    /// governs frame navigations, and an empty frame navigates nowhere, so the frame exists; whether the script layer
    /// reaches its window is what the frame test checks.
    const EMPTY_FRAME_WINDOW: &str =
        "(function () { var frame = document.createElement('iframe'); document.documentElement.appendChild(frame); return frame.contentWindow; })()";

    /// A page that read its project files and tries to send them out through WebRTC: STUN over UDP, TURN over UDP and
    /// TCP, all at local listeners of this test (loopback only, so no firewall prompt opens). Any byte that arrives is a
    /// leak. The listeners stay open for the whole child run. `scope` is the script expression of the window whose
    /// constructors the page uses ([`PAGE_WINDOW`] or [`EMPTY_FRAME_WINDOW`]).
    fn webrtc_attempt(name: &str, keep_webrtc: bool, scope: &str) -> (String, usize, usize) {
        use std::io::ErrorKind;
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        use std::sync::Arc;
        let udp = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let tcp = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        udp.set_nonblocking(true).unwrap();
        tcp.set_nonblocking(true).unwrap();
        let (udp_port, tcp_port) = (udp.local_addr().unwrap().port(), tcp.local_addr().unwrap().port());
        let (packets, connections, stop) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)), Arc::new(AtomicBool::new(false)));
        let watcher = {
            let (packets, connections, stop) = (Arc::clone(&packets), Arc::clone(&connections), Arc::clone(&stop));
            std::thread::spawn(move || {
                let mut buffer = [0u8; 2048];
                while !stop.load(Ordering::SeqCst) {
                    match udp.recv_from(&mut buffer) {
                        Ok(_) => { packets.fetch_add(1, Ordering::SeqCst); }
                        Err(e) if e.kind() == ErrorKind::WouldBlock => {}
                        Err(_) => {}
                    }
                    if tcp.accept().is_ok() {
                        connections.fetch_add(1, Ordering::SeqCst);
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
            })
        };
        let page = format!(
            "<!doctype html><title>rtc</title><script>
var post = function (m) {{ window.chrome.webview.postMessage(m); }};
setTimeout(async function () {{
  try {{
    var Peer = ({scope}).RTCPeerConnection;
    if (typeof Peer !== 'function') {{ post('rtc:unavailable'); }}
    else {{
      var servers = [{{ urls: 'stun:127.0.0.1:{udp_port}' }},
        {{ urls: 'turn:127.0.0.1:{udp_port}?transport=udp', username: 'u', credential: 'p' }},
        {{ urls: 'turn:127.0.0.1:{tcp_port}?transport=tcp', username: 'u', credential: 'p' }}];
      var pc = new Peer({{ iceServers: servers }});
      pc.createDataChannel('leak');
      var kinds = [];
      pc.onicecandidate = function (e) {{ if (e.candidate) kinds.push(e.candidate.type); }};
      await pc.setLocalDescription(await pc.createOffer());
      await new Promise(function (done) {{
        var timer = setTimeout(done, 8000);
        pc.onicegatheringstatechange = function () {{ if (pc.iceGatheringState === 'complete') {{ clearTimeout(timer); done(); }} }};
      }});
      post('rtc:created:' + kinds.join(','));
    }}
  }} catch (e) {{ post('rtc:error:' + e); }}
  post('designllm.preview.ready');
}}, 100);
</script>"
        );
        let (result, _) = run_test_child("child_smoke", name, &page, &[(KEEP_WEBRTC_ENV, keep_webrtc)]);
        std::thread::sleep(Duration::from_millis(500));
        stop.store(true, Ordering::SeqCst);
        watcher.join().unwrap();
        (result, packets.load(Ordering::SeqCst), connections.load(Ordering::SeqCst))
    }

    #[test]
    fn webrtc_reaches_no_address_even_with_its_constructors_in_place() {
        let (result, packets, connections) = webrtc_attempt("rtc-args", true, PAGE_WINDOW);
        assert!(result.starts_with("ok|") && result.contains("rtc:created:"), "the page really ran WebRTC gathering: {result}");
        assert_eq!((packets, connections), (0, 0), "no STUN/TURN packet or connection may leave the preview: {result}");
    }

    #[test]
    fn preview_pages_have_no_webrtc_constructors() {
        let (result, packets, connections) = webrtc_attempt("rtc-script", false, PAGE_WINDOW);
        assert!(result.contains("rtc:unavailable"), "{result}");
        assert_eq!((packets, connections), (0, 0), "{result}");
    }

    /// An empty frame's `about:blank` window has no WebRTC constructors either, and nothing leaves through it. The
    /// control run without the script layer shows that the frame attempt does reach the constructors when they are there.
    #[test]
    fn an_empty_frame_has_no_webrtc_constructors_and_reaches_no_address() {
        let (control, packets, connections) = webrtc_attempt("rtc-frame-args", true, EMPTY_FRAME_WINDOW);
        assert!(control.starts_with("ok|") && control.contains("rtc:created:"), "the frame attempt really ran WebRTC gathering: {control}");
        assert_eq!((packets, connections), (0, 0), "no STUN/TURN packet or connection may leave the preview: {control}");
        let (result, packets, connections) = webrtc_attempt("rtc-frame", false, EMPTY_FRAME_WINDOW);
        assert!(result.starts_with("ok|") && result.contains("rtc:unavailable"), "the script layer reaches the empty frame: {result}");
        assert_eq!((packets, connections), (0, 0), "{result}");
    }

    #[test]
    fn the_network_closing_browser_arguments_are_all_present() {
        for argument in [
            "--proxy-server=http://127.0.0.1:9",
            "--proxy-bypass-list=<-loopback>",
            "--host-resolver-rules=\"MAP * ~NOTFOUND\"",
            "--webrtc-ip-handling-policy=disable_non_proxied_udp",
        ] {
            assert!(super::PREVIEW_BROWSER_ARGUMENTS.contains(argument), "{argument}");
        }
    }

    #[test]
    fn a_page_past_the_memory_limit_is_ended_with_the_reason() {
        let (result, elapsed) = run_child("bomb", BOMB_PAGE, false);
        assert!(result.starts_with("Unavailable|") && result.contains(super::PREVIEW_MEMORY_LIMIT), "{result}");
        println!("memory limit enforced after {elapsed:?}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ORIGIN: &str = "https://designllm-preview.local/";

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("dlm-isolation-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("index.html"), "<p>i</p>").unwrap();
        std::fs::write(dir.join("sub").join("index.html"), "<p>s</p>").unwrap();
        std::fs::write(dir.join("sub").join("a b.html"), "<p>ab</p>").unwrap();
        std::fs::canonicalize(&dir).unwrap()
    }

    #[test]
    fn preview_urls_resolve_only_to_files_inside_the_folder() {
        let dir = scratch("paths");
        let found = |uri: &str| preview_file(&dir, ORIGIN, uri);
        assert_eq!(found("https://designllm-preview.local/index.html"), Some(dir.join("index.html")));
        assert_eq!(found("https://designllm-preview.local/"), Some(dir.join("index.html")));
        assert_eq!(found("https://designllm-preview.local/sub/"), Some(dir.join("sub").join("index.html")));
        assert_eq!(found("https://designllm-preview.local/sub/a%20b.html?x=1#y"), Some(dir.join("sub").join("a b.html")));
        for bad in [
            "https://designllm-preview.local/../index.html",
            "https://designllm-preview.local/%2e%2e/index.html",
            "https://designllm-preview.local/sub/..%2Findex.html",
            "https://designllm-preview.local/sub%5C..%5Cindex.html",
            "https://designllm-preview.local/index.html:stream",
            "https://designllm-preview.local/C:%2Fwindows%2Fwin.ini",
            "https://designllm-preview.local//index.html",
            "https://designllm-preview.local/index.html%00",
            "https://designllm-preview.local/%zz",
            "https://designllm-preview.local/missing.html",
            "https://designllm-preview.local/sub",
            "https://other.local/index.html",
        ] {
            assert_eq!(found(bad), None, "{bad}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn responses_carry_the_policy_and_a_fixed_type() {
        assert_eq!(content_type(Path::new("a.HTML")), "text/html");
        assert_eq!(content_type(Path::new("a.svg")), "image/svg+xml");
        assert_eq!(content_type(Path::new("a.bin")), "application/octet-stream");
        let headers = response_headers("text/html");
        assert!(headers.contains("Content-Security-Policy: default-src 'self'"), "{headers}");
        assert!(headers.contains("X-Content-Type-Options: nosniff"));
        for directive in ["connect-src 'self' data: blob:", "frame-src 'none'", "form-action 'none'", "base-uri 'none'", "object-src 'none'", "worker-src blob:"] {
            assert!(PREVIEW_CONTENT_SECURITY_POLICY.contains(directive), "{directive}");
        }
        assert!(!PREVIEW_CONTENT_SECURITY_POLICY.contains("https:") && !PREVIEW_CONTENT_SECURITY_POLICY.contains('*'), "no remote source is allowed");
    }

    #[cfg(windows)]
    #[test]
    fn a_held_folder_cannot_be_renamed_until_it_is_released() {
        let dir = scratch("hold");
        let moved = dir.with_extension("moved");
        let held = hold_folder(&dir).expect("hold the folder");
        assert!(std::fs::read(dir.join("index.html")).is_ok(), "files stay readable while held");
        assert!(std::fs::rename(&dir, &moved).is_err(), "a held folder cannot be renamed");
        drop(held);
        std::fs::rename(&dir, &moved).expect("a released folder can be renamed");
        let _ = std::fs::remove_dir_all(&moved);
    }
}
