mod app;
mod board;
mod commands;
mod content_icons;
mod dialogs;
mod figure_layout;
mod frame;
pub mod gl_probe;
mod grapheme;
mod i18n;
mod icons;
mod icons_table;
mod notice;
mod pixels;
pub mod renderer;
mod smoke_window;
mod rpc;
mod strings;
mod theme;
mod ui_record;
mod widgets;

use std::path::Path;

use designllm_contracts::{DltError, DltResult, ErrorCode};

/// How the launch starts the hidden host for a workspace (`designllm_application::ensure_host`). The window's `다시 연결`
/// uses it when its host has gone; without it the window can only close.
static HOST_STARTER: std::sync::OnceLock<fn(&Path) -> DltResult<()>> = std::sync::OnceLock::new();

pub fn set_host_starter(starter: fn(&Path) -> DltResult<()>) {
    let _ = HOST_STARTER.set(starter);
}

/// Whether this launch turns the UI-test input on (`DESIGNLLM_UI_TEST_INPUT`, task 69). The launcher gives the host a mock
/// agent only then.
pub fn ui_test_mode_enabled() -> bool {
    app::ui_test_mode_from_env().is_some()
}

pub fn run(workspace: &Path) -> DltResult<()> {
    run_app(workspace, None)
}

pub fn run_smoke(workspace: &Path, output: &Path) -> DltResult<()> {
    run_app(workspace, Some(output))
}

/// Shows a compact eframe window when the managed host cannot be started. The reason is a user-facing
/// sentence; the workspace log path is always included because the host may not exist to answer RPCs.
pub fn run_startup_error(workspace: &Path, error: &DltError) -> DltResult<()> {
    dialogs::set_headless(true);
    let view = StartupErrorView {
        cause: app::user_error(error),
        recovery_warning: app::startup_recovery_warning(error),
        log_path: workspace.join("logs").join("host.log").to_string_lossy().into_owned(),
    };
    let title = "DesignLLMTool";
    let viewport = eframe::egui::ViewportBuilder::default()
        .with_title(title)
        .with_inner_size([640.0, 360.0])
        .with_min_inner_size([520.0, 300.0])
        .with_icon(crate::icons::window_icon());
    let options = renderer::native_options(viewport);
    eframe::run_native(
        title,
        options,
        Box::new(move |cc| {
            theme::install(&cc.egui_ctx);
            Ok(Box::new(StartupErrorApp { view }))
        }),
    )
    .map_err(|error| DltError::new(ErrorCode::Internal, error.to_string()))
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct StartupErrorView {
    cause: String,
    recovery_warning: Option<String>,
    log_path: String,
}

impl StartupErrorView {
    fn draw(&self, ctx: &eframe::egui::Context) {
        let palette = theme::palette(ctx);
        eframe::egui::CentralPanel::default().show(ctx, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space(28.0);
                ui.heading(crate::i18n::tr("작업공간에 연결할 수 없습니다"));
                ui.add_space(12.0);
                widgets::callout(ui, &palette, &self.cause, &[], false);
                if let Some(warning) = &self.recovery_warning {
                    ui.add_space(8.0);
                    widgets::callout(ui, &palette, warning, &[], true);
                }
                ui.add_space(12.0);
                ui.label(crate::i18n::tr("자세한 내용은 다음 로그에서 확인할 수 있습니다."));
                ui.monospace(&self.log_path);
            });
        });
    }
}

struct StartupErrorApp { view: StartupErrorView }

impl eframe::App for StartupErrorApp {
    fn update(&mut self, ctx: &eframe::egui::Context, _frame: &mut eframe::Frame) {
        self.view.draw(ctx);
    }
}

fn project_folder(workspace: &Path) -> Option<std::path::PathBuf> {
    if !workspace.file_name()?.eq_ignore_ascii_case(designllm_contracts::PROJECT_STORE_DIR) { return None; }
    std::path::absolute(workspace).ok()?.parent().map(Path::to_owned)
}

fn open_project_window(project: &Path) -> std::io::Result<()> {
    use std::process::{Command, Stdio};
    let mut command = Command::new(std::env::current_exe()?);
    command
        .arg("--project").arg(project)
        .env_remove("DESIGNLLM_MCP_GUI")
        .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(designllm_runtimes::job::CREATE_NO_WINDOW);
    }
    // Unix: its own process group, so closing this window's terminal does not end the new window.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    command.spawn()?;
    Ok(())
}

fn run_app(workspace: &Path, smoke_output: Option<&Path>) -> DltResult<()> {
    use std::sync::{Arc, Mutex};

    // D19.5: what a forced exit during a video insert left in the temp folders; another running window's files stay.
    let _ = std::thread::Builder::new().name("designllm-temp-cleanup".into()).spawn(app::remove_orphaned_video_poster_temp);
    let worker = rpc::RpcWorker::spawn(workspace)?;
    let project = project_folder(workspace);
    let folder = project.as_deref().unwrap_or(workspace);
    let workspace_name = folder.file_name().map_or_else(
        || if project.is_some() { folder.display().to_string() } else { "작업공간".to_owned() },
        |name| name.to_string_lossy().into_owned(),
    );
    let smoke_path = smoke_output.map(Path::to_path_buf);
    let workspace_path = workspace.to_path_buf();
    let host_starter = HOST_STARTER.get().copied().map(|start| {
        let workspace = workspace_path.clone();
        std::sync::Arc::new(move || start(&workspace)) as app::HostStarter
    });
    let smoke_result = smoke_path.as_ref().map(|_| Arc::new(Mutex::new(None)));
    let result_for_app = smoke_result.clone();
    // Task 69: synthetic input for UI tests, off unless the launch asks for it (docs/UI_CHANNEL.md "시험용 입력").
    let (ui_test, visible, headless) = app::ui_test_launch_mode(smoke_path.is_some());
    let ui_test_scale = ui_test.and_then(|_| app::ui_test_scale_from_env());
    let app_name = if ui_test.is_some() { "DesignLLMTool [UI 시험 입력]" } else { "DesignLLMTool" };
    let title = if project.is_some() { format!("{workspace_name} · {app_name}") } else { app_name.to_owned() };
    // No visible main window: every native dialog and preview window is refused (`dialogs::headless`).
    dialogs::set_headless(headless);
    // Smoke captures and off-screen UI tests only: `DESIGNLLM_SMOKE_SIZE=800x560` sets the window's logical size.
    let size = (!visible)
        .then(|| std::env::var("DESIGNLLM_SMOKE_SIZE").ok())
        .flatten()
        .and_then(|value| {
            let (width, height) = value.split_once('x')?;
            Some([width.trim().parse::<f32>().ok()?, height.trim().parse::<f32>().ok()?])
        })
        .unwrap_or([1440.0, 900.0]);
    let mut viewport = eframe::egui::ViewportBuilder::default()
        .with_title(title)
        .with_inner_size(size)
        .with_min_inner_size([800.0, 560.0])
        .with_visible(visible)
        .with_icon(crate::icons::window_icon())
        .with_active(visible);
    if !visible {
        // eframe shows the window after its first frame anyway; a smoke window is shown outside every monitor.
        viewport = viewport.with_position(smoke_window::offscreen_position(size));
    }
    let options = renderer::native_options(viewport);
    let result = eframe::run_native(
        "DesignLLMTool",
        options,
        Box::new(move |cc| {
            theme::install(&cc.egui_ctx);
            // Smoke captures only: a zoom factor (the display scale is fixed) and the dark theme.
            if let Some(zoom) = std::env::var("DESIGNLLM_SMOKE_ZOOM").ok().and_then(|value| value.parse::<f32>().ok()) {
                cc.egui_ctx.set_zoom_factor(zoom);
            }
            if std::env::var("DESIGNLLM_SMOKE_THEME").as_deref() == Ok("dark") {
                cc.egui_ctx.set_theme(eframe::egui::Theme::Dark);
            }
            // U02 (task 69): a UI-test window at a forced zoom (`DESIGNLLM_UI_TEST_SCALE`), egui's own scaling path.
            if let Some(scale) = ui_test_scale {
                cc.egui_ctx.set_zoom_factor(scale);
            }
            Ok(Box::new(
                app::DesktopApp::new(worker, workspace_name.clone(), smoke_path, result_for_app)
                    .with_workspace(&workspace_path)
                    .with_host_starter(host_starter.clone())
                    .with_ui_test_input(ui_test),
            ))
        }),
    );
    result.map_err(|error| DltError::new(ErrorCode::Internal, error.to_string()))?;
    if let Some(shared) = smoke_result {
        let message = shared.lock().unwrap_or_else(|poison| poison.into_inner()).take();
        if let Some(message) = message {
            return Err(DltError::new(ErrorCode::Internal, message));
        }
    }
    Ok(())
}

#[cfg(test)]
mod startup_error_tests {
    use super::*;
    use designllm_contracts::Diagnostic;
    use eframe::egui::{Context, Pos2, RawInput, Rect, Vec2};

    #[test]
    fn startup_error_view_shows_a_safe_cause_and_host_log_path_offscreen() {
        let workspace = Path::new(r"C:\workspaces\sample");
        let mut error = DltError::new(ErrorCode::HostNotRunning, "internal process path and startup details");
        error.diagnostics.push(Diagnostic {
            code: "core.pending_recovery_before_migration_failure".into(),
            message: "recovered_saves_partial=932; private staging path".into(),
            node_id: None,
            blocking: false,
            params: Default::default(),
        });
        let view = StartupErrorView {
            cause: app::user_error(&error),
            recovery_warning: app::startup_recovery_warning(&error),
            log_path: workspace.join("logs").join("host.log").to_string_lossy().into_owned(),
        };
        assert_eq!(view.cause, "작업공간에 연결할 수 없습니다");
        assert_eq!(view.recovery_warning.as_deref(), Some("이전 저장의 복구가 일부 진행됐을 수 있습니다. 처리 건수는 확인할 수 없습니다. 자세한 내용은 로그를 확인해 주세요."));
        if cfg!(windows) {
            assert_eq!(view.log_path, r"C:\workspaces\sample\logs\host.log");
        }
        assert!(!view.cause.contains("internal process"), "internal details stay in the log");
        assert!(!view.recovery_warning.as_ref().unwrap().contains("932"));
        assert!(!view.recovery_warning.as_ref().unwrap().contains("private staging"));

        let ctx = Context::default();
        theme::install(&ctx);
        let output = ctx.run(
            RawInput { screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(640.0, 360.0))), ..Default::default() },
            |ctx| view.draw(ctx),
        );
        assert!(!output.shapes.is_empty(), "the startup error is drawable without opening a window");

        let mut known_error = error;
        known_error.diagnostics[0].params = [
            ("counts_known".into(), serde_json::json!(true)),
            ("removed_pending_temps".into(), serde_json::json!(2)),
            ("recovered_saves".into(), serde_json::json!(3)),
        ]
        .into_iter()
        .collect();
        let known_view = StartupErrorView {
            cause: app::user_error(&known_error),
            recovery_warning: app::startup_recovery_warning(&known_error),
            log_path: view.log_path.clone(),
        };
        assert_eq!(
            known_view.recovery_warning.as_deref(),
            Some("이전 저장 복구가 일부 진행된 뒤 시작에 실패했습니다. 임시 파일 2개 정리, 저장 완료 3건 확인했습니다. 자세한 내용은 로그를 확인해 주세요.")
        );
        assert!(!known_view.recovery_warning.as_ref().unwrap().contains("932"));
        let known_output = ctx.run(
            RawInput { screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(640.0, 360.0))), ..Default::default() },
            |ctx| known_view.draw(ctx),
        );
        assert!(!known_output.shapes.is_empty(), "known recovery counts also draw offscreen");

        for (code, expected) in [
            (
                "core.workspace_path_too_long",
                "작업공간 경로가 너무 깁니다. 더 짧은 폴더 경로로 옮긴 뒤 다시 여세요. 이번 시도에서는 데이터베이스와 기존 백업이 변경되지 않았습니다",
            ),
            (
                "core.workspace_path_ambiguous",
                "긴 Windows 작업공간 경로에 '.' 또는 '..' 구성요소가 있어 안전하게 열 수 없습니다. 경로를 정규화한 뒤 다시 선택하세요. 이번 시도에서는 데이터베이스와 기존 백업이 변경되지 않았습니다",
            ),
        ] {
            let mut path_error = DltError::new(ErrorCode::StorageFailure, "internal storage detail");
            path_error.diagnostics.push(Diagnostic {
                code: code.into(),
                message: "internal path diagnostic".into(),
                node_id: None,
                blocking: true,
                params: Default::default(),
            });
            let path_view = StartupErrorView {
                cause: app::user_error(&path_error),
                recovery_warning: app::startup_recovery_warning(&path_error),
                log_path: view.log_path.clone(),
            };
            assert_eq!(path_view.cause, expected, "{code} must show its actionable reason");
            assert_ne!(path_view.cause, "작업을 완료하지 못했습니다. 작업공간을 확인한 뒤 다시 시도해 주세요");
            let path_output = ctx.run(
                RawInput { screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(640.0, 360.0))), ..Default::default() },
                |ctx| path_view.draw(ctx),
            );
            assert!(!path_output.shapes.is_empty(), "{code} startup reason draws offscreen");
        }
    }
}
