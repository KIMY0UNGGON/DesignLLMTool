//! Renderer pre-check before the desktop window opens (tasks 62, 65). A broken OpenGL driver can crash inside WGL
//! (seen: a host AMD ICD projected into Windows Sandbox fails in `DescribePixelFormat` with 0xC0000005), which
//! kills the process before any error reaches the user. So this executable first starts itself hidden with a probe
//! argument; the child does only eframe's renderer start (`designllm_desktop::gl_probe`). OpenGL is tried first; if
//! its child does not end with 0 within the time limit, wgpu on a DirectX 12 hardware adapter is tried the same way,
//! then wgpu on WARP. The first renderer whose child passes is selected for this process's windows. When none passes,
//! the launch stops with a message and `GL_UNAVAILABLE_EXIT_CODE`. `DESIGNLLM_RENDERER` names the only renderer to try.

use designllm_contracts::{DltError, DltResult, ErrorCode};
use designllm_desktop::renderer::{self, Renderer};
use designllm_runtimes::win;
use std::{
    io::Read,
    os::windows::io::AsRawHandle,
    process::{Command, ExitStatus, Stdio},
    time::{Duration, Instant},
};

/// Exit code of a launch stopped because no renderer can open the window. 1 is any other error, 3 is WORKSPACE_LOCKED.
pub const GL_UNAVAILABLE_EXIT_CODE: i32 = 4;
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);
/// How much of the child's stderr is kept for the message.
const STDERR_LIMIT: u64 = 64 * 1024;
/// How long the report waits for the child's stderr text once the child has ended.
const STDERR_WAIT: Duration = Duration::from_secs(2);

/// How a failed check is shown. A hidden run (`--smoke-test`) must not open a message box.
pub enum Report {
    MessageBox,
    Stderr,
}

/// The probe argument of each renderer's child.
pub fn probe_argument(renderer: Renderer) -> &'static str {
    match renderer {
        Renderer::Glow => "--gl-probe",
        Renderer::Wgpu => "--wgpu-probe",
        Renderer::WgpuWarp => "--wgpu-warp-probe",
    }
}

pub fn probe_renderer(argument: &str) -> Option<Renderer> {
    Renderer::FALLBACK_ORDER.into_iter().find(|renderer| probe_argument(*renderer) == argument)
}

/// Test hook read by the probe child of a debug build only (release ignores it): `hang` outlasts the time limit,
/// `orphan` first starts a hidden grandchild that hangs with this child's stderr open (as a driver helper process
/// could), and any other non-empty value ends the child at once with STATUS_ACCESS_VIOLATION (0xC0000005).
#[cfg(debug_assertions)]
fn fail_hook(renderer: Renderer) -> &'static str {
    match renderer {
        Renderer::Glow => "DESIGNLLM_GL_PROBE_FAIL",
        Renderer::Wgpu => "DESIGNLLM_WGPU_PROBE_FAIL",
        Renderer::WgpuWarp => "DESIGNLLM_WGPU_WARP_PROBE_FAIL",
    }
}

/// A probe child. Exits 0 from inside eframe once the renderer (GL context and painter, or wgpu device and surface)
/// exists.
pub fn run_probe(renderer: Renderer) -> DltResult<()> {
    #[cfg(windows)]
    {
        #[link(name = "kernel32")]
        extern "system" {
            fn SetErrorMode(mode: u32) -> u32;
        }
        const SEM_FAILCRITICALERRORS: u32 = 0x0001;
        const SEM_NOGPFAULTERRORBOX: u32 = 0x0002;
        // A crash in the driver ends this child without the Windows crash dialog; the parent reports it.
        // SAFETY: SetErrorMode takes a flag value and touches only this process's error mode.
        unsafe { SetErrorMode(SEM_FAILCRITICALERRORS | SEM_NOGPFAULTERRORBOX) };
    }
    #[cfg(debug_assertions)]
    if let Some(value) = std::env::var_os(fail_hook(renderer)).filter(|value| !value.is_empty()) {
        if value == "hang" {
            std::thread::sleep(PROBE_TIMEOUT * 6);
            return Ok(());
        }
        if value == "orphan" {
            use std::os::windows::process::CommandExt;
            // stderr is inherited: the grandchild holds the parent's pipe after this child is gone.
            let _ = Command::new(std::env::current_exe()?)
                .arg(probe_argument(renderer))
                .env(fail_hook(renderer), "hang")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
                .spawn();
        }
        std::process::exit(0xC000_0005_u32 as i32);
    }
    designllm_desktop::gl_probe::run(renderer)
}

/// Selects the first renderer whose probe child passes (or could not be started) and returns. If none passes, reports
/// and exits with `GL_UNAVAILABLE_EXIT_CODE`. An unknown `DESIGNLLM_RENDERER` value is an invalid-argument error.
pub fn require(report: Report) -> DltResult<()> {
    let forced = Renderer::forced().map_err(DltError::invalid)?;
    let candidates = forced.map_or_else(|| Renderer::FALLBACK_ORDER.to_vec(), |renderer| vec![renderer]);
    let mut failures = Vec::new();
    for renderer in candidates {
        match probe(renderer) {
            None => {
                renderer::select(renderer);
                return Ok(());
            }
            Some(reason) => failures.push((renderer, reason)),
        }
    }
    let error = DltError::new(ErrorCode::Unavailable, message(&failures));
    eprintln!("{}", serde_json::to_string(&error).unwrap_or_else(|_| error.message.clone()));
    if matches!(report, Report::MessageBox) {
        show_message_box(&error.message);
    }
    std::process::exit(GL_UNAVAILABLE_EXIT_CODE);
}

/// The error code part of the message, or None when the child ended with 0. A child that cannot be started
/// says nothing about the driver, so the launch goes on as it did before this check.
fn probe(renderer: Renderer) -> Option<String> {
    let mut command = Command::new(std::env::current_exe().ok()?);
    command.arg(probe_argument(renderer)).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let mut child = command.spawn().ok()?;
    // A kill-on-close job ties the hidden child to this process: if the launcher dies first, Windows ends the
    // child with it. A job that cannot be made or joined does not stop the check; the time-limit kill below stays.
    let job = win::JobObject::create().ok().filter(|job| job.assign(child.as_raw_handle() as _).is_ok());
    // Read stderr on its own thread so a long panic text cannot fill the pipe and stall the child.
    let stderr = child.stderr.take().map(|pipe| {
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut text = String::new();
            let _ = pipe.take(STDERR_LIMIT).read_to_string(&mut text);
            let _ = sender.send(text);
        });
        receiver
    });
    let started = Instant::now();
    let outcome = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) if started.elapsed() < PROBE_TIMEOUT => std::thread::sleep(Duration::from_millis(5)),
            Ok(None) => break Err(format!("시간 초과({}초)", PROBE_TIMEOUT.as_secs())),
            Err(error) => break Err(format!("확인 실패({error})")),
        }
    };
    if outcome.is_err() {
        let _ = child.kill();
        let _ = child.wait();
    }
    // A process the child started (a driver helper) can still hold the stderr pipe open. Closing the job ends every
    // process left in it, which closes the pipe; without a job, the wait for the text is bounded instead.
    drop(job);
    let stderr = stderr.and_then(|reader| reader.recv_timeout(STDERR_WAIT).ok()).unwrap_or_default();
    match outcome {
        Ok(status) if status.success() => None,
        Ok(status) => Some(reason(status, &stderr)),
        Err(reason) => Some(reason),
    }
}

/// The exit code (hex for an NTSTATUS such as 0xC0000005) and, if the child said why, its error message.
fn reason(status: ExitStatus, stderr: &str) -> String {
    let code = match status.code() {
        Some(code) if (0..=255).contains(&code) => code.to_string(),
        Some(code) => format!("0x{:08X}", code as u32),
        None => status.to_string(),
    };
    let detail = stderr.trim();
    if detail.is_empty() {
        return code;
    }
    let detail = serde_json::from_str::<DltError>(detail).map_or_else(|_| detail.to_owned(), |error| error.message);
    format!("{code}\n{detail}")
}

/// One tried renderer: its code alone. Several: one `interface: code` line each, in the order tried.
fn message(failures: &[(Renderer, String)]) -> String {
    let mut interfaces: Vec<&str> = Vec::new();
    for (renderer, _) in failures {
        let interface = if *renderer == Renderer::Glow { "OpenGL" } else { "DirectX 12" };
        if !interfaces.contains(&interface) {
            interfaces.push(interface);
        }
    }
    let codes = match failures {
        [(_, reason)] => format!(" {reason}"),
        _ => failures.iter().map(|(renderer, reason)| format!("\n{}: {reason}", renderer.api_name())).collect(),
    };
    format!(
        "그래픽 드라이버({})를 열 수 없어 DesignLLMTool 화면을 띄우지 못했습니다.\n\
         그래픽 카드 제조사(AMD·NVIDIA·Intel) 웹사이트나 Windows 업데이트에서 그래픽 드라이버를 최신으로 갱신한 뒤 다시 실행해 주세요.\n\n\
         오류 코드:{codes}",
        interfaces.join("·")
    )
}

fn show_message_box(text: &str) {
    #[cfg(windows)]
    {
        #[link(name = "user32")]
        extern "system" {
            fn MessageBoxW(window: *mut std::ffi::c_void, text: *const u16, caption: *const u16, kind: u32) -> i32;
        }
        const MB_ICONERROR: u32 = 0x0000_0010;
        let wide = |value: &str| value.encode_utf16().chain(std::iter::once(0)).collect::<Vec<u16>>();
        let (text, caption) = (wide(text), wide("DesignLLMTool"));
        // SAFETY: both strings are NUL-terminated UTF-16 buffers that outlive the call; no owner window.
        unsafe { MessageBoxW(std::ptr::null_mut(), text.as_ptr(), caption.as_ptr(), MB_ICONERROR) };
    }
    #[cfg(not(windows))]
    let _ = text;
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use std::os::windows::process::ExitStatusExt;

    #[test]
    fn an_ntstatus_exit_is_shown_in_hex_and_a_small_exit_in_decimal() {
        assert_eq!(reason(ExitStatus::from_raw(0xC000_0005), ""), "0xC0000005");
        assert_eq!(reason(ExitStatus::from_raw(1), "  \n"), "1");
    }

    #[test]
    fn the_message_of_a_structured_child_error_follows_the_code() {
        let stderr = serde_json::to_string(&DltError::new(ErrorCode::Unavailable, "Painter error")).unwrap();
        assert_eq!(reason(ExitStatus::from_raw(1), &stderr), "1\nPainter error");
        assert_eq!(reason(ExitStatus::from_raw(1), "plain text\n"), "1\nplain text");
    }

    #[test]
    fn the_message_names_the_driver_and_the_code() {
        let text = message(&[(Renderer::Glow, "0xC0000005".into())]);
        assert!(text.starts_with("그래픽 드라이버(OpenGL)를 열 수 없어"), "{text}");
        assert!(text.contains("드라이버를 최신으로 갱신") && text.ends_with("오류 코드: 0xC0000005"), "{text}");
        let text = message(&[(Renderer::WgpuWarp, "1".into())]);
        assert!(text.starts_with("그래픽 드라이버(DirectX 12)를 열 수 없어") && text.ends_with("오류 코드: 1"), "{text}");
    }

    #[test]
    fn the_message_of_every_tried_renderer_lists_each_code_in_order() {
        let failures = [
            (Renderer::Glow, "0xC0000005".to_owned()),
            (Renderer::Wgpu, "시간 초과(10초)".to_owned()),
            (Renderer::WgpuWarp, "1\nNo adapter".to_owned()),
        ];
        let text = message(&failures);
        assert!(text.starts_with("그래픽 드라이버(OpenGL·DirectX 12)를 열 수 없어"), "{text}");
        assert!(
            text.ends_with("오류 코드:\nOpenGL: 0xC0000005\nDirectX 12: 시간 초과(10초)\nDirectX 12 WARP: 1\nNo adapter"),
            "{text}"
        );
    }

    #[test]
    fn each_renderer_has_its_own_probe_argument() {
        for renderer in Renderer::FALLBACK_ORDER {
            assert_eq!(probe_renderer(probe_argument(renderer)), Some(renderer));
        }
        assert_eq!(probe_argument(Renderer::Glow), "--gl-probe");
        assert_eq!(probe_renderer("--smoke-test"), None);
    }
}
