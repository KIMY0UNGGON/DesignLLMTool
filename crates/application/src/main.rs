#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]
use designllm_contracts::*;
use serde_json::Value;
use std::{io::Read, path::PathBuf};

mod gl_check;

#[derive(Default)]
struct Args { workspace: Option<PathBuf>, project: Option<PathBuf>, mode: String, command: Option<PathBuf>, output: Option<PathBuf>, checkpoint_id: Option<String>, destination: Option<PathBuf>, job: Option<String>, attempt: Option<String>, web_path: Option<PathBuf>, parent_hwnd: Option<isize>, invisible: bool, background: bool, ui_test_mock_agent: Option<PathBuf> }
fn parse() -> DltResult<Args> {
    let mut result = Args::default();
    let mut workspace_occurrences = 0usize;
    let mut args = std::env::args_os().skip(1);
    while let Some(arg) = args.next() {
        match arg.to_string_lossy().as_ref() {
            "--workspace" => {
                workspace_occurrences += 1;
                result.workspace = Some(PathBuf::from(args.next().ok_or_else(|| DltError::invalid("--workspace needs a path"))?));
            },
            "--project" => {
                if result.project.is_some() { return Err(DltError::invalid("Specify --project only once")); }
                result.project = Some(PathBuf::from(args.next().ok_or_else(|| DltError::invalid("--project needs a folder"))?));
            },
            "--host" | "--headless" | "--mcp" | "--smoke-test" | "--command" | "--version" | "--help" | "--list-migration-checkpoints" | "--pdf-worker" | "--gl-probe" | "--wgpu-probe" | "--wgpu-warp-probe" => set_mode(&mut result.mode, &arg.to_string_lossy())?,
            "--restore-migration-checkpoint" => {
                set_mode(&mut result.mode, "--restore-migration-checkpoint")?;
                if result.checkpoint_id.is_some() { return Err(DltError::invalid("checkpoint ID may be specified only once")); }
                let id = args.next().ok_or_else(|| DltError::invalid("--restore-migration-checkpoint needs a checkpoint ID"))?.to_string_lossy().into_owned();
                if id.trim().is_empty() { return Err(DltError::invalid("checkpoint ID must not be empty")); }
                result.checkpoint_id = Some(id);
            },
            "--command-file" => { set_mode(&mut result.mode, "--command")?; result.command = Some(PathBuf::from(args.next().ok_or_else(|| DltError::invalid("--command-file needs a path"))?)); },
            "--output" => result.output = Some(PathBuf::from(args.next().ok_or_else(|| DltError::invalid("--output needs a path"))?)),
            "--destination" => {
                if result.destination.is_some() { return Err(DltError::invalid("--destination may be specified only once")); }
                let path = PathBuf::from(args.next().ok_or_else(|| DltError::invalid("--destination needs a new folder path"))?);
                if path.as_os_str().is_empty() { return Err(DltError::invalid("--destination must name a new folder")); }
                result.destination = Some(path);
            },
            "--preview-web" => { set_mode(&mut result.mode, "--preview-web")?; result.web_path = Some(PathBuf::from(args.next().ok_or_else(|| DltError::invalid("--preview-web needs a path"))?)); },
            "--parent-hwnd" => result.parent_hwnd = Some(args.next().ok_or_else(|| DltError::invalid("--parent-hwnd needs a handle"))?.to_string_lossy().parse().map_err(|_| DltError::invalid("Invalid window handle"))?),
            "--invisible" => result.invisible = true,
            "--background" => result.background = true,
            // Task 69: only the window of a UI-test launch passes this to the host it starts (`ensure_host`).
            "--ui-test-mock-agent" => result.ui_test_mock_agent = Some(designllm_application::ui_test_mock_agent_path(&args.next().ok_or_else(|| DltError::invalid("--ui-test-mock-agent needs a path"))?)?),
            "--job-id" => result.job = Some(args.next().ok_or_else(|| DltError::invalid("--job-id needs an ID"))?.to_string_lossy().into_owned()),
            "--attempt-id" => result.attempt = Some(args.next().ok_or_else(|| DltError::invalid("--attempt-id needs an ID"))?.to_string_lossy().into_owned()),
            other => return Err(DltError::invalid(format!("Unknown argument: {other}"))),
        }
    }
    if result.project.is_some() && result.workspace.is_some() { return Err(DltError::invalid("Specify either --project or --workspace")); }
    if (result.invisible || result.parent_hwnd.is_some()) && result.mode != "--preview-web" { return Err(DltError::invalid("--invisible/--parent-hwnd apply only to --preview-web; use --smoke-test for the desktop")); }
    if (result.job.is_some() || result.attempt.is_some()) && result.mode != "--mcp" { return Err(DltError::invalid("Job binding applies only to --mcp")); }
    if result.background && result.mode != "--mcp" { return Err(DltError::invalid("--background applies only to --mcp")); }
    if result.ui_test_mock_agent.is_some() && result.mode != "--host" { return Err(DltError::invalid("--ui-test-mock-agent applies only to --host")); }
    // A host started by hand takes a mock agent only inside a UI-test launch (the window's host inherits its variable).
    if result.ui_test_mock_agent.is_some() && !designllm_application::ui_test_launch() { return Err(DltError::invalid("--ui-test-mock-agent needs DESIGNLLM_UI_TEST_INPUT (a UI-test launch)")); }
    if result.checkpoint_id.is_some() && result.mode != "--restore-migration-checkpoint" { return Err(DltError::invalid("checkpoint IDs apply only to --restore-migration-checkpoint")); }
    if result.destination.is_some() && result.mode != "--restore-migration-checkpoint" { return Err(DltError::invalid("--destination applies only to --restore-migration-checkpoint")); }
    if matches!(result.mode.as_str(), "--list-migration-checkpoints" | "--restore-migration-checkpoint") {
        if workspace_occurrences > 1 { return Err(DltError::invalid("checkpoint commands accept exactly one source --workspace")); }
        if result.output.is_some() { return Err(DltError::invalid("--output is not supported by checkpoint commands")); }
    }
    if result.mode == "--restore-migration-checkpoint" && result.destination.is_none() { return Err(DltError::invalid("--restore-migration-checkpoint requires --destination NEW_FOLDER")); }
    Ok(result)
}
fn set_mode(current: &mut String, new: &str) -> DltResult<()> {
    if !current.is_empty() && current != new { return Err(DltError::invalid("Specify one execution mode")); }
    *current = new.to_owned(); Ok(())
}
fn run() -> DltResult<()> {
    let args = parse()?;
    if args.mode == "--version" { println!("DesignLLMTool {}", env!("CARGO_PKG_VERSION")); return Ok(()); }
    // The hidden PDF reader worker (task 34c): one request on stdin, one reply frame on stdout. It opens no workspace.
    if args.mode == "--pdf-worker" { return designllm_application::pdf_worker::run_worker(); }
    // The hidden renderer probe children of `gl_check::require` (tasks 62, 65). They open no workspace.
    if let Some(renderer) = gl_check::probe_renderer(&args.mode) { return gl_check::run_probe(renderer); }
    if args.mode == "--help" { println!("DesignLLMTool [--project EXISTING_FOLDER | --workspace PATH]\n  --project stores the workspace in .designllm and saves native files in artifacts inside the project\n  --headless | --host | --mcp [--background]\n  --command-file JSON_FILE | --command (JSON on stdin)\n  --list-migration-checkpoints --workspace SOURCE\n  --restore-migration-checkpoint ID --workspace SOURCE --destination NEW_FOLDER\n  --smoke-test --workspace SCRATCH --output SCREENSHOT.png\n  --preview-web HTML_PATH [--parent-hwnd HANDLE] [--invisible]"); return Ok(()); }
    let explicit_workspace = args.workspace.is_some();
    if matches!(args.mode.as_str(), "--list-migration-checkpoints" | "--restore-migration-checkpoint")
        && (!explicit_workspace || args.workspace.as_ref().is_some_and(|path| path.as_os_str().is_empty()))
    {
        return Err(DltError::invalid("checkpoint commands require a non-empty explicit source --workspace path; they never select or create the default workspace"));
    }
    let workspace = match (args.project, args.workspace) {
        (Some(project), None) => match designllm_application::project_workspace(&project) {
            Ok(workspace) => workspace,
            Err(error) => {
                if args.mode.is_empty() {
                    gl_check::show_message_box(&format!("프로젝트 폴더를 열 수 없습니다.\n\n{}", error.message));
                }
                return Err(error);
            }
        },
        (None, Some(path)) => path,
        (None, None) => designllm_application::default_workspace()?,
        (Some(_), Some(_)) => return Err(DltError::invalid("Specify either --project or --workspace")),
    };
    match args.mode.as_str() {
        "--preview-web" => {
            if let Some(runtime) = designllm_application::web_runtime_directory() { std::env::set_var("DESIGNLLM_WEBVIEW2_DIR", runtime); }
            designllm_web_preview::run(&args.web_path.ok_or_else(|| DltError::invalid("Web path is required"))?, args.parent_hwnd, args.invisible)
        }
        "--host" | "--headless" => {
            let mode = if args.mode == "--headless" { designllm_host::HostMode::Headless } else { designllm_host::HostMode::Managed };
            // Only a host runs PDF reads, in this executable's worker mode (task 34c). Other modes register nothing.
            designllm_application::pdf_worker::register_current_exe();
            let opened = designllm_application::create_workspace(&workspace).map_err(|error| {
                if args.mode == "--host" { report_host_startup_error(&error); }
                error
            })?;
            let mut options = designllm_host::HostOptions::default();
            options.runtime.mock_agent = args.ui_test_mock_agent;
            designllm_host::serve_with(opened, mode, options).map_err(|error| {
                if args.mode == "--host" { report_host_startup_error(&error); }
                error
            })
        }
        "--mcp" => {
            let binding = match (args.job, args.attempt) { (None, None) => None, (Some(job_id), Some(attempt_id)) => Some(JobRef { job_id, attempt_id }), _ => return Err(DltError::invalid("Both job and attempt IDs are required")) };
            if binding.is_some() {
                return designllm_bridge::run_stdio(&workspace, binding);
            }
            let root = workspace.clone();
            designllm_bridge::mcp::run_stdio_with_startup(&workspace, move |background| {
                if args.background || background { designllm_application::ensure_host(&root) }
                else { designllm_application::ensure_visible_app(&root) }
            })
        }
        "--command" => {
            let text = if let Some(path) = args.command { std::fs::read_to_string(path)? } else { let mut text=String::new(); std::io::stdin().read_to_string(&mut text)?; text };
            let command: Value = serde_json::from_str(&text).map_err(|e| DltError::invalid(e.to_string()))?;
            let method = command.get("method").and_then(Value::as_str).ok_or_else(|| DltError::invalid("method is required"))?;
            // A command line is not the desktop window: it connects as `cli`, never as the verified `gui`.
            let result = designllm_application::cli_client(&workspace).call(method, command.get("params").cloned().unwrap_or_else(|| serde_json::json!({})))?;
            println!("{}", serde_json::to_string(&result).unwrap()); Ok(())
        }
        "--list-migration-checkpoints" => {
            let checkpoints = designllm_application::list_migration_checkpoints(&workspace)?;
            println!("{}", serde_json::to_string(&serde_json::json!({"checkpoints": checkpoints})).unwrap());
            Ok(())
        }
        "--restore-migration-checkpoint" => {
            let id = args.checkpoint_id.as_deref().expect("restore parser requires a checkpoint ID");
            let destination = args.destination.as_deref().expect("restore parser requires a destination");
            let restored = designllm_application::restore_migration_checkpoint(&workspace, id, destination)?;
            let workspace_id = restored.workspace.id().to_owned();
            let restored_root = restored.workspace.root().to_path_buf();
            let diagnostics = restored.diagnostics;
            drop(restored.workspace);
            println!("{}", serde_json::to_string(&serde_json::json!({"workspace_id": workspace_id, "destination": restored_root.display().to_string(), "diagnostics": diagnostics})).unwrap());
            Ok(())
        }
        "--smoke-test" => {
            if !explicit_workspace { return Err(DltError::invalid("Smoke tests require an explicit scratch --workspace")); }
            let output = args.output.ok_or_else(|| DltError::invalid("Smoke tests require --output screenshot.png"))?;
            gl_check::require(gl_check::Report::Stderr)?;
            designllm_application::ensure_host(&workspace)?;
            designllm_application::start_gui_heartbeat(&workspace);
            let artifact = designllm_application::prepare_smoke_canvas(&workspace)?;
            designllm_desktop::run_smoke(&workspace, &output)?;
            println!("{}", serde_json::json!({"smoke":"complete","artifact_id":artifact.id,"revision":artifact.revision,"screenshot":output,"renderer":designllm_desktop::renderer::selected().name()})); Ok(())
        }
        "" => {
            gl_check::require(gl_check::Report::MessageBox)?;
            // Task 69: a UI-test window (DESIGNLLM_UI_TEST_INPUT) may give its host the mock agent for scripted jobs; no
            // other launch reads DESIGNLLM_UI_TEST_MOCK_AGENT.
            if designllm_desktop::ui_test_mode_enabled() {
                if let Some(raw) = std::env::var_os("DESIGNLLM_UI_TEST_MOCK_AGENT").filter(|raw| !raw.is_empty()) {
                    designllm_application::set_ui_test_mock_agent(designllm_application::ui_test_mock_agent_path(&raw)?);
                }
            }
            if let Err(error) = designllm_application::ensure_host(&workspace) {
                if let Err(display_error) = designllm_desktop::run_startup_error(&workspace, &error) {
                    eprintln!("Could not display startup error: {display_error}");
                }
                return Err(error);
            }
            designllm_application::start_gui_heartbeat(&workspace);
            designllm_desktop::set_host_starter(designllm_application::ensure_host);
            designllm_desktop::run(&workspace)
        }
        _ => Err(DltError::invalid("Unsupported mode")),
    }
}
fn main() {
    // rhwp downloads web fonts only while RHWP_WEBFONT_OFFLINE is unset; keep that path offline. Set before any thread starts.
    std::env::set_var("RHWP_WEBFONT_OFFLINE", "1");
    if let Err(error) = run() {
        let text = serde_json::to_string(&error).unwrap_or_else(|_| error.to_string());
        eprintln!("{text}");
        std::process::exit(if error.code == ErrorCode::WorkspaceLocked { 3 } else { 1 });
    }
}

fn report_host_startup_error(error: &DltError) {
    use std::io::Write;
    if let Some(path) = std::env::var_os("DESIGNLLM_HOST_STARTUP_ERROR_PATH") {
        if let (Ok(text), Ok(mut file)) = (serde_json::to_vec(error), std::fs::OpenOptions::new().write(true).create_new(true).open(path)) {
            let _ = file.write_all(&text);
        }
    }
}
