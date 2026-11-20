//! designllm-web-preview: Rust WebView2 host for WebCode previews.
//!
//! Public entry points:
//! - `run(path, parent_hwnd, invisible)`: the frozen contract signature. `invisible = true` runs a smoke session
//!   (returns Ok when the page posts `READY_MESSAGE`, error at the timeout). `invisible = false` runs until
//!   `request_stop()`, the parent window is destroyed, or the standalone window is closed.
//! - `smoke(path, timeout)`: invisible session that returns the observed facts (see `SmokeReport`).
//! - `inspect_video_source(path)`: bounded local MP4/WebM type, size, and SHA preflight before asset import.
//! - `extract_video_poster(path, expected_asset_sha256, timeout)`: bounded first-frame PNG extraction from a local MP4/WebM.
//!   Use `extract_video_poster_cancellable` when a caller needs per-request cancellation.
//! - `request_stop()`: ends the current embedded or visible session.
//!
//! Runtime: see `runtime.rs`. A fixed-version runtime (`DESIGNLLM_WEBVIEW2_DIR` or `runtime/webview2`) is preferred.
//! The installed Evergreen runtime is a labelled development fallback.
//! This crate never installs or downloads a runtime.
//!
//! Host window handle contract for the desktop GUI: pass the HWND of a child window that the GUI created for the
//! preview area. The host fills that window and follows its size. The host never activates or focuses a window.

mod isolation;
mod runtime;
mod stale_temp;

pub use stale_temp::{process_tag, remove_orphaned_temp_files, remove_orphaned_video_poster_work};

#[cfg(windows)]
mod host;

#[cfg(any(windows, test))]
use base64::{engine::general_purpose::STANDARD, Engine as _};
use designllm_contracts::{DltError, DltResult, ErrorCode};
use designllm_contracts::media_header::{mp4_has_video_track, webm_has_video_track};
use sha2::{Digest, Sha256};
#[cfg(windows)]
use designllm_contracts::new_id;
use std::{fs::File, io::Read};
#[cfg(windows)]
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub use isolation::{
    PREVIEW_CONTENT_SECURITY_POLICY, PREVIEW_MEMORY_LIMIT, PREVIEW_MEMORY_LIMIT_BYTES, PREVIEW_PROCESS_EXITED, PREVIEW_UNRESPONSIVE,
};
pub use runtime::{
    resolve_runtime, runtime_unavailable_reason, select_runtime, user_data_folder, RuntimeSelection, RuntimeSource, RUNTIME_EXECUTABLE,
    RUNTIME_RELATIVE_DIR, WEBVIEW2_DIR_ENV, WEBVIEW2_REQUIRE_FIXED_ENV, WEBVIEW2_USER_DATA_ENV,
};

/// Virtual host name that serves the project folder.
pub const PREVIEW_HOST: &str = "designllm-preview.local";
/// Origin of the preview. Navigation and resource requests outside it are blocked.
pub const PREVIEW_ORIGIN: &str = "https://designllm-preview.local/";
/// Message a smoke page posts with `window.chrome.webview.postMessage` when it has loaded.
pub const READY_MESSAGE: &str = "designllm.preview.ready";
/// The embedded child's run ID. Its stdout acknowledgment is LF, READY_MESSAGE, space, ID, LF.
pub const REPORT_READY_ENV: &str = "DESIGNLLM_PREVIEW_REPORT_READY";
/// Default smoke timeout.
pub const DEFAULT_SMOKE_TIMEOUT: Duration = Duration::from_secs(60);
/// Maximum local video size read by the poster extractor.
pub const MAX_VIDEO_INPUT_BYTES: u64 = 128 * 1024 * 1024;
/// Maximum decoded source pixels accepted for an automatic poster.
pub const MAX_VIDEO_POSTER_PIXELS: u64 = 16 * 1024 * 1024;
/// Maximum encoded PNG poster returned by the hidden WebView2 session.
pub const MAX_VIDEO_POSTER_PNG_BYTES: usize = 8 * 1024 * 1024;
/// Maximum timeout accepted by the poster extractor.
pub const MAX_VIDEO_POSTER_TIMEOUT: Duration = Duration::from_secs(60);
/// Default timeout for waiting for the media element's first decoded frame.
pub const DEFAULT_VIDEO_POSTER_TIMEOUT: Duration = Duration::from_secs(30);
#[cfg(any(windows, test))]
const VIDEO_POSTER_MESSAGE_PREFIX: &str = "designllm.video.poster:";
#[cfg(windows)]
const VIDEO_ERROR_MESSAGE_PREFIX: &str = "designllm.video.error:";
#[cfg(windows)]
const VIDEO_POSTER_READY_MESSAGE: &str = "designllm.video.poster.ready";

/// A PNG of the first decoded frame of a local MP4 or WebM file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoPoster {
    pub png: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// Local video facts verified before importing it into the workspace asset store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoSourceInfo {
    pub mime: String,
    pub bytes: u64,
    pub sha256: String,
}

/// Validates a local MP4/WebM container and computes its bounded source hash before asset import.
/// This reads and hashes the file synchronously, so callers should run it off the UI thread.
pub fn inspect_video_source(path: &Path) -> DltResult<VideoSourceInfo> {
    inspect_video_source_cancellable(path, DEFAULT_VIDEO_POSTER_TIMEOUT, &VideoPosterCancellation::default())
}

/// Cancellable preflight variant for a worker that may be abandoned while reading a large local file.
pub fn inspect_video_source_cancellable(
    path: &Path,
    timeout: Duration,
    cancellation: &VideoPosterCancellation,
) -> DltResult<VideoSourceInfo> {
    if timeout.is_zero() { return Err(DltError::invalid("video source inspection timeout must be greater than zero")); }
    if cancellation.is_cancelled() { return Err(DltError::new(ErrorCode::Canceled, "video source inspection was canceled")); }
    let deadline = Instant::now() + timeout.min(MAX_VIDEO_POSTER_TIMEOUT);
    read_video_source(path, Some(cancellation), deadline).map(|(_, _, info)| info)
}

/// Per-request cancellation for video poster extraction. It never stops other WebView2 sessions.
#[derive(Clone, Default)]
pub struct VideoPosterCancellation(Arc<AtomicBool>);

impl VideoPosterCancellation {
    /// Cancels the matching poster extraction if it is still running.
    pub fn cancel(&self) { self.0.store(true, Ordering::SeqCst); }

    /// Whether cancellation has been requested.
    pub fn is_cancelled(&self) -> bool { self.0.load(Ordering::SeqCst) }
}

/// A resolved preview: the folder to map and the entry URL under the preview origin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviewTarget {
    pub folder: PathBuf,
    pub entry: String,
    pub url: String,
}

/// Facts observed during a smoke session. Tests and the GUI smoke command read these.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SmokeReport {
    pub runtime: RuntimeSelection,
    pub browser_version: String,
    pub navigation_success: bool,
    /// Every web message the page posted, in order. Includes `READY_MESSAGE`.
    pub messages: Vec<String>,
    pub blocked_requests: usize,
    pub blocked_navigations: usize,
    pub new_windows_blocked: usize,
    pub host_visible: bool,
    pub child_windows: usize,
    pub visible_child_windows: usize,
    /// Top-level windows of this process, counted at the end of the session. Smoke expects 0 visible.
    pub process_top_level_windows: usize,
    pub process_visible_top_level_windows: usize,
}

fn percent_encode_segment(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

fn clean_windows_path(path: PathBuf) -> PathBuf {
    let text = path.to_string_lossy().into_owned();
    match text.strip_prefix(r"\\?\") {
        Some(rest) if !rest.starts_with("UNC\\") => PathBuf::from(rest),
        _ => path,
    }
}

/// Resolves a file or folder into a folder to map and an entry under the preview origin.
pub fn preview_target(path: &Path) -> DltResult<PreviewTarget> {
    let meta = std::fs::metadata(path)
        .map_err(|_| DltError::new(ErrorCode::NotFound, format!("preview path does not exist: {}", path.display())))?;
    let (folder, entry) = if meta.is_dir() {
        if !path.join("index.html").is_file() {
            return Err(DltError::invalid(format!("{} has no index.html", path.display())));
        }
        (path.to_path_buf(), "index.html".to_string())
    } else if meta.is_file() {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .ok_or_else(|| DltError::invalid("preview file has no name"))?;
        let lower = name.to_ascii_lowercase();
        if !(lower.ends_with(".html") || lower.ends_with(".htm")) {
            return Err(DltError::invalid("only .html or .htm files can be previewed"));
        }
        let folder = path.parent().map(Path::to_path_buf).unwrap_or_default();
        (folder, name)
    } else {
        return Err(DltError::invalid("preview path is neither a file nor a folder"));
    };
    let folder = clean_windows_path(std::fs::canonicalize(&folder).unwrap_or(folder));
    if folder.to_string_lossy().starts_with(r"\\") {
        return Err(DltError::new(
            ErrorCode::UnsupportedOperation,
            "preview folders on network paths are not supported (WebView2 Fixed Version cannot run from UNC paths)",
        ));
    }
    let url = format!("{PREVIEW_ORIGIN}{}", percent_encode_segment(&entry));
    Ok(PreviewTarget { folder, entry, url })
}

#[cfg(windows)]
fn host_options<'a>(
    target: &'a PreviewTarget,
    parent_hwnd: Option<isize>,
    invisible: bool,
    smoke: bool,
    timeout: Duration,
    runtime: &'a RuntimeSelection,
    user_data: &'a Path,
) -> host::SessionOptions<'a> {
    host::SessionOptions {
        folder: &target.folder,
        entry_url: target.url.clone(),
        parent: parent_hwnd,
        invisible,
        smoke,
        timeout,
        total_timeout: None,
        runtime,
        user_data,
        cancellation: None,
        contain: false,
    }
}

/// Runs the preview host. See the crate documentation for the window modes.
pub fn run(path: &Path, parent_hwnd: Option<isize>, invisible: bool) -> DltResult<()> {
    #[cfg(windows)]
    {
        let runtime = resolve_runtime()?;
        let target = preview_target(path)?;
        let user_data = user_data_folder()?;
        let mut options = host_options(&target, parent_hwnd, invisible, invisible, DEFAULT_SMOKE_TIMEOUT, &runtime, &user_data);
        options.contain = true;
        host::run_session(options).map(|_| ())
    }
    #[cfg(not(windows))]
    {
        let _ = (path, parent_hwnd, invisible);
        Err(DltError::new(ErrorCode::UnsupportedOperation, "the WebView2 preview host runs on Windows only"))
    }
}

/// Invisible smoke: loads the page, waits for `READY_MESSAGE`, and reports what was observed.
pub fn smoke(path: &Path, timeout: Duration) -> DltResult<SmokeReport> {
    #[cfg(windows)]
    {
        let runtime = resolve_runtime()?;
        let target = preview_target(path)?;
        let user_data = user_data_folder()?;
        let options = host_options(&target, None, true, true, timeout, &runtime, &user_data);
        let session = host::run_session(options)?;
        Ok(SmokeReport {
            runtime,
            browser_version: session.browser_version,
            navigation_success: session.navigation_success,
            messages: session.messages,
            blocked_requests: session.blocked_requests,
            blocked_navigations: session.blocked_navigations,
            new_windows_blocked: session.new_windows_blocked,
            host_visible: session.host_visible,
            child_windows: session.child_windows,
            visible_child_windows: session.visible_child_windows,
            process_top_level_windows: session.process_top_level_windows,
            process_visible_top_level_windows: session.process_visible_top_level_windows,
        })
    }
    #[cfg(not(windows))]
    {
        let _ = (path, timeout);
        Err(DltError::new(ErrorCode::UnsupportedOperation, "the WebView2 preview host runs on Windows only"))
    }
}

/// Extracts a bounded PNG of the first decoded frame of a local MP4 or WebM.
///
/// The input is read only and must match `expected_asset_sha256`, which binds the poster to the asset returned by
/// `asset.import`. `timeout` covers chunked input reading, hashing, scratch-copying, and media loading; the host gets
/// an additional 30 seconds for WebView2 startup and shutdown. This function blocks while a hidden WebView2 session
/// decodes the frame, so
/// callers must run it off the UI thread. The session uses a private per-call WebView2 user-data directory and
/// serves only a temporary local copy on the isolated preview origin. The pixel limit is checked at `loadedmetadata` before
/// the page requests full preload or allocates its canvas, then checked again at `loadeddata`; a codec may still
/// allocate internal buffers while parsing metadata, so this cap does not hard-bound WebView2's private memory use.
pub fn extract_video_poster(path: &Path, expected_asset_sha256: &str, timeout: Duration) -> DltResult<VideoPoster> {
    extract_video_poster_cancellable(path, expected_asset_sha256, timeout, &VideoPosterCancellation::default())
}


/// Extracts a video poster with a per-session cancellation token.
pub fn extract_video_poster_cancellable(
    path: &Path,
    expected_asset_sha256: &str,
    timeout: Duration,
    cancellation: &VideoPosterCancellation,
) -> DltResult<VideoPoster> {
    if timeout.is_zero() {
        return Err(DltError::invalid("video poster timeout must be greater than zero"));
    }
    if expected_asset_sha256.len() != 64 || !expected_asset_sha256.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(DltError::invalid("expected video asset SHA-256 must contain 64 hexadecimal characters"));
    }
    if cancellation.is_cancelled() {
        return Err(DltError::new(ErrorCode::Canceled, "video poster extraction was canceled"));
    }
    let timeout = timeout.min(MAX_VIDEO_POSTER_TIMEOUT);
    let deadline = Instant::now() + timeout;

    #[cfg(windows)]
    {
        extract_video_poster_windows(path, expected_asset_sha256, deadline, cancellation)
    }
    #[cfg(not(windows))]
    {
        let _ = (path, expected_asset_sha256, timeout, deadline, cancellation);
        Err(DltError::new(ErrorCode::UnsupportedOperation, "automatic video poster extraction requires WebView2 on Windows"))
    }
}

#[cfg(any(windows, test))]
const VIDEO_POSTER_HTML: &str = r#"<!doctype html>
<html><head><meta charset="utf-8"><title>Video poster extraction</title></head>
<body><video id="source" muted playsinline preload="metadata"></video>
<script>
(function () {
  "use strict";
  const MAX_PIXELS = __MAX_PIXELS__;
  const MAX_PNG_BYTES = __MAX_PNG_BYTES__;
  const MEDIA_TIMEOUT_MS = __MEDIA_TIMEOUT_MS__;
  const SOURCE = "__VIDEO_FILE__";
  const post = (message) => window.chrome.webview.postMessage(message);
  const video = document.getElementById("source");
  let settled = false;
  let dimensionsAccepted = false;
  const finish = () => {
    if (settled) return false;
    settled = true;
    clearTimeout(timer);
    post("designllm.video.poster.ready");
    post("designllm.preview.ready");
    return true;
  };
  const fail = (reason) => {
    if (settled) return;
    post("designllm.video.error:" + SOURCE + ":" + reason);
    finish();
  };
  const timer = setTimeout(() => fail("loadeddata-timeout"), MEDIA_TIMEOUT_MS);
  video.addEventListener("loadedmetadata", () => {
    if (settled || dimensionsAccepted) return;
    const width = video.videoWidth;
    const height = video.videoHeight;
    const pixels = width * height;
    if (!(width > 0 && height > 0 && Number.isSafeInteger(pixels) && pixels <= MAX_PIXELS)) {
      fail("metadata-pixel-limit-or-empty-frame");
      return;
    }
    dimensionsAccepted = true;
    video.preload = "auto";
    video.load();
  });
  video.addEventListener("error", () => {
    if (!settled) fail("media-code-" + (video.error ? video.error.code : 0));
  }, { once: true });
  video.addEventListener("loadeddata", () => {
    if (settled) return;
    const width = video.videoWidth;
    const height = video.videoHeight;
    const pixels = width * height;
    if (!dimensionsAccepted || !(width > 0 && height > 0 && Number.isSafeInteger(pixels) && pixels <= MAX_PIXELS)) {
      fail("pixel-limit-or-empty-frame");
      return;
    }
    try {
      const canvas = document.createElement("canvas");
      canvas.width = width;
      canvas.height = height;
      const context = canvas.getContext("2d");
      if (!context) throw new Error("2d canvas is unavailable");
      context.drawImage(video, 0, 0, width, height);
      canvas.toBlob((blob) => {
        if (settled) return;
        if (!blob) { fail("png-encoding-failed"); return; }
        if (blob.size > MAX_PNG_BYTES) { fail("png-size-limit"); return; }
        const reader = new FileReader();
        reader.onload = () => {
          if (settled) return;
          const dataUrl = typeof reader.result === "string" ? reader.result : "";
          if (!dataUrl.startsWith("data:image/png;base64,")) { fail("invalid-png-data-url"); return; }
          post("designllm.video.poster:" + SOURCE + ":" + width + ":" + height + ":" + dataUrl);
          finish();
        };
        reader.onerror = () => fail("png-read-failed");
        reader.readAsDataURL(blob);
      }, "image/png");
    } catch (error) {
      fail("canvas-error-" + String(error));
    }
  }, { once: true });
  video.src = SOURCE;
  video.load();
})();
</script></body></html>
"#;

#[cfg(any(windows, test))]
fn video_poster_html(file_name: &str, timeout: Duration) -> String {
    let timeout_ms = timeout.as_millis().max(1).min(u128::from(u64::MAX));
    VIDEO_POSTER_HTML
        .replace("__MAX_PIXELS__", &MAX_VIDEO_POSTER_PIXELS.to_string())
        .replace("__MAX_PNG_BYTES__", &MAX_VIDEO_POSTER_PNG_BYTES.to_string())
        .replace("__MEDIA_TIMEOUT_MS__", &timeout_ms.to_string())
        .replace("__VIDEO_FILE__", file_name)
}

#[cfg(any(windows, test))]
fn parse_video_poster_message(message: &str, expected_file: &str) -> DltResult<VideoPoster> {
    let invalid_message = || DltError::new(ErrorCode::IntegrityError, "WebView2 returned an invalid video poster message");
    let mut fields = message.strip_prefix(VIDEO_POSTER_MESSAGE_PREFIX).ok_or_else(invalid_message)?.splitn(4, ':');
    let file_name = fields.next().ok_or_else(invalid_message)?;
    if file_name != expected_file { return Err(invalid_message()); }
    let width = fields.next().ok_or_else(invalid_message)?.parse::<u32>().map_err(|_| invalid_message())?;
    let height = fields.next().ok_or_else(invalid_message)?.parse::<u32>().map_err(|_| invalid_message())?;
    let data_url = fields.next().ok_or_else(invalid_message)?;
    let encoded = data_url.strip_prefix("data:image/png;base64,").ok_or_else(invalid_message)?;
    let max_encoded_bytes = MAX_VIDEO_POSTER_PNG_BYTES.saturating_add(2) / 3 * 4;
    if encoded.len() > max_encoded_bytes { return Err(DltError::invalid("video poster PNG exceeds the encoded size limit")); }
    let png = STANDARD.decode(encoded).map_err(|_| invalid_message())?;
    if png.len() > MAX_VIDEO_POSTER_PNG_BYTES { return Err(invalid_message()); }
    // Signature, IHDR length 13, and positive sizes: the shared header rule (contracts `media_header`).
    let (png_width, png_height) = designllm_contracts::media_header::png_size(&png).ok_or_else(invalid_message)?;
    let pixels = u64::from(width).checked_mul(u64::from(height)).ok_or_else(invalid_message)?;
    if width == 0 || height == 0 || pixels > MAX_VIDEO_POSTER_PIXELS
        || (png_width, png_height) != (width, height)
    {
        return Err(DltError::invalid("video poster dimensions are empty, over the pixel limit, or inconsistent with its PNG"));
    }
    Ok(VideoPoster { png, width, height })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum VideoContainer { Mp4, Webm }

impl VideoContainer {
    fn file_name(self) -> &'static str {
        match self { Self::Mp4 => "video.mp4", Self::Webm => "video.webm" }
    }

    fn mime(self) -> &'static str {
        match self { Self::Mp4 => "video/mp4", Self::Webm => "video/webm" }
    }
}

fn video_container(path: &Path, bytes: &[u8]) -> DltResult<VideoContainer> {
    let extension = path.extension().and_then(|extension| extension.to_str()).unwrap_or_default().to_ascii_lowercase();
    match extension.as_str() {
        // The same MP4 and WebM rules as Canvas' InsertVideo check: the container and a video track.
        "mp4" if mp4_has_video_track(bytes) => Ok(VideoContainer::Mp4),
        "webm" if webm_has_video_track(bytes) => Ok(VideoContainer::Webm),
        "mp4" | "webm" => Err(DltError::invalid("video extension and container signature do not match")),
        _ => Err(DltError::unsupported("only local MP4 and WebM videos can produce automatic posters")),
    }
}

fn network_path(path: &Path) -> bool {
    let text = path.to_string_lossy();
    text.starts_with(r"\\") || text.starts_with("//")
}

/// The WebView2 user-data folder that poster work uses and its cleanup removes from: absolute, local, and canonical
/// (links resolved). `create` makes the folder first; without it a missing folder is an error.
fn poster_user_data_root(create: bool) -> DltResult<PathBuf> {
    let configured = clean_windows_path(user_data_folder()?);
    if !configured.is_absolute() {
        return Err(DltError::invalid("WebView2 user-data path for poster extraction must be absolute"));
    }
    if network_path(&configured) {
        return Err(DltError::unsupported("WebView2 user data for video poster extraction must be local"));
    }
    if create {
        std::fs::create_dir_all(&configured)?;
    }
    let root = clean_windows_path(std::fs::canonicalize(configured)?);
    if network_path(&root) {
        return Err(DltError::unsupported("WebView2 user data for video poster extraction must be local"));
    }
    Ok(root)
}

fn check_video_work(cancellation: Option<&VideoPosterCancellation>, deadline: Instant) -> DltResult<()> {
    if cancellation.is_some_and(VideoPosterCancellation::is_cancelled) {
        return Err(DltError::new(ErrorCode::Canceled, "video poster work was canceled"));
    }
    if Instant::now() >= deadline {
        return Err(DltError::new(ErrorCode::Unavailable, "video poster work exceeded its timeout"));
    }
    Ok(())
}

fn read_video_source(
    path: &Path,
    cancellation: Option<&VideoPosterCancellation>,
    deadline: Instant,
) -> DltResult<(VideoContainer, Vec<u8>, VideoSourceInfo)> {
    check_video_work(cancellation, deadline)?;
    let source_path = clean_windows_path(std::fs::canonicalize(path)?);
    check_video_work(cancellation, deadline)?;
    if network_path(&source_path) {
        return Err(DltError::unsupported("video poster extraction accepts local files only"));
    }
    let mut source = File::open(&source_path)?;
    let metadata = source.metadata()?;
    if !metadata.is_file() { return Err(DltError::invalid("video poster source must be a regular file")); }
    if metadata.len() == 0 || metadata.len() > MAX_VIDEO_INPUT_BYTES {
        return Err(DltError::invalid(format!("video source must be between 1 byte and {MAX_VIDEO_INPUT_BYTES} bytes")));
    }
    let capacity = usize::try_from(metadata.len()).map_err(|_| DltError::invalid("video source is too large for this process"))?;
    let mut bytes = Vec::with_capacity(capacity);
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        check_video_work(cancellation, deadline)?;
        let count = source.read(&mut buffer)?;
        if count == 0 { break; }
        if bytes.len().checked_add(count).is_none_or(|length| length as u64 > MAX_VIDEO_INPUT_BYTES) {
            return Err(DltError::invalid("video source exceeded the input size limit while reading"));
        }
        bytes.extend_from_slice(&buffer[..count]);
        hasher.update(&buffer[..count]);
    }
    check_video_work(cancellation, deadline)?;
    if bytes.len() as u64 != metadata.len() {
        return Err(DltError::new(ErrorCode::SourceChanged, "video source changed while it was being inspected"));
    }
    let container = video_container(&source_path, &bytes)?;
    let info = VideoSourceInfo { mime: container.mime().to_owned(), bytes: bytes.len() as u64, sha256: format!("{:x}", hasher.finalize()) };
    check_video_work(cancellation, deadline)?;
    Ok((container, bytes, info))
}

#[cfg(windows)]
fn write_video_copy(
    path: &Path,
    bytes: &[u8],
    cancellation: &VideoPosterCancellation,
    deadline: Instant,
) -> DltResult<()> {
    let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(path)?;
    for chunk in bytes.chunks(64 * 1024) {
        check_video_work(Some(cancellation), deadline)?;
        file.write_all(chunk)?;
    }
    file.flush()?;
    check_video_work(Some(cancellation), deadline)
}

#[cfg(windows)]
struct PosterTempDirs(Vec<PathBuf>);

#[cfg(windows)]
impl Drop for PosterTempDirs {
    fn drop(&mut self) {
        for path in &self.0 { let _ = std::fs::remove_dir_all(path); }
    }
}

#[cfg(windows)]
fn extract_video_poster_windows(
    path: &Path,
    expected_sha256: &str,
    deadline: Instant,
    cancellation: &VideoPosterCancellation,
) -> DltResult<VideoPoster> {
    check_video_work(Some(cancellation), deadline)?;
    let (container, bytes, source_info) = read_video_source(path, Some(cancellation), deadline)?;
    if !source_info.sha256.eq_ignore_ascii_case(expected_sha256) {
        return Err(DltError::new(ErrorCode::IntegrityError, "video source no longer matches the imported asset"));
    }

    check_video_work(Some(cancellation), deadline)?;
    let private_root = poster_user_data_root(true)?;
    // The process tag lets a later start remove these folders if this process is killed before `_cleanup` runs.
    let run_id = format!("{}{}", process_tag(), new_id());
    let scratch = private_root.join("video-poster").join("work").join(&run_id);
    let profile = private_root.join("video-poster").join("profiles").join(&run_id);
    let _cleanup = PosterTempDirs(vec![scratch.clone(), profile.clone()]);
    check_video_work(Some(cancellation), deadline)?;
    std::fs::create_dir_all(&scratch)?;
    std::fs::create_dir_all(&profile)?;
    let file_name = container.file_name();
    write_video_copy(&scratch.join(file_name), &bytes, cancellation, deadline)?;
    let media_timeout = deadline.saturating_duration_since(Instant::now());
    check_video_work(Some(cancellation), deadline)?;
    std::fs::write(scratch.join("index.html"), video_poster_html(file_name, media_timeout))?;

    let runtime = resolve_runtime()?;
    let target = preview_target(&scratch.join("index.html"))?;
    let remaining = deadline.saturating_duration_since(Instant::now());
    check_video_work(Some(cancellation), deadline)?;
    let host_timeout = remaining.saturating_add(Duration::from_secs(30));
    let mut options = host_options(&target, None, true, true, host_timeout, &runtime, &profile);
    options.total_timeout = Some(host_timeout);
    options.cancellation = Some(Arc::clone(&cancellation.0));
    let report = host::run_session(options).map_err(|error| {
        if error.message.contains("timed out") || error.message.contains("did not post") {
            DltError::new(ErrorCode::Unavailable, "WebView2 timed out while decoding the video poster")
        } else {
            error
        }
    })?;
    check_video_work(Some(cancellation), deadline)?;
    if !report.navigation_success || report.host_visible || report.visible_child_windows != 0 {
        return Err(DltError::new(ErrorCode::Internal, "video poster WebView2 session did not remain hidden"));
    }
    if report.blocked_requests != 0 || report.blocked_navigations != 0 || report.new_windows_blocked != 0 {
        return Err(DltError::unsupported("video poster page attempted a blocked network request or navigation"));
    }
    if !report.messages.iter().any(|message| message == READY_MESSAGE)
        || !report.messages.iter().any(|message| message == VIDEO_POSTER_READY_MESSAGE)
    {
        return Err(DltError::new(ErrorCode::Unavailable, "video poster page did not finish its decode session"));
    }
    if let Some(error) = report.messages.iter().find_map(|message| message.strip_prefix(VIDEO_ERROR_MESSAGE_PREFIX)) {
        return Err(DltError::unsupported(format!("WebView2 could not decode this video poster: {error}")));
    }
    let mut posters = report.messages.iter().filter(|message| message.starts_with(VIDEO_POSTER_MESSAGE_PREFIX));
    let Some(message) = posters.next() else {
        return Err(DltError::new(ErrorCode::IntegrityError, "WebView2 returned no video poster"));
    };
    if posters.next().is_some() {
        return Err(DltError::new(ErrorCode::IntegrityError, "WebView2 returned multiple video posters for one source"));
    }
    parse_video_poster_message(message, file_name)
}

/// Asks the current embedded or visible session to end. Smoke sessions end on their own.
pub fn request_stop() {
    #[cfg(windows)]
    host::request_stop();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_names_are_percent_encoded_and_only_html_is_accepted() {
        assert_eq!(percent_encode_segment("a b.html"), "a%20b.html");
        assert_eq!(percent_encode_segment("한.html"), "%ED%95%9C.html");
    }

    #[test]
    fn verbatim_prefix_is_removed_only_for_local_paths() {
        assert_eq!(clean_windows_path(PathBuf::from(r"\\?\C:\x")), PathBuf::from(r"C:\x"));
        assert_eq!(clean_windows_path(PathBuf::from(r"\\?\UNC\srv\share")), PathBuf::from(r"\\?\UNC\srv\share"));
    }
}

#[cfg(test)]
mod video_poster_tests {
    use super::*;

    fn png_header(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = vec![0u8; 24];
        bytes[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
        bytes[8..12].copy_from_slice(&13u32.to_be_bytes());
        bytes[12..16].copy_from_slice(b"IHDR");
        bytes[16..20].copy_from_slice(&width.to_be_bytes());
        bytes[20..24].copy_from_slice(&height.to_be_bytes());
        bytes
    }

    #[test]
    fn video_container_requires_matching_local_extension_and_signature() {
        let mp4 = fake_mp4(*b"isom");
        assert_eq!(video_container(Path::new("clip.MP4"), &mp4).unwrap(), VideoContainer::Mp4);
        assert!(video_container(Path::new("clip.mp4"), &fake_mp4(*b"M4A ")).is_err(), "audio-only ISO-BMFF brands are refused");
        assert!(video_container(Path::new("clip.mp4"), &fake_mp4(*b"qt  ")).is_err(), "QuickTime is not accepted as MP4");
        assert!(video_container(Path::new("clip.mp4"), &mp4[..12]).is_err(), "truncated ftyp boxes are refused");
        assert!(video_container(Path::new("clip.mp4"), &mp4[..16]).is_err(), "an ftyp box alone has no video track");
        assert!(video_container(Path::new("clip.mp4"), &fake_mp4_with_handler(*b"isom", *b"soun")).is_err(), "an audio-only MP4 is refused");
        let mut bad_size = mp4.clone();
        bad_size[..4].copy_from_slice(&u32::MAX.to_be_bytes());
        assert!(video_container(Path::new("clip.mp4"), &bad_size).is_err(), "out-of-file box sizes are refused");
        assert_eq!(video_container(Path::new("clip.webm"), &fake_webm_with_video_track()).unwrap(), VideoContainer::Webm);
        assert!(video_container(Path::new("clip.webm"), &fake_webm(b"webm")).is_err(), "a WebM header without a video track is refused");
        assert!(video_container(Path::new("clip.webm"), &fake_webm(b"matroska")).is_err(), "EBML Matroska DocType is not WebM");
        assert!(video_container(Path::new("clip.webm"), &[0x1a, 0x45, 0xdf, 0xa3]).is_err(), "an EBML magic prefix alone is insufficient");
        let mut bad_webm = fake_webm(b"webm");
        bad_webm[4] = 0xff;
        assert!(video_container(Path::new("clip.webm"), &bad_webm).is_err(), "truncated or oversized EBML headers are refused");
        assert!(video_container(Path::new("clip.mkv"), &fake_webm(b"webm")).is_err());
    }

    fn fake_mp4(brand: [u8; 4]) -> Vec<u8> {
        fake_mp4_with_handler(brand, *b"vide")
    }

    fn mp4_box(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        [&u32::try_from(8 + payload.len()).unwrap().to_be_bytes()[..], kind, payload].concat()
    }

    /// A 16-byte ftyp box, then a moov box with one track whose handler type is `handler`.
    fn fake_mp4_with_handler(brand: [u8; 4], handler: [u8; 4]) -> Vec<u8> {
        let file_type = mp4_box(b"ftyp", &[&brand[..], &[0; 4]].concat());
        // FullBox header and pre-defined, handler type, reserved, empty name.
        let handler = mp4_box(b"hdlr", &[&[0u8; 8][..], &handler, &[0; 13]].concat());
        let movie = mp4_box(b"moov", &mp4_box(b"trak", &mp4_box(b"mdia", &handler)));
        [file_type, movie].concat()
    }

    fn fake_webm(doc_type: &[u8]) -> Vec<u8> {
        let mut bytes = vec![0x1a, 0x45, 0xdf, 0xa3];
        bytes.push((0x80 | (3 + doc_type.len())) as u8);
        bytes.extend_from_slice(&[0x42, 0x82, (0x80 | doc_type.len()) as u8]);
        bytes.extend_from_slice(doc_type);
        bytes
    }

    fn ebml(id: &[u8], data: &[u8]) -> Vec<u8> {
        let mut bytes = id.to_vec();
        bytes.push((0x80 | data.len()) as u8);
        bytes.extend_from_slice(data);
        bytes
    }

    /// EBML header with DocType "webm", then a Segment whose only track is a video track (TrackType 1).
    fn fake_webm_with_video_track() -> Vec<u8> {
        let header = ebml(&[0x1a, 0x45, 0xdf, 0xa3], &ebml(&[0x42, 0x82], b"webm"));
        let track_entry = ebml(&[0xae], &ebml(&[0x83], &[1]));
        let segment = ebml(&[0x18, 0x53, 0x80, 0x67], &ebml(&[0x16, 0x54, 0xae, 0x6b], &track_entry));
        [header, segment].concat()
    }

    #[test]
    fn webm_input_set_acceptance_through_inspect_video_source() {
        let root = std::env::temp_dir().join(format!("dlt-49e-webm-input-set-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("clip.webm");
        let normal = fake_webm_with_video_track();
        let truncated = normal[..normal.len() - 1].to_vec();
        let header_cut = fake_webm(b"webm")[..6].to_vec();
        let mut table = Vec::new();
        for (name, bytes) in [("W1 normal", normal), ("W2 truncated", truncated), ("W3 header only", fake_webm(b"webm")), ("W4 header cut", header_cut)] {
            std::fs::write(&path, &bytes).unwrap();
            table.push((name, inspect_video_source(&path).is_ok()));
        }
        std::fs::remove_dir_all(root).unwrap();
        println!("49e web-preview inspect_video_source accepts: {table:?}");
        assert_eq!(table, [("W1 normal", true), ("W2 truncated", false), ("W3 header only", false), ("W4 header cut", false)]);
    }

    #[test]
    fn inspect_video_source_returns_preflight_hash_and_mime() {
        let root = std::env::temp_dir().join(format!("dlt-video-poster-preflight-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("sample.mp4");
        let bytes = fake_mp4(*b"isom");
        std::fs::write(&path, &bytes).unwrap();
        let info = inspect_video_source(&path).expect("local MP4 preflight");
        assert_eq!(info.mime, "video/mp4");
        assert_eq!(info.bytes, bytes.len() as u64);
        assert_eq!(info.sha256, designllm_contracts::sha256(&bytes));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn poster_message_checks_png_type_dimensions_and_caps() {
        let png = png_header(320, 180);
        let message = format!(
            "{VIDEO_POSTER_MESSAGE_PREFIX}video.mp4:320:180:data:image/png;base64,{}",
            STANDARD.encode(&png),
        );
        let poster = parse_video_poster_message(&message, "video.mp4").expect("valid first-frame PNG");
        assert_eq!((poster.width, poster.height), (320, 180));
        assert_eq!(poster.png, png);

        let too_large = png_header(5000, 5000);
        let message = format!(
            "{VIDEO_POSTER_MESSAGE_PREFIX}video.mp4:5000:5000:data:image/png;base64,{}",
            STANDARD.encode(too_large),
        );
        assert!(parse_video_poster_message(&message, "video.mp4").is_err());
    }

    #[test]
    fn poster_message_refuses_an_ihdr_chunk_of_the_wrong_length() {
        for length in [0u32, 12, 14] {
            let mut png = png_header(320, 180);
            png[8..12].copy_from_slice(&length.to_be_bytes());
            let message = format!(
                "{VIDEO_POSTER_MESSAGE_PREFIX}video.mp4:320:180:data:image/png;base64,{}",
                STANDARD.encode(&png),
            );
            assert!(parse_video_poster_message(&message, "video.mp4").is_err(), "IHDR length {length} must be refused");
        }
    }

    #[test]
    fn cancellation_token_is_cloneable_and_per_request() {
        let token = VideoPosterCancellation::default();
        let clone = token.clone();
        assert!(!token.is_cancelled());
        clone.cancel();
        assert!(token.is_cancelled());
        let error = extract_video_poster_cancellable(
            Path::new("missing.mp4"),
            &"a".repeat(64),
            DEFAULT_VIDEO_POSTER_TIMEOUT,
            &token,
        ).unwrap_err();
        assert!(matches!(error.code, ErrorCode::Canceled));
    }

    #[test]
    fn poster_api_rejects_invalid_timeout_and_expected_hash_before_file_access() {
        let path = Path::new("missing.mp4");
        let zero_timeout = extract_video_poster(path, &"a".repeat(64), Duration::ZERO).unwrap_err();
        assert!(matches!(zero_timeout.code, ErrorCode::InvalidArgument));
        let bad_hash = extract_video_poster(path, "not-a-sha", DEFAULT_VIDEO_POSTER_TIMEOUT).unwrap_err();
        assert!(matches!(bad_hash.code, ErrorCode::InvalidArgument));
    }

    #[test]
    fn poster_page_uses_local_source_and_bounded_decode() {
        let html = video_poster_html("video.webm", Duration::from_secs(7));
        assert!(html.contains("video.webm"));
        assert!(!html.contains("__MAX_PIXELS__"));
        assert!(html.contains("loadeddata"));
        assert!(html.contains("toBlob"));
        assert!(html.contains("window.chrome.webview.postMessage"));
        assert!(!html.contains("https://"));
        assert!(html.contains("7000"));
    }
}
