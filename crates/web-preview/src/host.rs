//! Windows WebView2 host (Win32 + webview2-com). Ported from the G0 hidden-host probe and extended with isolation.
//!
//! Window modes:
//! - `smoke` / invisible: a popup window kept off-screen with no WS_VISIBLE. It is never shown and never activated.
//! - anchored (`parent` given, visible): an independent top-level window whose lifetime follows the desktop window.
//! - standalone visible (no parent, not invisible): a normal window for an explicit user request.
//!
//! Isolation: navigation is limited to the preview origin, external URI schemes (mailto:, tel:, ...) never launch
//! an application, new windows are blocked, script dialogs are off, devtools and context menus are off, and every
//! resource request outside the preview origin gets HTTP 403. The host serves the preview origin from the folder
//! itself, every response with `isolation::PREVIEW_CONTENT_SECURITY_POLICY`, and the WebView2 environment has no
//! network (`isolation::PREVIEW_BROWSER_ARGUMENTS`: dead proxy, no name resolution, no non-proxied WebRTC UDP;
//! `isolation::REMOVE_WEBRTC_SCRIPT` is a second layer). A renderer that stops responding, or a WebView2 process that
//! ends, ends the session with the reason. `contain` sessions also run inside the preview Job Object (memory cap).
//! The served folder is held open for the whole session, so `web.materialize` never deletes a tree in use.
//! The page has no bridge object, so the only channel from the page is `postMessage`.
//!
//! All waits pump messages and have deadlines. Nothing waits forever.

use std::cell::Cell;
use std::os::windows::ffi::OsStrExt;
use std::rc::Rc;
use std::ops::Deref;
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use designllm_contracts::{DltError, DltResult, ErrorCode};
use webview2_com::Microsoft::Web::WebView2::Win32::{
    CreateCoreWebView2EnvironmentWithOptions, ICoreWebView2Controller, ICoreWebView2Environment, ICoreWebView2EnvironmentOptions,
    ICoreWebView2WebResourceResponse, ICoreWebView2_18, ICoreWebView2_2, ICoreWebView2_4, COREWEBVIEW2_PERMISSION_STATE_DENY,
    COREWEBVIEW2_PROCESS_FAILED_KIND, COREWEBVIEW2_PROCESS_FAILED_KIND_BROWSER_PROCESS_EXITED,
    COREWEBVIEW2_PROCESS_FAILED_KIND_RENDER_PROCESS_EXITED, COREWEBVIEW2_PROCESS_FAILED_KIND_RENDER_PROCESS_UNRESPONSIVE,
    COREWEBVIEW2_WEB_RESOURCE_CONTEXT_ALL, COREWEBVIEW2_WEB_RESOURCE_CONTEXT_DOCUMENT,
};
use webview2_com::{
    take_pwstr, AddScriptToExecuteOnDocumentCreatedCompletedHandler, CoreWebView2EnvironmentOptions,
    CreateCoreWebView2ControllerCompletedHandler, CreateCoreWebView2EnvironmentCompletedHandler,
    ContentLoadingEventHandler, DOMContentLoadedEventHandler, DownloadStartingEventHandler, LaunchingExternalUriSchemeEventHandler, NavigationCompletedEventHandler,
    NavigationStartingEventHandler, NewWindowRequestedEventHandler, PermissionRequestedEventHandler,
    ProcessFailedEventHandler, WebMessageReceivedEventHandler, WebResourceRequestedEventHandler,
};
use windows::core::{w, Interface, BOOL, PCWSTR, PWSTR};
use windows::Win32::Foundation::{E_FAIL, HGLOBAL, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MonitorFromRect, MonitorFromWindow, HMONITOR, MONITORINFO, MONITOR_DEFAULTTONEAREST};
use windows::Win32::System::Com::StructuredStorage::CreateStreamOnHGlobal;
use windows::Win32::System::Com::{CoInitializeEx, IStream, COINIT_APARTMENTTHREADED, STREAM_SEEK_SET};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, EnumChildWindows, EnumWindows,
    GetClientRect, GetWindowPlacement, GetWindowRect, GetWindowThreadProcessId, IsWindow, IsWindowVisible, PeekMessageW, RegisterClassW, SetWindowPlacement, SetWindowPos, ShowWindow,
    TranslateMessage, CW_USEDEFAULT, HWND_TOP, MSG, PM_REMOVE, SW_SHOWNOACTIVATE, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, WINDOWPLACEMENT, WM_CLOSE, WNDCLASSW,
    WS_CLIPCHILDREN, WS_EX_TOOLWINDOW, WS_MAXIMIZE, WS_OVERLAPPEDWINDOW, WS_POPUP, WS_VISIBLE,
};

use crate::isolation::{self, Containment};
use crate::runtime::RuntimeSelection;
use crate::{PREVIEW_ORIGIN, READY_MESSAGE};

#[path = "host_bounds.rs"]
mod bounds;

#[cfg(test)]
#[path = "host_tests.rs"]
mod tests;

const OFFSCREEN: i32 = -32_000;
const RPC_E_CHANGED_MODE: i32 = 0x8001_0106u32 as i32;
const ENVIRONMENT_TIMEOUT: Duration = Duration::from_secs(120);

/// Stop flag for anchored and visible hosts. Set by `request_stop`, cleared at the start of each session.
static STOP: AtomicBool = AtomicBool::new(false);

pub(crate) fn request_stop() {
    STOP.store(true, Ordering::SeqCst);
}

/// What the caller wants from one session.
pub(crate) struct SessionOptions<'a> {
    pub folder: &'a Path,
    pub entry_url: String,
    pub parent: Option<isize>,
    pub invisible: bool,
    /// Smoke sessions return when the page posts the ready message, or fail at the deadline.
    pub smoke: bool,
    pub timeout: Duration,
    /// Optional end-to-end deadline, including WebView2 environment/controller setup.
    pub total_timeout: Option<Duration>,
    pub runtime: &'a RuntimeSelection,
    pub user_data: &'a Path,
    /// Optional per-session cancellation. Unlike `request_stop`, it never affects another preview.
    pub cancellation: Option<Arc<AtomicBool>>,
    /// Joins this process to the preview Job Object first. Only the `--preview-web` process (`run`) does this: it
    /// binds the whole calling process, so in-process callers (smoke tests, the desktop's video poster) do not.
    pub contain: bool,
}

/// Facts observed during one session. Smoke tests assert on these.
#[derive(Debug, Clone, Default)]
pub(crate) struct SessionReport {
    pub browser_version: String,
    pub navigation_success: bool,
    pub messages: Vec<String>,
    pub blocked_requests: usize,
    pub blocked_navigations: usize,
    pub new_windows_blocked: usize,
    pub host_visible: bool,
    pub child_windows: usize,
    pub visible_child_windows: usize,
    pub process_top_level_windows: usize,
    pub process_visible_top_level_windows: usize,
}

#[derive(Debug)]
enum HostError {
    Win(String),
    Timeout(&'static str),
    Disconnected(&'static str),
    Cancelled(&'static str),
}

struct ControllerGuard(ICoreWebView2Controller);

impl Deref for ControllerGuard {
    type Target = ICoreWebView2Controller;

    fn deref(&self) -> &Self::Target { &self.0 }
}

impl Drop for ControllerGuard {
    fn drop(&mut self) {
        unsafe { let _ = self.0.Close(); }
    }
}

fn to_dlt(error: HostError, code: ErrorCode) -> DltError {
    let (code, message) = match error {
        HostError::Win(m) => (code, format!("WebView2 host error: {m}")),
        HostError::Timeout(what) => (code, format!("timed out waiting for {what}")),
        HostError::Disconnected(what) => (code, format!("WebView2 callback channel closed while waiting for {what}")),
        HostError::Cancelled(what) => (ErrorCode::Canceled, format!("WebView2 session canceled while waiting for {what}")),
    };
    DltError::new(code, message)
}

impl From<windows::core::Error> for HostError {
    fn from(e: windows::core::Error) -> Self {
        HostError::Win(format!("{e}"))
    }
}

fn wide(s: &str) -> Vec<u16> {
    std::ffi::OsStr::new(s).encode_wide().chain(std::iter::once(0)).collect()
}

fn wide_path(p: &Path) -> Vec<u16> {
    p.as_os_str().encode_wide().chain(std::iter::once(0)).collect()
}

fn init_com_sta() -> Result<(), HostError> {
    let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
    if hr.is_ok() || hr.0 == RPC_E_CHANGED_MODE {
        Ok(())
    } else {
        Err(HostError::Win(format!("CoInitializeEx returned {:#010x}", hr.0)))
    }
}

unsafe extern "system" fn host_wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if msg == WM_CLOSE {
        // Closing the host window closes only this host. The parent and other windows are not touched.
        unsafe {
            let _ = DestroyWindow(hwnd);
        }
        return LRESULT(0);
    }
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

fn register_class() -> Result<(), HostError> {
    unsafe {
        let module = GetModuleHandleW(None)?;
        let wc = WNDCLASSW {
            lpfnWndProc: Some(host_wndproc),
            hInstance: windows::Win32::Foundation::HINSTANCE(module.0),
            lpszClassName: w!("DesignLLMWebPreviewHost"),
            ..Default::default()
        };
        // A second registration returns 0. That is not an error.
        let _ = RegisterClassW(&wc);
    }
    Ok(())
}

fn client_size(hwnd: HWND) -> (i32, i32) {
    let mut rc = RECT::default();
    unsafe {
        let _ = GetClientRect(hwnd, &mut rc);
    }
    ((rc.right - rc.left).max(1), (rc.bottom - rc.top).max(1))
}

fn monitor_info(monitor: HMONITOR) -> Option<MONITORINFO> {
    let mut info = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
    unsafe { GetMonitorInfoW(monitor, &mut info).as_bool() }.then_some(info)
}

fn restored_window_placement() -> Option<(RECT, RECT, u32, u32)> {
    let (rect, show, flags) = bounds::parse_placement(&std::env::var("DESIGNLLM_PREVIEW_RECT").ok()?)?;
    let info = monitor_info(unsafe { MonitorFromRect(&rect, MONITOR_DEFAULTTONEAREST) })?;
    let screen = bounds::clamp_rect(rect, info.rcWork)?;
    Some((screen, bounds::workspace_rect(screen, info.rcMonitor, info.rcWork)?, show, flags))
}

fn initial_window_rect(owner: HWND) -> Result<RECT, HostError> {
    let mut rect = RECT::default();
    unsafe { GetWindowRect(owner, &mut rect)?; }
    let info = monitor_info(unsafe { MonitorFromWindow(owner, MONITOR_DEFAULTTONEAREST) })
        .ok_or_else(|| HostError::Win("could not get the owner's monitor work area".into()))?;
    bounds::initial_rect(rect, info.rcWork).ok_or_else(|| HostError::Win("invalid monitor work area".into()))
}

fn parse_z_order(value: &str) -> Option<((isize, u32), (isize, u32))> {
    let mut parts = value.split(',');
    let result = ((parts.next()?.parse().ok()?, parts.next()?.parse().ok()?), (parts.next()?.parse().ok()?, parts.next()?.parse().ok()?));
    parts.next().is_none().then_some(result)
}

fn restore_z_order(hwnd: HWND) {
    let Some((previous, editor)) = std::env::var("DESIGNLLM_PREVIEW_Z_AFTER").ok().and_then(|value| parse_z_order(&value)) else { return; };
    for (after, pid) in [previous, editor] {
        let target = if after == 0 && pid == 0 && previous == (0, 0) { HWND_TOP } else {
            let target = HWND(after as *mut _);
            let mut actual_pid = 0;
            if pid == 0 || target == hwnd || !unsafe { IsWindow(Some(target)).as_bool() } { continue; }
            unsafe { GetWindowThreadProcessId(target, Some(&mut actual_pid)); }
            if actual_pid != pid { continue; }
            target
        };
        if unsafe { SetWindowPos(hwnd, Some(target), 0, 0, 0, 0, SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE) }.is_ok() { break; }
    }
}

fn create_host_window(parent: Option<HWND>, invisible: bool) -> Result<HWND, HostError> {
    register_class()?;
    let module = unsafe { GetModuleHandleW(None)? };
    let instance = Some(windows::Win32::Foundation::HINSTANCE(module.0));
    let hwnd = match parent {
        Some(p) => {
            let title = wide(&std::env::var("DESIGNLLM_PREVIEW_TITLE").unwrap_or_else(|_| "DesignLLM Web Preview".into()));
            let restored = if invisible { None } else { restored_window_placement() };
            let style = if invisible { WS_POPUP } else if restored.is_some_and(|(_, _, show, _)| show == 3) {
                WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN | WS_MAXIMIZE
            } else { WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN };
            let rect = if invisible { None } else { Some(match restored { Some((screen, _, _, _)) => screen, None => initial_window_rect(p)? }) };
            let (x, y, width, height) = rect.map_or((CW_USEDEFAULT, CW_USEDEFAULT, 1024, 768), |r| (r.left, r.top, r.right - r.left, r.bottom - r.top));
            let hwnd = unsafe { CreateWindowExW(Default::default(), w!("DesignLLMWebPreviewHost"), PCWSTR(title.as_ptr()), style, x, y, width, height, None, None, instance, None)? };
            if !invisible { restore_z_order(hwnd); }
            if let Some((_, normal, show, flags)) = restored {
                let mut placement = WINDOWPLACEMENT { length: std::mem::size_of::<WINDOWPLACEMENT>() as u32, ..Default::default() };
                let result = unsafe { GetWindowPlacement(hwnd, &mut placement) }.and_then(|()| {
                    bounds::apply_placement(&mut placement, normal, show, flags);
                    unsafe { SetWindowPlacement(hwnd, &placement) }
                });
                if let Err(error) = result {
                    unsafe { let _ = DestroyWindow(hwnd); }
                    return Err(error.into());
                }
                unsafe { let _ = ShowWindow(hwnd, bounds::show_without_activation(show)); }
            } else if !invisible { unsafe { let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE); } }
            if !invisible { restore_z_order(hwnd); }
            hwnd
        }
        None if invisible => unsafe {
            CreateWindowExW(
                WS_EX_TOOLWINDOW,
                w!("DesignLLMWebPreviewHost"),
                w!("DesignLLM Web Preview (smoke)"),
                WS_POPUP,
                OFFSCREEN,
                OFFSCREEN,
                800,
                600,
                None,
                None,
                instance,
                None,
            )?
        },
        None => unsafe {
            CreateWindowExW(
                Default::default(),
                w!("DesignLLMWebPreviewHost"),
                w!("DesignLLM Web Preview"),
                WS_OVERLAPPEDWINDOW | WS_VISIBLE,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                1024,
                768,
                None,
                None,
                instance,
                None,
            )?
        },
    };
    Ok(hwnd)
}

fn pump_messages() {
    unsafe {
        let mut msg = MSG::default();
        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

fn remaining_timeout(deadline: Option<Instant>, fallback: Duration) -> Duration {
    deadline.map_or(fallback, |deadline| fallback.min(deadline.saturating_duration_since(Instant::now())))
}

/// Pumps messages until a value arrives, the deadline passes, or the sender is gone.
fn pump_until<T>(rx: &Receiver<T>, timeout: Duration, what: &'static str, cancellation: Option<&AtomicBool>, hwnd: HWND, owner: Option<HWND>) -> Result<T, HostError> {
    let deadline = Instant::now() + timeout;
    loop {
        if cancellation.is_some_and(|cancel| cancel.load(Ordering::SeqCst)) {
            return Err(HostError::Cancelled(what));
        }
        match rx.try_recv() {
            Ok(v) => return Ok(v),
            Err(TryRecvError::Disconnected) => return Err(HostError::Disconnected(what)),
            Err(TryRecvError::Empty) => {}
        }
        if Instant::now() >= deadline {
            return Err(HostError::Timeout(what));
        }
        pump_messages();
        if window_gone(hwnd, owner) {
            return Err(HostError::Cancelled(what));
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn window_gone(hwnd: HWND, owner: Option<HWND>) -> bool {
    unsafe { !IsWindow(Some(hwnd)).as_bool() || owner.is_some_and(|owner| !IsWindow(Some(owner)).as_bool()) }
}

/// Visibility of this process's windows (host, children, and all top-level windows of this process).
fn visibility(hwnd: HWND) -> (bool, usize, usize) {
    struct Acc {
        children: Vec<HWND>,
    }
    unsafe extern "system" fn collect(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let acc = unsafe { &mut *(lparam.0 as *mut Acc) };
        acc.children.push(hwnd);
        BOOL(1)
    }
    let mut acc = Acc { children: Vec::new() };
    unsafe {
        let _ = EnumChildWindows(Some(hwnd), Some(collect), LPARAM(&mut acc as *mut Acc as isize));
    }
    let host_visible = unsafe { IsWindowVisible(hwnd).as_bool() };
    let visible_children = acc.children.iter().filter(|h| unsafe { IsWindowVisible(**h).as_bool() }).count();
    (host_visible, acc.children.len(), visible_children)
}

/// Top-level windows owned by this process: total and visible.
pub(crate) fn process_top_level_windows() -> (usize, usize) {
    struct Acc {
        pid: u32,
        total: usize,
        visible: usize,
    }
    unsafe extern "system" fn visit(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let acc = unsafe { &mut *(lparam.0 as *mut Acc) };
        let mut pid = 0u32;
        unsafe {
            GetWindowThreadProcessId(hwnd, Some(&mut pid));
        }
        if pid == acc.pid {
            acc.total += 1;
            if unsafe { IsWindowVisible(hwnd) }.as_bool() {
                acc.visible += 1;
            }
        }
        BOOL(1)
    }
    let mut acc = Acc { pid: std::process::id(), total: 0, visible: 0 };
    unsafe {
        let _ = EnumWindows(Some(visit), LPARAM(&mut acc as *mut Acc as isize));
    }
    (acc.total, acc.visible)
}

fn is_preview_uri(uri: &str) -> bool {
    uri.starts_with(PREVIEW_ORIGIN) || uri == "about:blank"
}

/// Runs one session. Smoke sessions return on the ready message. Other sessions run until stop, parent close,
/// or window close.
pub(crate) fn run_session(options: SessionOptions<'_>) -> DltResult<SessionReport> {
    if !options.smoke {
        STOP.store(false, Ordering::SeqCst);
    }
    let overall_deadline = options.total_timeout.map(|timeout| Instant::now() + timeout);
    // Before WebView2 starts: its processes inherit the job when they are created.
    let containment = if options.contain { Some(Containment::enter()?) } else { None };
    let _in_use = isolation::hold_folder(options.folder)
        .map_err(|e| DltError::new(ErrorCode::Io, format!("preview folder {} cannot be opened: {e}", options.folder.display())))?;
    init_com_sta().map_err(|e| to_dlt(e, ErrorCode::Internal))?;
    std::fs::create_dir_all(options.user_data)?;
    let parent_hwnd = options.parent.map(|v| HWND(v as *mut core::ffi::c_void));
    let hwnd = create_host_window(parent_hwnd, options.invisible).map_err(|e| to_dlt(e, ErrorCode::Internal))?;

    if std::env::var_os("DESIGNLLM_PREVIEW_REPORT_WINDOW").is_some() {
        let mut out = std::io::stdout().lock();
        let _ = out.write_all(b"\ndesignllm.preview.window.ready\n").and_then(|()| out.flush());
    }

    // Everything after window creation runs inside this guard, so the window is destroyed on every error path.
    struct WindowGuard(HWND);
    impl Drop for WindowGuard {
        fn drop(&mut self) {
            unsafe {
                let _ = DestroyWindow(self.0);
            }
        }
    }
    let guard = WindowGuard(hwnd);
    let mut report = SessionReport::default();
    let result = run_inside(&options, hwnd, &mut report, overall_deadline, containment);
    let closed = !options.smoke && window_gone(hwnd, parent_hwnd);
    drop(guard);
    finish_session(result, closed, options.cancellation.as_ref().is_some_and(|cancel| cancel.load(Ordering::SeqCst)), report)
}

fn finish_session(result: DltResult<()>, closed: bool, cancelled: bool, report: SessionReport) -> DltResult<SessionReport> {
    match result {
        Err(error) if closed && !cancelled && error.code == ErrorCode::Canceled => Ok(report),
        other => other.map(|()| report),
    }
}

/// In-memory stream holding `bytes`, positioned at its start.
fn memory_stream(bytes: &[u8]) -> windows::core::Result<IStream> {
    let stream = unsafe { CreateStreamOnHGlobal(HGLOBAL(std::ptr::null_mut()), true)? };
    let mut written = 0u32;
    let length = u32::try_from(bytes.len()).map_err(|_| windows::core::Error::from(E_FAIL))?;
    unsafe {
        stream.Write(bytes.as_ptr().cast(), length, Some(&mut written)).ok()?;
        stream.Seek(0, STREAM_SEEK_SET, None)?;
    }
    if written != length {
        return Err(E_FAIL.into());
    }
    Ok(stream)
}

/// The response for a request of the preview origin: the file under `folder`, with the preview policy. Anything
/// else (outside the folder, missing, too large, unreadable) is a 404 that carries the policy too, so no preview
/// request is ever left to the network.
fn preview_response(env: &ICoreWebView2Environment, folder: &Path, uri: &str) -> windows::core::Result<ICoreWebView2WebResourceResponse> {
    let found = isolation::preview_file(folder, PREVIEW_ORIGIN, uri)
        .filter(|path| std::fs::metadata(path).is_ok_and(|meta| meta.len() <= isolation::MAX_PREVIEW_FILE_BYTES))
        .and_then(|path| std::fs::read(&path).ok().map(|bytes| (isolation::content_type(&path), bytes)));
    let (status, reason, mime, stream) = match found.and_then(|(mime, bytes)| memory_stream(&bytes).ok().map(|stream| (mime, stream))) {
        Some((mime, stream)) => (200, "OK", mime, Some(stream)),
        None => (404, "Not Found", "text/plain", None),
    };
    let reason_w = wide(reason);
    let headers_w = wide(&isolation::response_headers(mime));
    unsafe { env.CreateWebResourceResponse(stream.as_ref(), status, PCWSTR(reason_w.as_ptr()), PCWSTR(headers_w.as_ptr())) }
}

/// The reason a WebView2 process failure ends the session, or None for failures WebView2 recovers from itself
/// (GPU or utility process restarts, a subframe renderer).
fn failure_reason(kind: COREWEBVIEW2_PROCESS_FAILED_KIND) -> Option<String> {
    if kind == COREWEBVIEW2_PROCESS_FAILED_KIND_RENDER_PROCESS_UNRESPONSIVE {
        Some(format!("{}; the preview was closed", isolation::PREVIEW_UNRESPONSIVE))
    } else if kind == COREWEBVIEW2_PROCESS_FAILED_KIND_RENDER_PROCESS_EXITED {
        Some(format!("{} (renderer); the preview was closed", isolation::PREVIEW_PROCESS_EXITED))
    } else if kind == COREWEBVIEW2_PROCESS_FAILED_KIND_BROWSER_PROCESS_EXITED {
        Some(format!("{} (browser); the preview was closed", isolation::PREVIEW_PROCESS_EXITED))
    } else {
        None
    }
}

/// Test-only stand-in for a user who keeps using a preview: posts mouse and key input to the WebView2 windows, which
/// starts WebView2's input-acknowledgement watch on the renderer (`RenderProcessUnresponsive` needs pending input).
#[cfg(test)]
pub(crate) static SIMULATE_INPUT: AtomicBool = AtomicBool::new(false);

/// Test-only: leaves the WebRTC constructors in place, so a test checks the browser arguments on their own.
#[cfg(test)]
pub(crate) static KEEP_WEBRTC: AtomicBool = AtomicBool::new(false);

#[cfg(test)]
fn simulate_input(hwnd: HWND, step: isize) {
    use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_KEYDOWN, WM_KEYUP, WM_MOUSEMOVE};
    unsafe extern "system" fn post(child: HWND, lparam: LPARAM) -> BOOL {
        let step = lparam.0;
        unsafe {
            let _ = PostMessageW(Some(child), WM_MOUSEMOVE, WPARAM(0), LPARAM((10 + step % 50) | ((10 + step % 50) << 16)));
            let _ = PostMessageW(Some(child), WM_KEYDOWN, WPARAM(0x10), LPARAM(1));
            let _ = PostMessageW(Some(child), WM_KEYUP, WPARAM(0x10), LPARAM(1 | (0xC000_0000u32 as isize)));
        }
        BOOL(1)
    }
    unsafe {
        let _ = EnumChildWindows(Some(hwnd), Some(post), LPARAM(step));
    }
}

fn run_inside(
    options: &SessionOptions<'_>,
    hwnd: HWND,
    report: &mut SessionReport,
    overall_deadline: Option<Instant>,
    containment: Option<&Containment>,
) -> DltResult<()> {
    let served_folder = std::fs::canonicalize(options.folder)?;
    let user_w = wide_path(options.user_data);
    let browser_w = options.runtime.fixed_folder.as_deref().map(wide_path);
    let browser_ptr = match &browser_w {
        Some(v) => PCWSTR(v.as_ptr()),
        None => PCWSTR::null(),
    };

    // The preview needs no network: see `isolation::PREVIEW_BROWSER_ARGUMENTS`.
    let environment_options: ICoreWebView2EnvironmentOptions = {
        let options = CoreWebView2EnvironmentOptions::default();
        unsafe { options.set_additional_browser_arguments(isolation::PREVIEW_BROWSER_ARGUMENTS.to_owned()) };
        options.into()
    };
    let environment: ICoreWebView2Environment = {
        let (tx, rx) = mpsc::channel::<Result<ICoreWebView2Environment, HostError>>();
        let handler = CreateCoreWebView2EnvironmentCompletedHandler::create(Box::new(move |error_code, env| {
            let r = match (error_code, env) {
                (Err(e), _) => Err(HostError::Win(format!("environment: {e}"))),
                (Ok(()), Some(env)) => Ok(env),
                (Ok(()), None) => Err(HostError::Win("environment: null result".into())),
            };
            let _ = tx.send(r);
            Ok(())
        }));
        unsafe {
            CreateCoreWebView2EnvironmentWithOptions(
                browser_ptr,
                PCWSTR(user_w.as_ptr()),
                &environment_options,
                &handler,
            )
            .map_err(|e| {
                DltError::new(
                    ErrorCode::UnsupportedOperation,
                    format!("WebView2 is unavailable on this machine ({e}); web preview is disabled"),
                )
            })?;
        }
        match pump_until(&rx, remaining_timeout(overall_deadline, ENVIRONMENT_TIMEOUT), "environment", options.cancellation.as_deref(), hwnd, parent_hwnd_of(options)) {
            Ok(Ok(env)) => env,
            Ok(Err(e)) => {
                return Err(DltError::new(
                    ErrorCode::UnsupportedOperation,
                    format!("WebView2 environment failed; web preview is disabled: {}", match e {
                        HostError::Win(m) => m,
                        other => format!("{other:?}"),
                    }),
                ))
            }
            Err(e) => return Err(to_dlt(e, ErrorCode::Internal)),
        }
    };

    let controller: ControllerGuard = {
        let (tx, rx) = mpsc::channel::<Result<ICoreWebView2Controller, HostError>>();
        let handler = CreateCoreWebView2ControllerCompletedHandler::create(Box::new(move |error_code, c| {
            let r = match (error_code, c) {
                (Err(e), _) => Err(HostError::Win(format!("controller: {e}"))),
                (Ok(()), Some(c)) => Ok(c),
                (Ok(()), None) => Err(HostError::Win("controller: null result".into())),
            };
            let _ = tx.send(r);
            Ok(())
        }));
        unsafe {
            environment.CreateCoreWebView2Controller(hwnd, &handler).map_err(|e| to_dlt(HostError::from(e), ErrorCode::Internal))?;
        }
        match pump_until(&rx, remaining_timeout(overall_deadline, ENVIRONMENT_TIMEOUT), "controller", options.cancellation.as_deref(), hwnd, parent_hwnd_of(options)) {
            Ok(Ok(c)) => ControllerGuard(c),
            Ok(Err(e)) => return Err(to_dlt(e, ErrorCode::Internal)),
            Err(e) => return Err(to_dlt(e, ErrorCode::Internal)),
        }
    };

    report.browser_version = unsafe {
        let mut version_ptr = PWSTR::null();
        environment.BrowserVersionString(&mut version_ptr).map_err(|e| to_dlt(HostError::from(e), ErrorCode::Internal))?;
        take_pwstr(version_ptr)
    };

    let (width, height) = client_size(hwnd);
    let mut last_size = (0, 0);
    let (msg_tx, msg_rx) = mpsc::channel::<String>();
    let (nav_tx, nav_rx) = mpsc::channel::<bool>();
    let blocked_requests = Arc::new(AtomicUsize::new(0));
    let blocked_navigations = Arc::new(AtomicUsize::new(0));
    let new_windows = Arc::new(AtomicUsize::new(0));

    let webview = unsafe {
        controller
            .SetBounds(RECT { left: 0, top: 0, right: width, bottom: height })
            .map_err(|e| to_dlt(HostError::from(e), ErrorCode::Internal))?;
        controller.SetIsVisible(true).map_err(|e| to_dlt(HostError::from(e), ErrorCode::Internal))?;
        controller.CoreWebView2().map_err(|e| to_dlt(HostError::from(e), ErrorCode::Internal))?
    };

    unsafe {
        let settings = webview.Settings().map_err(|e| to_dlt(HostError::from(e), ErrorCode::Internal))?;
        let _ = settings.SetAreDevToolsEnabled(false);
        let _ = settings.SetAreDefaultContextMenusEnabled(false);
        let _ = settings.SetAreDefaultScriptDialogsEnabled(false);
        let _ = settings.SetIsStatusBarEnabled(false);
        let _ = settings.SetIsZoomControlEnabled(false);
        let _ = settings.SetIsWebMessageEnabled(true);
    }

    // Second layer behind the browser arguments (see `isolation::REMOVE_WEBRTC_SCRIPT`). Waited for, so it is in
    // place before the first navigation.
    #[cfg(test)]
    let remove_webrtc = !KEEP_WEBRTC.load(Ordering::SeqCst);
    #[cfg(not(test))]
    let remove_webrtc = true;
    let mut ready_id = std::env::var(crate::REPORT_READY_ENV).ok().filter(|_| options.parent.is_some() && !options.smoke);
    if remove_webrtc {
        let (tx, rx) = mpsc::channel::<windows::core::Result<()>>();
        let handler = AddScriptToExecuteOnDocumentCreatedCompletedHandler::create(Box::new(move |result, _id| {
            let _ = tx.send(result);
            Ok(())
        }));
        let script = wide(isolation::REMOVE_WEBRTC_SCRIPT);
        unsafe {
            webview
                .AddScriptToExecuteOnDocumentCreated(PCWSTR(script.as_ptr()), &handler)
                .map_err(|e| to_dlt(HostError::from(e), ErrorCode::Internal))?
        };
        match pump_until(&rx, remaining_timeout(overall_deadline, ENVIRONMENT_TIMEOUT), "document script", options.cancellation.as_deref(), hwnd, parent_hwnd_of(options)) {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(to_dlt(HostError::from(e), ErrorCode::Internal)),
            Err(e) => return Err(to_dlt(e, ErrorCode::Internal)),
        }
    }

    // Navigate below starts the first top-level preview navigation. Its ID survives same-document
    // history changes and distinguishes its DOM event from about:blank or a later document.
    let entry_navigation = Rc::new(Cell::new(None));
    let entry_dom_ready = Rc::new(Cell::new(false));
    let entry_served = Rc::new(Cell::new(None));
    if ready_id.is_some() {
        let is_entry = Rc::new(Cell::new(false));
        let current = Rc::clone(&is_entry);
        let entry = Rc::clone(&entry_navigation);
        let ready = Rc::clone(&entry_dom_ready);
        let loading = ContentLoadingEventHandler::create(Box::new(move |_sender, args| {
            if let Some(args) = args {
                let mut id = 0;
                let mut error_page = BOOL(0);
                unsafe { args.NavigationId(&mut id)?; args.IsErrorPage(&mut error_page)?; }
                current.set(entry.get() == Some(id) && !error_page.as_bool());
                ready.set(false);
            }
            Ok(())
        }));
        let mut token = 0;
        unsafe { webview.add_ContentLoading(&loading, &mut token).map_err(|e| to_dlt(HostError::from(e), ErrorCode::Internal))? };
        let entry = Rc::clone(&entry_navigation);
        let ready = Rc::clone(&entry_dom_ready);
        let handler = DOMContentLoadedEventHandler::create(Box::new(move |_sender, args| {
            if let Some(args) = args {
                let mut id = 0;
                unsafe { args.NavigationId(&mut id)? };
                if is_entry.get() && entry.get() == Some(id) { ready.set(true); }
            }
            Ok(())
        }));
        let webview2 = webview.cast::<ICoreWebView2_2>().map_err(|e| to_dlt(HostError::from(e), ErrorCode::Unavailable))?;
        unsafe { webview2.add_DOMContentLoaded(&handler, &mut token).map_err(|e| to_dlt(HostError::from(e), ErrorCode::Internal))? };
    }

    // Navigation outside the preview origin is cancelled.
    {
        let blocked = Arc::clone(&blocked_navigations);
        let entry = Rc::clone(&entry_navigation);
        let handler = NavigationStartingEventHandler::create(Box::new(move |_sender, args| {
            if let Some(args) = args {
                let mut p = PWSTR::null();
                unsafe { args.Uri(&mut p)? };
                let uri = take_pwstr(p);
                if !is_preview_uri(&uri) {
                    unsafe { args.SetCancel(true)? };
                    blocked.fetch_add(1, Ordering::SeqCst);
                } else if entry.get().is_none() && uri.starts_with(PREVIEW_ORIGIN) {
                    let mut id = 0;
                    unsafe { args.NavigationId(&mut id)? };
                    entry.set(Some(id));
                }
            }
            Ok(())
        }));
        let mut token = 0i64;
        unsafe { webview.add_NavigationStarting(&handler, &mut token).map_err(|e| to_dlt(HostError::from(e), ErrorCode::Internal))? };
    }

    // New windows are never opened.
    {
        let count = Arc::clone(&new_windows);
        let handler = NewWindowRequestedEventHandler::create(Box::new(move |_sender, args| {
            if let Some(args) = args {
                unsafe { args.SetHandled(true)? };
                count.fetch_add(1, Ordering::SeqCst);
            }
            Ok(())
        }));
        let mut token = 0i64;
        unsafe { webview.add_NewWindowRequested(&handler, &mut token).map_err(|e| to_dlt(HostError::from(e), ErrorCode::Internal))? };
    }

    // Permission prompts are denied. A prompt would otherwise open browser UI.
    {
        let handler = PermissionRequestedEventHandler::create(Box::new(move |_sender, args| {
            if let Some(args) = args {
                unsafe { args.SetState(COREWEBVIEW2_PERMISSION_STATE_DENY)? };
            }
            Ok(())
        }));
        let mut token = 0i64;
        unsafe { webview.add_PermissionRequested(&handler, &mut token).map_err(|e| to_dlt(HostError::from(e), ErrorCode::Internal))? };
    }

    // Downloads are cancelled. A preview never writes files on its own.
    {
        let handler = DownloadStartingEventHandler::create(Box::new(move |_sender, args| {
            if let Some(args) = args {
                unsafe { args.SetCancel(true)? };
            }
            Ok(())
        }));
        let mut token = 0i64;
        // Download events are on ICoreWebView2_4, which every Evergreen runtime since 2020 implements.
        let webview4: ICoreWebView2_4 = webview.cast().map_err(|e| to_dlt(HostError::from(e), ErrorCode::Internal))?;
        unsafe { webview4.add_DownloadStarting(&handler, &mut token).map_err(|e| to_dlt(HostError::from(e), ErrorCode::Internal))? };
    }

    // External URI schemes (mailto:, tel:, ...) never start another application. NavigationStarting already cancels
    // them in the main frame; this covers any path that reaches the launch step. Runtimes before ICoreWebView2_18
    // (2023) do not have the event and keep only the navigation rule.
    if let Ok(webview18) = webview.cast::<ICoreWebView2_18>() {
        let blocked = Arc::clone(&blocked_navigations);
        let handler = LaunchingExternalUriSchemeEventHandler::create(Box::new(move |_sender, args| {
            if let Some(args) = args {
                unsafe { args.SetCancel(true)? };
                blocked.fetch_add(1, Ordering::SeqCst);
            }
            Ok(())
        }));
        let mut token = 0i64;
        unsafe { webview18.add_LaunchingExternalUriScheme(&handler, &mut token).map_err(|e| to_dlt(HostError::from(e), ErrorCode::Internal))? };
    }

    // Process failures end the session with their reason (see `failure_reason`).
    let (failed_tx, failed_rx) = mpsc::channel::<COREWEBVIEW2_PROCESS_FAILED_KIND>();
    {
        let handler = ProcessFailedEventHandler::create(Box::new(move |_sender, args| {
            if let Some(args) = args {
                let mut kind = COREWEBVIEW2_PROCESS_FAILED_KIND::default();
                unsafe { args.ProcessFailedKind(&mut kind)? };
                let _ = failed_tx.send(kind);
            }
            Ok(())
        }));
        let mut token = 0i64;
        unsafe { webview.add_ProcessFailed(&handler, &mut token).map_err(|e| to_dlt(HostError::from(e), ErrorCode::Internal))? };
    }

    // The preview origin is served here from the folder, every response with the preview policy (a WebView2
    // virtual host mapping would bypass this event and so the header). Requests to hosts other than the preview
    // origin receive 403 and never reach the network.
    {
        let blocked = Arc::clone(&blocked_requests);
        let env = environment.clone();
        let served = Rc::clone(&entry_served);
        let handler = WebResourceRequestedEventHandler::create(Box::new(move |_sender, args| {
            if let Some(args) = args {
                let request = unsafe { args.Request()? };
                let mut p = PWSTR::null();
                unsafe { request.Uri(&mut p)? };
                let uri = take_pwstr(p);
                if uri.starts_with(PREVIEW_ORIGIN) {
                    let response = preview_response(&env, &served_folder, &uri)?;
                    if served.get().is_none() {
                        let mut context = COREWEBVIEW2_WEB_RESOURCE_CONTEXT_ALL;
                        unsafe { args.ResourceContext(&mut context)? };
                        if context == COREWEBVIEW2_WEB_RESOURCE_CONTEXT_DOCUMENT {
                            let mut status = 0;
                            unsafe { response.StatusCode(&mut status)? };
                            served.set(Some(status == 200));
                        }
                    }
                    unsafe { args.SetResponse(&response)? };
                } else if !(uri.starts_with("data:") || uri.starts_with("blob:")) {
                    let response = unsafe {
                        env.CreateWebResourceResponse(
                            None::<&windows::Win32::System::Com::IStream>,
                            403,
                            w!("Blocked by DesignLLM preview"),
                            w!("Content-Type: text/plain"),
                        )?
                    };
                    unsafe { args.SetResponse(&response)? };
                    blocked.fetch_add(1, Ordering::SeqCst);
                }
            }
            Ok(())
        }));
        let mut token = 0i64;
        unsafe {
            webview
                .AddWebResourceRequestedFilter(w!("*"), COREWEBVIEW2_WEB_RESOURCE_CONTEXT_ALL)
                .map_err(|e| to_dlt(HostError::from(e), ErrorCode::Internal))?;
            webview.add_WebResourceRequested(&handler, &mut token).map_err(|e| to_dlt(HostError::from(e), ErrorCode::Internal))?;
        }
    }

    // Page messages. The page has no other channel to the host.
    {
        let handler = WebMessageReceivedEventHandler::create(Box::new(move |_sender, args| {
            if let Some(args) = args {
                let mut p = PWSTR::null();
                unsafe { args.TryGetWebMessageAsString(&mut p)? };
                let _ = msg_tx.send(take_pwstr(p));
            }
            Ok(())
        }));
        let mut token = 0i64;
        unsafe { webview.add_WebMessageReceived(&handler, &mut token).map_err(|e| to_dlt(HostError::from(e), ErrorCode::Internal))? };
    }

    // Navigation completion. A failed navigation ends a smoke session immediately.
    {
        let entry = Rc::clone(&entry_navigation);
        let awaiting_entry = ready_id.is_some();
        let handler = NavigationCompletedEventHandler::create(Box::new(move |_sender, args| {
            let success = match args {
                Some(args) => {
                    if awaiting_entry {
                        let mut id = 0;
                        unsafe { args.NavigationId(&mut id)? };
                        if entry.get() != Some(id) { return Ok(()); }
                    }
                    let mut ok = BOOL(0);
                    unsafe { args.IsSuccess(&mut ok)? };
                    ok.as_bool()
                }
                None => false,
            };
            let _ = nav_tx.send(success);
            Ok(())
        }));
        let mut token = 0i64;
        unsafe { webview.add_NavigationCompleted(&handler, &mut token).map_err(|e| to_dlt(HostError::from(e), ErrorCode::Internal))? };
    }

    // No folder mapping: the WebResourceRequested handler above serves the folder read-only on the preview origin.
    // Its responses have no CORS headers, so other origins cannot read them (what DENY_CORS did for the mapping).
    let url_w = wide(&options.entry_url);
    unsafe { webview.Navigate(PCWSTR(url_w.as_ptr())).map_err(|e| to_dlt(HostError::from(e), ErrorCode::Internal))? };

    let page_deadline = Instant::now() + options.timeout;
    let deadline = overall_deadline.map_or(page_deadline, |overall| overall.min(page_deadline));
    bounds::sync_bounds(&mut last_size, client_size(hwnd), |rect| unsafe { controller.SetBounds(rect) })
        .map_err(|e| to_dlt(HostError::from(e), ErrorCode::Internal))?;
    let mut ready = false;
    #[cfg(test)]
    let mut input_step: isize = 0;
    loop {
        if options.cancellation.as_ref().is_some_and(|cancel| cancel.load(Ordering::SeqCst)) {
            return Err(DltError::new(ErrorCode::Canceled, "WebView2 video poster session was canceled"));
        }
        pump_messages();
        // Memory first: a renderer that fails to allocate may also exit, and the limit is the reason.
        if let Some(containment) = containment {
            if containment.memory_limit_reached() {
                containment.end_other_processes();
                return Err(DltError::new(
                    ErrorCode::Unavailable,
                    format!(
                        "{} ({} MiB for the preview process and its WebView2 processes); the preview was closed",
                        isolation::PREVIEW_MEMORY_LIMIT,
                        isolation::PREVIEW_MEMORY_LIMIT_BYTES / (1024 * 1024)
                    ),
                ));
            }
        }
        while let Ok(kind) = failed_rx.try_recv() {
            if let Some(reason) = failure_reason(kind) {
                return Err(DltError::new(ErrorCode::Unavailable, reason));
            }
        }
        #[cfg(test)]
        if SIMULATE_INPUT.load(Ordering::SeqCst) {
            input_step += 1;
            if input_step % 100 == 0 {
                simulate_input(hwnd, input_step / 100);
            }
        }
        while let Ok(message) = msg_rx.try_recv() {
            report.messages.push(message.clone());
            if message == READY_MESSAGE {
                ready = true;
            }
        }
        while let Ok(success) = nav_rx.try_recv() {
            // Only the load of the preview decides. A later navigation the host refused (to another origin or an
            // external scheme) completes unsuccessfully and leaves the loaded page in place.
            if report.navigation_success {
                continue;
            }
            report.navigation_success = success;
            if !success && (options.smoke || ready_id.is_some()) {
                return Err(DltError::new(ErrorCode::Internal, "preview navigation failed"));
            }
        }
        if ready_id.is_some() && entry_served.get() == Some(false) {
            return Err(DltError::new(ErrorCode::NotFound, "preview entry could not be served"));
        }
        if !options.smoke && window_gone(hwnd, parent_hwnd_of(options)) {
            break;
        }
        if let Some(id) = ready_id.as_ref().filter(|_| entry_dom_ready.get() && entry_served.get() == Some(true)) {
            let mut out = std::io::stdout().lock();
            write!(out, "\n{READY_MESSAGE} {id}\n")?;
            out.flush()?;
            ready_id = None;
        }
        if options.smoke {
            // A fast inline script may post readiness before NavigationCompleted is delivered.
            if ready && report.navigation_success {
                break;
            }
            if Instant::now() >= deadline {
                return Err(DltError::new(
                    ErrorCode::Internal,
                    format!("preview did not post {READY_MESSAGE} within the smoke timeout"),
                ));
            }
        } else {
            if STOP.load(Ordering::SeqCst) {
                break;
            }
            let size = client_size(hwnd);
            let _ = bounds::sync_bounds(&mut last_size, size, |rect| unsafe { controller.SetBounds(rect) });
        }
        std::thread::sleep(Duration::from_millis(5));
    }

    let (host_visible, child_windows, visible_children) = visibility(hwnd);
    report.host_visible = host_visible;
    report.child_windows = child_windows;
    report.visible_child_windows = visible_children;
    let (total, visible) = process_top_level_windows();
    report.process_top_level_windows = total;
    report.process_visible_top_level_windows = visible;
    report.blocked_requests = blocked_requests.load(Ordering::SeqCst);
    report.blocked_navigations = blocked_navigations.load(Ordering::SeqCst);
    report.new_windows_blocked = new_windows.load(Ordering::SeqCst);

    Ok(())
}

fn parent_hwnd_of(options: &SessionOptions<'_>) -> Option<HWND> {
    options.parent.map(|v| HWND(v as *mut core::ffi::c_void))
}
