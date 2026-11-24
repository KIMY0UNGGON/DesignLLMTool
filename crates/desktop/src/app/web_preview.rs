mod lifecycle;
#[cfg(windows)]
mod placement;
pub(super) use lifecycle::PreviewState;

use super::*;
use std::io::{Read, Seek, SeekFrom};

#[derive(Deserialize)]
struct MaterializedPreview {
    artifact_id: String,
    revision: u64,
    entry_path: PathBuf,
    source_hash: String,
}

pub(super) struct PreviewRun {
    id: String,
    generation: u64,
    deadline: Instant,
    preview: MaterializedPreview,
    ready_frame: Vec<u8>,
    matched: usize,
}

#[cfg(windows)]
pub(super) struct FocusGuard {
    anchor: isize,
    matched: usize,
    ready: bool,
    deadline: Instant,
}

#[cfg(windows)]
const WINDOW_READY_FRAME: &[u8] = b"\ndesignllm.preview.window.ready\n";

fn preview_timeout() -> DltError {
    DltError::new(ErrorCode::Unavailable, "제한 시간 안에 웹 미리보기의 READY 응답을 받지 못했습니다")
}

fn closed_before_ready(stderr: &str, exit: std::process::ExitStatus) -> DltError {
    if exit.success() {
        return DltError::new(ErrorCode::Canceled, "READY 응답 전에 웹 미리보기 창이 닫혔습니다");
    }
    let last = stderr.lines().rev().map(str::trim).find(|line| !line.is_empty());
    if let Some(line) = last {
        return serde_json::from_str(line).unwrap_or_else(|_| DltError::new(ErrorCode::Unavailable, line));
    }
    DltError::new(ErrorCode::Unavailable, format!("READY 응답 전에 웹 미리보기 창이 닫혔습니다 ({exit})"))
}

fn child_reported_ready(child: &mut Child, run: &mut PreviewRun) -> DltResult<bool> {
    child_reported_frame(child, &run.ready_frame, &mut run.matched)
}

fn child_reported_frame(child: &mut Child, frame: &[u8], matched: &mut usize) -> DltResult<bool> {
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use windows::Win32::{Foundation::{ERROR_BROKEN_PIPE, HANDLE}, System::Pipes::PeekNamedPipe};
        let stdout = child.stdout.as_mut().ok_or_else(|| DltError::new(ErrorCode::Unavailable, "미리보기 READY 응답 채널이 없습니다"))?;
        let mut available = 0;
        // SAFETY: stdout owns the pipe and available is a valid out parameter.
        let peek = unsafe {
            PeekNamedPipe(HANDLE(stdout.as_raw_handle()), None, 0, None, Some(&mut available), None)
        };
        match peek {
            Ok(()) if available == 0 => Ok(false),
            Ok(()) => {
                let mut bytes = [0; 8192];
                let limit = bytes.len().min(available as usize);
                let count = stdout.read(&mut bytes[..limit])?;
                // The frame starts and ends with LF and has no internal LF.
                for &byte in &bytes[..count] {
                    if byte == frame[*matched] {
                        *matched += 1;
                        if *matched == frame.len() { return Ok(true); }
                    } else {
                        *matched = usize::from(byte == b'\n');
                    }
                }
                Ok(false)
            }
            // The child may have exited since try_wait. The next poll collects its exit status and stderr.
            Err(error) if error.code() == ERROR_BROKEN_PIPE.to_hresult() => Ok(false),
            Err(error) => Err(DltError::new(ErrorCode::Io, format!("웹 미리보기 READY 응답을 읽지 못했습니다: {error}"))),
        }
    }
    #[cfg(not(windows))]
    {
        let _ = (child, frame, matched);
        Err(DltError::unsupported("웹 미리보기는 Windows에서만 열 수 있습니다"))
    }
}

impl DesktopApp {
    pub(super) fn read_preview_asset(&mut self, asset: designllm_contracts::AssetRef, snapshot: RenderSnapshot, page_index: u32) {
        let artifact_id = snapshot.artifact_id.clone();
        if let Err(error) = self.send_asset_read(asset, artifact_id, PendingAction::PreviewAsset { snapshot, page_index }) {
            self.set_error(user_error(&error));
        }
    }

    pub(super) fn start_materialized_web_preview(&mut self) {
        self.collect_exited_preview_before_request();
        self.cancel_materializing_previews();
        let ticket = self.send_web_materialization();
        self.track_explicit_materialization(ticket);
    }

    /// `view.play` (canvas, D09): the canvas exported as HTML to a scratch file, shown in the WebView2 preview window.
    /// The export first asks what it would lose and then accepts exactly those losses: playing changes nothing.
    pub(crate) fn play_canvas(&mut self) -> Result<(), String> {
        let active = self.active.as_ref().ok_or("작업물이 열려 있지 않습니다")?;
        if active.kind != ArtifactKind::Canvas {
            return Err("캔버스만 재생할 수 있습니다".into());
        }
        let path = std::env::temp_dir().join(format!("designllm-play-{}.html", active.id));
        let request = ExportFileRequest {
            request_id: new_id(),
            dry_run: Some(true),
            options: ExportRequest::new(active.id.clone(), active.revision, "html", Editability::Objects),
            target_path: path.to_string_lossy().into_owned(),
            overwrite: true,
            expected_target_hash: None,
        };
        let params = serde_json::to_value(&request).map_err(|error| error.to_string())?;
        self.call("artifact.export", params, PendingAction::PlayDryRun { request });
        Ok(())
    }

    pub(super) fn open_materialized_web_preview(&mut self, payload: RpcPayload, run: Option<&ui_channel::RunContext>) -> DltResult<()> {
        let RpcPayload::Json(value) = payload else {
            return Err(DltError::new(ErrorCode::IntegrityError, "웹 미리보기 파일 정보를 받지 못했습니다"));
        };
        let preview: MaterializedPreview = serde_json::from_value(value)
            .map_err(|_| DltError::new(ErrorCode::IntegrityError, "웹 미리보기 경로나 작업물 정보를 받지 못했습니다"))?;
        if let Some(run) = run {
            if Instant::now() >= run.deadline {
                return Err(preview_timeout());
            }
        }
        if !self.preview_target_is_current(&preview) {
            return if run.is_some() { Err(DltError::new(ErrorCode::RevisionConflict, "미리보기를 준비하는 동안 작업물이나 판 번호가 바뀌었습니다")) } else { Ok(()) };
        }
        if self.refresh_child_has_closed(preview.revision) { return Ok(()); }
        self.begin_preview_launch();
        let mut launched = self.launch_web_preview(&preview.entry_path, run.map(|r| r.run_id.as_str()));
        self.end_preview_launch(launched.is_ok());
        if let Err(error) = &mut launched {
            error.message.push_str(" 미리보기를 다시 열어 주세요");
        }
        launched?;
        if let Some(run) = run {
            let ready_frame = format!("\n{} {}\n", designllm_web_preview::READY_MESSAGE, run.run_id).into_bytes();
            self.web_preview_run = Some(PreviewRun { id: run.run_id.clone(), generation: self.web_preview_state.child_generation, deadline: run.deadline, preview, ready_frame, matched: 0 });
        }
        Ok(())
    }

    fn preview_target_is_current(&self, preview: &MaterializedPreview) -> bool {
        self.screen_is_editor() && self.active.as_ref().is_some_and(|a| a.id == preview.artifact_id && a.revision == preview.revision)
    }

    fn preview_command(&mut self, entry_path: &Path) -> DltResult<Option<Command>> {
        #[cfg(test)]
        if let Some(command) = self.fake_preview_command.take() {
            return Ok(Some(command));
        }
        if self.smoke_output.is_some() || crate::dialogs::preview_refused() {
            return Ok(None);
        }
        let parent_hwnd = self.parent_hwnd.ok_or_else(|| DltError::new(ErrorCode::Unavailable, "웹 미리보기 창을 현재 작업공간에 연결할 수 없습니다"))?;
        let mut command = Command::new(std::env::current_exe()?);
        command
            .arg("--preview-web")
            .arg(entry_path)
            .arg("--parent-hwnd")
            .arg(parent_hwnd.to_string());
        Ok(Some(command))
    }

    pub(super) fn launch_web_preview(&mut self, entry_path: &Path, ready_id: Option<&str>) -> DltResult<()> {
        let Some(mut command) = self.preview_command(entry_path)? else {
            return if ready_id.is_some() {
                Err(DltError::new(ErrorCode::Unavailable, "화면이 없는 실행에서는 웹 미리보기 창을 열 수 없습니다"))
            } else {
                Ok(())
            };
        };
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000);
        }
        #[cfg(windows)]
        let refresh = self.preview_is_refresh();
        #[cfg(windows)]
        let focus_anchor = if refresh && self.parent_hwnd.is_some() {
            placement::foreground_to_preserve(self.web_preview_child.as_ref().map(Child::id))
        } else { None };
        self.poll_web_preview_child();
        #[cfg(windows)]
        placement::hand_over(&mut command, self.web_preview_child.as_ref().and_then(|child| placement::window_placement(child.id(), self.parent_hwnd)), refresh);
        self.stop_web_preview_child()?;
        if let Some(old) = self.web_preview_run.take() {
            self.complete_ui_run(&old.id, None, Some(DltError::new(ErrorCode::Canceled, "다른 미리보기를 요청해 이전 창의 READY 대기를 취소했습니다")));
        }
        if let Some(active) = &self.active {
            command.env("DESIGNLLM_PREVIEW_TITLE", format!("{} - 웹 미리보기", active.title));
        }
        command.env_remove(designllm_web_preview::REPORT_READY_ENV);
        command.env_remove("DESIGNLLM_PREVIEW_REPORT_WINDOW");
        #[cfg(windows)]
        if focus_anchor.is_some() && ready_id.is_none() {
            command.env("DESIGNLLM_PREVIEW_REPORT_WINDOW", "1").stdout(std::process::Stdio::piped());
        }
        if let Some(id) = ready_id {
            command.env(designllm_web_preview::REPORT_READY_ENV, id).stdout(std::process::Stdio::piped());
        }
        let path = preview_stderr_path();
        match std::fs::File::create(&path) {
            Ok(file) => command.stderr(file),
            Err(error) if ready_id.is_some() => return Err(error.into()),
            Err(_) => command.stderr(std::process::Stdio::null()),
        };
        self.web_preview_child = Some(command.spawn().map_err(|error| {
            let _ = std::fs::remove_file(path);
            DltError::new(ErrorCode::Unavailable, format!("웹 미리보기를 열지 못했습니다: {error}"))
        })?);
        let deferred_notice = self.web_preview_state.deferred_exit_notice.take();
        self.track_web_preview();
        #[cfg(windows)]
        if ready_id.is_none() {
            self.web_preview_state.focus_guard = focus_anchor.map(|anchor| FocusGuard { anchor, matched: 0, ready: false, deadline: Instant::now() + Duration::from_secs(2) });
        }
        if let Some(message) = deferred_notice { self.set_error(message); }
        Ok(())
    }

    pub(super) fn stop_web_preview_child(&mut self) -> std::io::Result<()> {
        if let Some(child) = self.web_preview_child.as_mut() {
            let exited = match child.try_wait()? {
                Some(exit) => Some(exit),
                None => match child.kill() {
                    Ok(()) => { child.wait()?; None }
                    Err(error) => match child.try_wait()? {
                        Some(exit) => Some(exit),
                        None => return Err(error),
                    },
                },
            };
            self.web_preview_child = None;
            #[cfg(windows)]
            { self.web_preview_state.focus_guard = None; }
            let path = preview_stderr_path();
            if exited.is_some_and(|exit| !exit.success()) {
                self.report_preview_failure(&read_preview_stderr(&path));
            }
            let _ = std::fs::remove_file(path);
        }
        Ok(())
    }

    fn report_preview_failure(&mut self, stderr: &str) {
        if let Some(message) = super::messages::preview_exit_message(stderr, false) {
            if self.web_preview_state.generation != self.web_preview_state.child_generation {
                self.web_preview_state.deferred_exit_notice = Some(message.to_owned());
            }
            self.set_error(message.to_owned());
        }
    }

    /// The materialization reply can arrive after the caller's deadline. Forget its ticket so it cannot launch a window.
    fn expire_materializing_previews(&mut self) {
        let now = Instant::now();
        let expired: Vec<_> = self.pending.extract_if(|_, request| {
            matches!(request.action, PendingAction::MaterializeWeb) && request.ui_run.as_ref().is_some_and(|run| now >= run.deadline)
        }).filter_map(|(_, request)| request.ui_run).collect();
        for run in expired {
            self.complete_ui_run(&run.run_id, None, Some(preview_timeout()));
        }
    }

    pub(super) fn poll_web_preview_child(&mut self) {
        self.expire_materializing_previews();
        self.update_web_preview();
        if self.host_lost() {
            self.cancel_materializing_previews();
            self.web_preview_state = Default::default();
            self.web_preview_run = None;
            if let Err(error) = self.stop_web_preview_child() {
                self.set_error(format!("웹 미리보기 창을 닫지 못했습니다 ({error})"));
            }
            return;
        }
        #[cfg(windows)]
        if let (Some(guard), Some(child)) = (&mut self.web_preview_state.focus_guard, &mut self.web_preview_child) {
            let mut failed = false;
            if !guard.ready && Instant::now() < guard.deadline {
                match child_reported_frame(child, WINDOW_READY_FRAME, &mut guard.matched) {
                    Ok(ready) => guard.ready = ready,
                    Err(_) => failed = true,
                }
            }
            if failed || placement::preserve_foreground(guard.anchor, child.id(), guard.ready, Instant::now() >= guard.deadline) {
                self.web_preview_state.focus_guard = None;
            }
        }
        self.poll_preview_generation(self.web_preview_state.child_generation);
    }

    pub(super) fn preview_focus_pending(&self) -> bool {
        #[cfg(windows)] { self.web_preview_state.focus_guard.is_some() }
        #[cfg(not(windows))] { false }
    }

    fn poll_preview_generation(&mut self, generation: u64) {
        if generation != self.web_preview_state.child_generation { return; }
        let current = self.web_preview_run.as_ref().is_none_or(|run| run.generation == generation && self.preview_target_is_current(&run.preview));
        let Some(child) = self.web_preview_child.as_mut() else { return };
        let pid = child.id();
        let outcome = match child.try_wait() {
            Ok(Some(exit)) => {
                self.web_preview_child = None;
                #[cfg(windows)]
                { self.web_preview_state.focus_guard = None; }
                let owns_request = generation == self.web_preview_state.generation;
                if owns_request {
                    self.cancel_materializing_previews();
                    self.web_preview_state = Default::default();
                } else {
                    self.cancel_preview_refresh();
                }
                let path = preview_stderr_path();
                let stderr = read_preview_stderr(&path);
                let _ = std::fs::remove_file(path);
                if !exit.success() {
                    self.report_preview_failure(&stderr);
                }
                Err(closed_before_ready(&stderr, exit))
            }
            Ok(None) => {
                let Some(run) = &mut self.web_preview_run else { return };
                if run.generation != generation { return; }
                if Instant::now() >= run.deadline {
                    Err(preview_timeout())
                } else if !current {
                    Err(DltError::new(ErrorCode::RevisionConflict, "READY 응답을 기다리는 동안 작업물이나 판 번호가 바뀌었습니다"))
                } else {
                    match child_reported_ready(child, run) {
                        Ok(false) => return,
                        Ok(true) => Ok(()),
                        Err(error) => Err(error),
                    }
                }
            }
            Err(error) => Err(DltError::from(error)),
        };
        if self.web_preview_run.as_ref().is_none_or(|run| run.generation != generation) { return; }
        let run = self.web_preview_run.take().unwrap();
        if let Err(mut error) = outcome {
            let keep = error.code == ErrorCode::RevisionConflict && self.screen_is_editor()
                && self.active.as_ref().is_some_and(|active| active.id == run.preview.artifact_id);
            if !keep {
                if let Err(stop) = self.stop_web_preview_child() {
                    error.message.push_str(&format!(" (미리보기 종료 실패: {stop})"));
                }
            }
            self.complete_ui_run(&run.id, None, Some(error));
        } else {
            let preview = run.preview;
            self.complete_ui_run(&run.id, Some(json!({
                "preview_pid": pid, "artifact_id": preview.artifact_id, "revision": preview.revision,
                "entry_path": preview.entry_path, "source_hash": preview.source_hash,
            })), None);
        }
    }
}

/// One file per desktop process. A launch truncates it after the previous child has stopped.
fn preview_stderr_path() -> PathBuf {
    std::env::temp_dir().join(format!("designllm-preview-{}.err", std::process::id()))
}

/// How much of the end of the stderr file is read for the reason. The child writes its reason as one error line last, a few
/// hundred bytes; 16 KiB leaves room for its diagnostics. Set without a measurement.
const PREVIEW_STDERR_TAIL_BYTES: u64 = 16 * 1024;

/// Ignore excessively large logs. The child's final diagnostic usually fits in the bounded tail.
const PREVIEW_STDERR_MAX_BYTES: u64 = 1024 * 1024;

/// The end of the preview's stderr file, at most `PREVIEW_STDERR_TAIL_BYTES`. Empty when the file is missing, unreadable or larger
/// than `PREVIEW_STDERR_MAX_BYTES`.
fn read_preview_stderr(path: &Path) -> String {
    let Ok(mut file) = std::fs::File::open(path) else {
        return String::new();
    };
    let Ok(length) = file.metadata().map(|meta| meta.len()) else {
        return String::new();
    };
    if length > PREVIEW_STDERR_MAX_BYTES {
        return String::new();
    }
    let mut tail = Vec::new();
    let read = file
        .seek(SeekFrom::Start(length.saturating_sub(PREVIEW_STDERR_TAIL_BYTES)))
        .and_then(|_| file.take(PREVIEW_STDERR_TAIL_BYTES).read_to_end(&mut tail));
    match read {
        Ok(_) => String::from_utf8_lossy(&tail).into_owned(),
        Err(_) => String::new(),
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use crate::app::test_support::{Harness, Host, Window};
    use std::time::{Duration, Instant};

    /// The stderr file that `preview_stderr_path` names is one per process, and the tests share that process: the tests that use it
    /// take this lock, so that one test does not read or remove the file of another.
    static SHARED_STDERR_FILE: std::sync::Mutex<()> = std::sync::Mutex::new(());

    pub(super) fn shared_stderr_file() -> std::sync::MutexGuard<'static, ()> {
        SHARED_STDERR_FILE.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub(super) fn fake_child() -> Command {
        let script = r#"
            while ($null -ne ($line = [Console]::ReadLine())) {
                if ($line -eq 'exit') { exit 1 }
                if ($line -eq 'close') { exit 0 }
                if ($line -eq 'end-frame') {
                    [Console]::Out.Write(" $env:DESIGNLLM_PREVIEW_REPORT_READY" + [char]10)
                    [Console]::Out.Flush()
                } elseif ($line.StartsWith('out ') -and $env:DESIGNLLM_PREVIEW_REPORT_READY) {
                    [Console]::Out.Write($line.Substring(4))
                    [Console]::Out.Flush()
                } else { [Console]::Error.WriteLine($line) }
            }"#;
        let mut command = Command::new("powershell.exe");
        command.args(["-NoProfile", "-NonInteractive", "-Command", script]).stdin(std::process::Stdio::piped());
        command
    }

    #[test]
    fn focus_window_ready_frame_is_detected_across_pipe_reads() {
        use std::io::Write;
        use std::os::windows::process::CommandExt;
        let mut command = Command::new("powershell.exe");
        command.args(["-NoProfile", "-NonInteractive", "-Command", r#"
            $null = [Console]::ReadLine(); [Console]::Out.Write("`ndesignllm.preview."); [Console]::Out.Flush();
            $null = [Console]::ReadLine(); [Console]::Out.Write("window.ready`n"); [Console]::Out.Flush();
            $null = [Console]::ReadLine()
        "#]).stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).creation_flags(0x08000000);
        let mut child = command.spawn().unwrap();
        writeln!(child.stdin.as_mut().unwrap(), "first").unwrap();
        let mut matched = 0;
        let deadline = Instant::now() + Duration::from_secs(5);
        while matched < b"\ndesignllm.preview.".len() {
            assert!(Instant::now() < deadline);
            assert!(!child_reported_frame(&mut child, WINDOW_READY_FRAME, &mut matched).unwrap());
            std::thread::sleep(Duration::from_millis(10));
        }
        writeln!(child.stdin.as_mut().unwrap(), "second").unwrap();
        while !child_reported_frame(&mut child, WINDOW_READY_FRAME, &mut matched).unwrap() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = child.kill();
        let _ = child.wait();
    }

    fn tell_child(window: &mut Window, script: &str) {
        use std::io::Write;
        let child = window.app.web_preview_child.as_mut().unwrap();
        writeln!(child.stdin.as_mut().unwrap(), "{script}\r").unwrap();
    }

    fn preview_outcome(window: &mut Window, host: &mut Host, run: &Value) -> Value {
        let until = Instant::now() + Duration::from_secs(5);
        while window.app.web_preview_run.is_some() && Instant::now() < until {
            window.app.poll_web_preview_child();
            std::thread::sleep(Duration::from_millis(10));
        }
        window.settle(host);
        host.call("ui.command.get", json!({ "run_id": run["run_id"] })).unwrap()
    }

    #[test]
    fn web_preview_run_waits_for_its_childs_ready_and_returns_the_displayed_revision() {
        use designllm_web_preview::READY_MESSAGE;
        let _shared = shared_stderr_file();
        let mut host = Host::start();
        let web = host.create("web_code", "웹");
        let mut window = Window::open(&mut host, &web);
        window.app.web_runtime_unavailable = None;
        window.app.fake_preview_command = Some(fake_child());
        window.step(&mut host, "web.preview", json!({})).unwrap();
        let clicked = window.calls.iter().find(|(m, _, _)| m == "web.materialize").unwrap().2.clone();
        assert!(window.app.web_preview_child.is_some(), "the GUI starts the preview");

        window.app.fake_preview_command = Some(fake_child());
        let run = window.agent_run_granted(&mut host, "web.preview", json!({}), "read", &[]);
        assert_eq!(run["status"], "delivered", "{run}");
        tell_child(&mut window, &format!("out startup log {READY_MESSAGE}"));
        tell_child(&mut window, "end-frame");
        tell_child(&mut window, "out designllm.");
        tell_child(&mut window, &format!("{READY_MESSAGE} old-run"));
        let until = Instant::now() + Duration::from_secs(5);
        while !read_preview_stderr(&preview_stderr_path()).contains("old-run") {
            assert!(Instant::now() < until, "the fake child did not send the unrelated READY");
            std::thread::sleep(Duration::from_millis(10));
        }
        window.app.poll_web_preview_child();
        window.settle(&mut host);
        assert_eq!(host.call("ui.command.get", json!({ "run_id": run["run_id"] })).unwrap()["status"], "delivered");
        let pid = window.app.web_preview_child.as_ref().unwrap().id();
        tell_child(&mut window, "out preview.ready");
        tell_child(&mut window, "end-frame");
        let outcome = preview_outcome(&mut window, &mut host, &run);
        assert_eq!(outcome["status"], "completed", "{outcome}");
        assert_eq!(outcome["result"]["preview_pid"], pid);
        for field in ["artifact_id", "revision", "entry_path", "source_hash"] {
            assert_eq!(outcome["result"][field], clicked[field], "{field}");
        }
        assert!(window.app.web_preview_child.as_mut().unwrap().try_wait().unwrap().is_none(), "the ready preview stays open");
        let lost = DltError::new(ErrorCode::HostNotRunning, "the pipe closed");
        window.app.note_host_reply(u64::MAX, Some(&lost));
        window.app.poll_web_preview_child();
        assert_eq!(window.app.web_preview_child.as_ref().unwrap().id(), pid, "one lost reply keeps the preview");
        let (&ticket, _) = window.app.pending.iter().find(|(_, p)| matches!(p.action, PendingAction::HostProbe))
            .expect("the window checks the first lost reply");
        window.app.note_host_reply(ticket, Some(&lost));
        window.app.poll_web_preview_child();
        assert!(window.app.web_preview_child.is_none(), "the ready preview must uncover the reconnect buttons");
    }

    #[test]
    fn web_preview_runs_fail_on_child_error_replacement_or_timeout() {
        let _shared = shared_stderr_file();
        let mut host = Host::start();
        let web = host.create("web_code", "웹");
        let mut window = Window::open(&mut host, &web);
        window.app.web_runtime_unavailable = None;
        window.app.fake_preview_command = Some(fake_child());
        let run = window.agent_run_granted(&mut host, "web.preview", json!({}), "read", &[]);
        tell_child(&mut window, r#"{"code":"UNAVAILABLE","message":"preview navigation failed"}"#);
        tell_child(&mut window, "exit");
        let outcome = preview_outcome(&mut window, &mut host, &run);
        assert_eq!(outcome["status"], "failed", "{outcome}");
        assert_eq!(outcome["error"]["message"], "preview navigation failed");

        window.app.fake_preview_command = Some(fake_child());
        let old = window.agent_run_granted(&mut host, "web.preview", json!({}), "read", &[]);
        assert_eq!(old["status"], "delivered");
        window.app.fake_preview_command = Some(fake_child());
        let run = window.agent_run_granted(&mut host, "web.preview", json!({}), "read", &[]);
        assert_eq!(run["status"], "delivered");
        let replaced = host.call("ui.command.get", json!({ "run_id": old["run_id"] })).unwrap();
        assert_eq!(replaced["status"], "failed");
        assert_eq!(replaced["error"]["code"], "CANCELED");
        window.app.web_preview_run.as_mut().unwrap().deadline = Instant::now();
        let outcome = preview_outcome(&mut window, &mut host, &run);
        assert_eq!(outcome["status"], "failed", "{outcome}");
        assert_eq!(outcome["error"]["message"], preview_timeout().message);
        assert!(window.app.web_preview_child.is_none());
    }

    #[test]
    fn user_close_before_ready_is_canceled_without_notice_and_can_reopen() {
        let _shared = shared_stderr_file();
        let mut host = Host::start();
        let web = host.create("web_code", "웹");
        let mut window = Window::open(&mut host, &web);
        window.app.web_runtime_unavailable = None;
        window.app.fake_preview_command = Some(fake_child());
        let run = window.agent_run_granted(&mut host, "web.preview", json!({}), "read", &[]);
        tell_child(&mut window, "close");
        let outcome = preview_outcome(&mut window, &mut host, &run);
        assert_eq!(outcome["status"], "failed");
        assert_eq!(outcome["error"]["code"], "CANCELED");
        assert!(window.message().is_none(), "closing the preview is not an error notice");
        window.app.fake_preview_command = Some(fake_child());
        window.step(&mut host, "web.preview", json!({})).unwrap();
        assert!(window.app.web_preview_child.is_some());
    }

    /// Starts a child that writes `stderr` (one line, when given) to the preview's stderr file and exits with `code`, hands it to a
    /// window, and returns the sentence the window shows once it has taken the ended child down.
    fn end_preview(stderr: Option<&str>, code: u32) -> Option<String> {
        use std::os::windows::process::CommandExt;
        let mut harness = Harness::new();
        let script = match stderr {
            Some(line) => format!("/C echo {line} 1>&2 & exit {code}"),
            None => format!("/C exit {code}"),
        };
        let file = std::fs::File::create(preview_stderr_path()).expect("the preview's stderr file");
        let child = Command::new("cmd")
            .raw_arg(script)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::from(file))
            .spawn()
            .expect("the fake preview child starts");
        harness.app.web_preview_child = Some(child);
        let started = Instant::now();
        while harness.app.web_preview_child.is_some() && started.elapsed() < Duration::from_secs(10) {
            harness.app.poll_web_preview_child();
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(harness.app.web_preview_child.is_none(), "the window took the ended child down");
        harness.message().map(|(_, text)| text)
    }

    /// The preview process writes its reason as one JSON error line. A window whose process ends on its own shows that reason's
    /// sentence, or the generic one when there is no reason; a normal close shows nothing.
    #[test]
    fn a_preview_window_whose_process_ends_on_its_own_says_why() {
        let _shared = shared_stderr_file();
        let cases = [
            (designllm_web_preview::PREVIEW_UNRESPONSIVE, "미리보기 화면이 응답하지 않아 창을 닫았습니다. 다시 열어 주세요"),
            (designllm_web_preview::PREVIEW_MEMORY_LIMIT, "미리보기가 메모리 상한을 넘어 창을 닫았습니다. 다시 열어 주세요"),
            (designllm_web_preview::PREVIEW_PROCESS_EXITED, "미리보기 프로세스가 예기치 않게 끝나 창을 닫았습니다. 다시 열어 주세요"),
        ];
        for (prefix, sentence) in cases {
            let line = format!("{{\"message\":\"{prefix}; the preview was closed\"}}");
            assert_eq!(end_preview(Some(&line), 1).as_deref(), Some(sentence), "{prefix}");
        }
        assert_eq!(end_preview(None, 1).as_deref(), Some("미리보기 창이 예기치 않게 닫혔습니다. 다시 열어 주세요"), "a failure with no reason");
        assert_eq!(end_preview(None, 0), None, "a normal close shows nothing");
        let stale = format!("{{\"message\":\"{}; the preview was closed\"}}", designllm_web_preview::PREVIEW_UNRESPONSIVE);
        assert_eq!(end_preview(Some(&stale), 0), None, "a successful close ignores old diagnostic text");
    }

    /// The stderr file is read from its end, and not at all past `PREVIEW_STDERR_MAX_BYTES`.
    #[test]
    fn the_preview_reason_is_read_from_the_end_of_a_bounded_stderr_file() {
        let folder = std::env::temp_dir().join(format!("designllm-preview-stderr-{}", std::process::id()));
        std::fs::create_dir_all(&folder).expect("a test folder");
        let path = folder.join("stderr.err");
        let line = "x".repeat(1023) + "\n";
        let reason = format!("{{\"message\":\"{}; the preview was closed\"}}\n", designllm_web_preview::PREVIEW_UNRESPONSIVE);
        let known = "미리보기 화면이 응답하지 않아 창을 닫았습니다. 다시 열어 주세요";
        let generic = "미리보기 창이 예기치 않게 닫혔습니다. 다시 열어 주세요";

        // Past the cap the file is not read, so the reason at its end does not count: the generic sentence.
        let oversized = line.repeat(PREVIEW_STDERR_MAX_BYTES as usize / line.len() + 1) + &reason;
        std::fs::write(&path, oversized).expect("the oversized stderr");
        let read = read_preview_stderr(&path);
        assert!(read.is_empty(), "a file past the cap is not read");
        assert_eq!(crate::app::messages::preview_exit_message(&read, false).as_deref(), Some(generic), "past the cap");

        // Under the cap and far larger than the tail: only the tail is read, and the reason at its end is found.
        std::fs::write(&path, line.repeat(256) + &reason).expect("the large stderr");
        let read = read_preview_stderr(&path);
        assert!(read.len() <= PREVIEW_STDERR_TAIL_BYTES as usize, "{} bytes were read", read.len());
        assert_eq!(crate::app::messages::preview_exit_message(&read, false).as_deref(), Some(known), "under the cap");

        let _ = std::fs::remove_dir_all(&folder);
    }

    /// The app's end removes the stderr file of a preview child that the poll has not read yet.
    #[test]
    fn ending_the_app_removes_the_stderr_file_of_a_preview_child_not_yet_read() {
        use std::os::windows::process::CommandExt;
        let _shared = shared_stderr_file();
        let mut harness = Harness::new();
        let file = std::fs::File::create(preview_stderr_path()).expect("the preview's stderr file");
        let child = Command::new("cmd")
            .raw_arg("/C exit 0")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::from(file))
            .spawn()
            .expect("the fake preview child starts");
        harness.app.web_preview_child = Some(child);
        drop(harness);
        assert!(!preview_stderr_path().exists(), "the app's end removed the file");
    }
}
