use designllm_contracts::*;
use std::{path::{Path, PathBuf}, sync::Arc};

/// The PDF reader worker (task 34c): the product's `source.extract_pdf` read runs in a child process of this executable.
pub mod pdf_worker;

/// Host connection of this launcher outside the desktop window (`--command`, host probes, the
/// keep-alive ping, smoke-test setup). Role `cli`: only the desktop window's own connection is the
/// verified `gui`, the role that may publish GUI view state.
pub fn cli_client(workspace: &Path) -> designllm_bridge::Client {
    designllm_bridge::Client::with_role(workspace, designllm_bridge::ClientRole::Cli)
}

/// Home "create + request" (client side, shared with the desktop window): designllm_bridge::home.
pub use designllm_bridge::home::{create_from_home, HomeOutcome, HomeRequest, Rpc};

pub fn font_directory() -> DltResult<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(value) = std::env::var_os("DESIGNLLM_FONT_DIR") { candidates.push(PathBuf::from(value)); }
    if let Ok(exe) = std::env::current_exe() { if let Some(parent) = exe.parent() { candidates.push(parent.join("assets/fonts")); } }
    // Source-tree fallback only for dev builds: a release package must carry its own fonts, and a
    // fallback would let an incomplete package pass on a machine that has the source tree.
    if cfg!(debug_assertions) { candidates.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../assets/fonts")); }
    candidates.into_iter().find(|path| path.join("Pretendard-Regular.ttf").is_file() && path.join("Pretendard-Bold.ttf").is_file())
        .ok_or_else(|| DltError::new(ErrorCode::NotFound, "Bundled fonts are missing: assets/fonts/Pretendard-Regular.ttf and Pretendard-Bold.ttf"))
}

fn render_provider() -> DltResult<Arc<dyn RenderProvider>> {
    Ok(Arc::new(designllm_render::Renderer::new(&font_directory()?)?))
}

fn engine_adapters() -> Vec<Arc<dyn EngineAdapter>> {
    vec![
        Arc::new(designllm_canvas::CanvasEngine::default()),
        Arc::new(designllm_canvas::WebEngine::default()),
        Arc::new(designllm_slides::SlidesEngine::default()),
        Arc::new(designllm_docx::DocxEngine::default()),
        Arc::new(designllm_hwpx::HwpxEngine::default()),
        Arc::new(designllm_hwp::HwpEngine::default()),
        Arc::new(designllm_figures::FigureEngine::default().with_pptx_exporter(designllm_pptx::encode_scene)),
    ]
}

/// `source.extract_pdf` reader of the product: the read runs in the registered worker process (`pdf_worker`), and is refused when
/// no worker is registered.
fn pdf_read(asset: &AssetRef, pages: &[u32], assets: &dyn AssetStore) -> DltResult<designllm_core::PdfReading> {
    pdf_worker::read_with_registered_worker(asset, pages, assets)
}

/// The in-process reader: the code the worker runs. It is not a product path: a host reads through the worker (`pdf_read`). One parse
/// (`extract_pdf_scope`) yields the evidence and the status of every requested page. A failed request, or a requested page that
/// was not read, is returned as an error whose messages are fixed text (`withheld_request_error`, `withheld_read_failure`). No
/// partial evidence leaves this function.
pub(crate) fn read_pdf_in_process(asset: &AssetRef, pages: &[u32], assets: &dyn AssetStore) -> DltResult<designllm_core::PdfReading> {
    let extraction = designllm_sources::extract_pdf_scope(asset, pages, assets).map_err(withheld_request_error)?;
    if !extraction.failures.is_empty() {
        return Err(withheld_read_failure(failures_as_error(&extraction.failures)));
    }
    Ok(designllm_core::PdfReading {
        evidence: extraction.evidence,
        read_pages: extraction.coverage.physical_page_indices,
        page_reports: extraction.pages.into_iter().map(|page| designllm_core::PdfPageReport {
            physical_page_index: page.physical_page_index,
            read: page.read,
            evidence_count: page.evidence_count,
            diagnostics: page.diagnostics,
        }).collect(),
        page_count: Some(extraction.page_count),
    })
}

/// Test-only: the in-process reader, so that a test compares a worker's result with the reader it must match. Not a product path.
#[doc(hidden)]
pub fn read_pdf_in_process_for_tests(asset: &AssetRef, pages: &[u32], assets: &dyn AssetStore) -> DltResult<designllm_core::PdfReading> {
    read_pdf_in_process(asset, pages, assets)
}

/// The failed pages of one request as one error. The strict extraction is not run again: the codes come from the failures that
/// the first parse produced. A failure without a diagnostic makes the lists unequal, and `withhold` then names no page.
fn failures_as_error(failures: &[designllm_sources::PageFailure]) -> DltError {
    let Some(first) = failures.first() else {
        return DltError::new(ErrorCode::Internal, "the PDF reader reported a failed read without a failed page");
    };
    let mut error = DltError::new(first.error.code.clone(), "the PDF reader reported failed pages");
    error.affected_ids = failures.iter().map(|failure| failure.physical_page_index.to_string()).collect();
    error.diagnostics = failures.iter().filter_map(|failure| failure.error.diagnostics.first().cloned()).collect();
    error
}

/// The message of every error that leaves the reader path without a named page. It carries no reader text.
const REQUEST_MESSAGE: &str = "the PDF could not be read; the reader's detail is withheld";

/// An error from before any page was read: the document does not load, the request is refused, or the asset does not match its
/// reference. Parser text can be in its message, so the message becomes fixed text. The code, the affected pages and each
/// diagnostic's code and blocking flag are kept.
pub(crate) fn withheld_request_error(error: DltError) -> DltError {
    withhold(error, REQUEST_MESSAGE.to_string())
}

/// An error about requested pages that could not be read. Its message names the first affected page. Each diagnostic names its
/// page only when the lists line up, one diagnostic per affected page in the same order.
fn withheld_read_failure(error: DltError) -> DltError {
    let message = match error.affected_ids.first() {
        Some(first) => read_failure_message(first),
        None => REQUEST_MESSAGE.to_string(),
    };
    withhold(error, message)
}

/// The fixed text of a read failure that names its first page.
fn read_failure_message(first: &str) -> String {
    format!("physical page {first} could not be read; the reader's detail is withheld")
}

/// An error that came back from the worker process. The worker withheld it already, but the host does not take a worker's text on
/// trust: only the code, the affected pages (page indices only) and each diagnostic's code and blocking flag are kept. A message
/// survives only when it is one of this module's fixed texts, which is what the in-process reader sends, so a refusal that the
/// worker withheld comes back with the same bytes. Any other text, such as an operating-system message, becomes the request text.
pub(crate) fn withheld_worker_error(mut error: DltError) -> DltError {
    error.affected_ids.retain(|id| !id.is_empty() && id.bytes().all(|byte| byte.is_ascii_digit()));
    let page_text = error.affected_ids.first().map(|first| read_failure_message(first));
    let message = match page_text {
        Some(text) if error.message == text => text,
        _ => REQUEST_MESSAGE.to_string(),
    };
    withhold(error, message)
}

/// Keeps the code, the affected pages, and each diagnostic's code and blocking flag, and replaces every message with fixed text.
/// Reader messages can name fonts, streams or other details of the document, and an error can reach an agent client. The rule for
/// a diagnostic is `designllm_core::withheld_diagnostic`; this function adds the error's own fields.
fn withhold(error: DltError, message: String) -> DltError {
    let aligned = error.affected_ids.len() == error.diagnostics.len();
    let mut out = DltError::new(error.code, message);
    out.diagnostics = error
        .diagnostics
        .iter()
        .enumerate()
        .map(|(position, diagnostic)| {
            let text = match error.affected_ids.get(position) {
                Some(index) if aligned => format!("physical page {index}: reader detail withheld; see the code"),
                _ => "reader detail withheld; see the code".to_string(),
            };
            designllm_core::withheld_diagnostic(diagnostic, text)
        })
        .collect();
    out.affected_ids = error.affected_ids;
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failed_page_keeps_its_code_pages_and_blocking_flag_but_not_the_reader_text() {
        let mut error = DltError::new(ErrorCode::UnsupportedOperation, "physical page 2 could not be read: Type3 font F9 is not supported");
        error.affected_ids = vec!["2".to_string()];
        error.diagnostics.push(Diagnostic {
            code: "PDF_UNSUPPORTED_FONT".into(),
            message: "physical page 2 could not be read: Type3 font F9 is not supported".into(),
            node_id: None,
            blocking: true,
            params: Default::default(),
        });
        let out = withheld_read_failure(error);
        assert_eq!(out.code, ErrorCode::UnsupportedOperation);
        assert_eq!(out.affected_ids, vec!["2".to_string()]);
        assert_eq!(out.diagnostics.len(), 1);
        assert_eq!(out.diagnostics[0].code, "PDF_UNSUPPORTED_FONT");
        assert!(out.diagnostics[0].blocking);
        assert!(out.message.contains("physical page 2"), "{}", out.message);
        assert!(!out.message.contains("F9") && !out.diagnostics[0].message.contains("F9"), "{out:?}");
    }

    #[test]
    fn failed_pages_are_paired_with_their_diagnostics_by_position_and_keep_no_reader_text() {
        let mut error = DltError::new(ErrorCode::InvalidArgument, "physical page 1 could not be read: font F9 is not embedded");
        error.affected_ids = vec!["1".to_string(), "3".to_string()];
        error.diagnostics = vec![
            Diagnostic {
                code: "PDF_FONT_MISSING".into(),
                message: "physical page 1 could not be read: font F9 is not embedded".into(),
                node_id: None,
                blocking: true,
                params: Default::default(),
            },
            Diagnostic {
                code: "PDF_CONTENT_TRUNCATED".into(),
                message: "physical page 3 could not be read: stream S7 ends early".into(),
                node_id: None,
                blocking: true,
                params: Default::default(),
            },
        ];
        let out = withheld_read_failure(error);
        assert_eq!(out.affected_ids, vec!["1".to_string(), "3".to_string()]);
        let pairs: Vec<(&str, &str)> = out.diagnostics.iter().map(|d| (d.code.as_str(), d.message.as_str())).collect();
        assert_eq!(
            pairs,
            vec![
                ("PDF_FONT_MISSING", "physical page 1: reader detail withheld; see the code"),
                ("PDF_CONTENT_TRUNCATED", "physical page 3: reader detail withheld; see the code"),
            ]
        );
        let shown = format!("{out:?}");
        assert!(!shown.contains("F9") && !shown.contains("S7"), "{shown}");
    }

    #[test]
    fn failed_page_lists_that_do_not_line_up_name_no_page() {
        let mut error = DltError::new(ErrorCode::InvalidArgument, "physical page 1 could not be read: F9");
        error.affected_ids = vec!["1".to_string(), "3".to_string()];
        error.diagnostics = vec![Diagnostic {
            code: "PDF_FONT_MISSING".into(),
            message: "F9".into(),
            node_id: None,
            blocking: true,
            params: Default::default(),
        }];
        let out = withheld_read_failure(error);
        assert_eq!(out.diagnostics.len(), 1);
        assert_eq!(out.diagnostics[0].message, "reader detail withheld; see the code");
        assert!(!format!("{out:?}").contains("F9"));
    }

    #[test]
    fn failed_page_request_errors_keep_codes_and_drop_parser_text() {
        let mut error = DltError::new(ErrorCode::InvalidArgument, "PDF parse error: SECRET token at offset 12");
        error.diagnostics.push(Diagnostic {
            code: "PDF_PARSE_ERROR".into(),
            message: "PDF parse error: SECRET token at offset 12".into(),
            node_id: None,
            blocking: true,
            params: Default::default(),
        });
        let out = withheld_request_error(error);
        assert_eq!(out.code, ErrorCode::InvalidArgument);
        assert_eq!(out.message, REQUEST_MESSAGE);
        assert!(out.affected_ids.is_empty());
        assert_eq!(out.diagnostics.len(), 1);
        assert_eq!(out.diagnostics[0].code, "PDF_PARSE_ERROR");
        assert!(out.diagnostics[0].blocking);
        assert_eq!(out.diagnostics[0].message, "reader detail withheld; see the code");
        assert!(!format!("{out:?}").contains("SECRET"));
    }

    #[test]
    fn failed_page_request_that_does_not_parse_returns_no_parser_text() {
        struct OneAsset {
            asset: AssetRef,
            bytes: Vec<u8>,
        }
        impl AssetStore for OneAsset {
            fn put(&self, _bytes: &[u8], _mime: &str) -> DltResult<AssetRef> {
                Err(DltError::new(ErrorCode::Io, "read-only test store"))
            }
            fn read(&self, asset: &AssetRef) -> DltResult<Vec<u8>> {
                if *asset == self.asset {
                    Ok(self.bytes.clone())
                } else {
                    Err(DltError::new(ErrorCode::NotFound, "asset not found"))
                }
            }
            fn lookup(&self, _hash: &str) -> DltResult<AssetRef> {
                Err(DltError::new(ErrorCode::NotFound, "read-only test store"))
            }
        }

        let bytes = b"%PDF-1.4 this is not a document".to_vec();
        let asset = AssetRef { sha256: sha256(&bytes), mime: "application/pdf".into(), bytes: bytes.len() as u64 };
        let store = OneAsset { asset: asset.clone(), bytes };
        let error = read_pdf_in_process(&asset, &[], &store).unwrap_err();
        assert_eq!(error.message, REQUEST_MESSAGE);
        assert!(!error.diagnostics.is_empty(), "{error:?}");
        assert!(error.diagnostics.iter().all(|d| d.message == "reader detail withheld; see the code"), "{error:?}");
        assert!(!format!("{error:?}").contains("PDF parse error"), "{error:?}");
    }

    #[test]
    fn failed_page_failures_become_one_error_with_each_page_and_its_code() {
        let failure = |index: u32, code: &str, text: &str| designllm_sources::PageFailure {
            physical_page_index: index,
            error: {
                let mut error = DltError::new(ErrorCode::UnsupportedOperation, format!("physical page {index} could not be read: {text}"));
                error.diagnostics.push(Diagnostic {
                    code: code.into(),
                    message: format!("physical page {index} could not be read: {text}"),
                    node_id: None,
                    blocking: true,
                    params: Default::default(),
                });
                error
            },
        };
        let failures = vec![failure(1, "PDF_FONT_MISSING", "font F9"), failure(3, "PDF_UNSUPPORTED_FONT", "Type3 S7")];
        let out = withheld_read_failure(failures_as_error(&failures));
        assert_eq!(out.code, ErrorCode::UnsupportedOperation);
        assert_eq!(out.affected_ids, vec!["1".to_string(), "3".to_string()]);
        assert!(out.message.starts_with("physical page 1 could not be read"), "{}", out.message);
        let pairs: Vec<(&str, &str)> = out.diagnostics.iter().map(|d| (d.code.as_str(), d.message.as_str())).collect();
        assert_eq!(
            pairs,
            vec![
                ("PDF_FONT_MISSING", "physical page 1: reader detail withheld; see the code"),
                ("PDF_UNSUPPORTED_FONT", "physical page 3: reader detail withheld; see the code"),
            ]
        );
        let shown = format!("{out:?}");
        assert!(!shown.contains("F9") && !shown.contains("S7"), "{shown}");
    }

    #[test]
    fn failed_page_list_that_is_empty_is_refused_with_a_fixed_error() {
        let out = withheld_read_failure(failures_as_error(&[]));
        assert_eq!(out.code, ErrorCode::Internal);
        assert_eq!(out.message, REQUEST_MESSAGE);
        assert!(out.affected_ids.is_empty() && out.diagnostics.is_empty());
    }
}

fn configure_workspace(workspace: &mut designllm_core::Workspace, pdf_reader: designllm_core::PdfReadHandler) {
    // `source.extract_pdf` goes to `pdf_reader`, which takes precedence over a legacy PDF handler, so none is registered here
    // (task 34c, decision 11): the legacy `sources::extract_pdf` could not be reached from a product workspace.
    workspace.set_csv_handler(designllm_figures::import_csv);
    workspace.set_pdf_read_handler(pdf_reader);
    workspace.set_csv_declared_handler(designllm_figures::import_csv_declared);
    workspace.set_dataset_editor(designllm_figures::edit_dataset);
}

pub fn create_workspace(root: &Path) -> DltResult<designllm_core::Workspace> {
    let mut workspace = designllm_core::Workspace::open(root, engine_adapters(), render_provider()?)?;
    configure_workspace(&mut workspace, pdf_read);
    Ok(workspace)
}

/// Test-only: like `create_workspace`, but `source.extract_pdf` reads in this process instead of in the worker. The product never
/// calls it. `create_workspace` refuses PDF reads unless a host has registered its worker (fail-closed), and a test process that
/// reads PDFs through a workspace uses this constructor.
#[doc(hidden)]
pub fn create_workspace_with_in_process_pdf_reader_for_tests(root: &Path) -> DltResult<designllm_core::Workspace> {
    let mut workspace = designllm_core::Workspace::open(root, engine_adapters(), render_provider()?)?;
    configure_workspace(&mut workspace, read_pdf_in_process);
    Ok(workspace)
}

pub fn list_migration_checkpoints(root: &Path) -> DltResult<Vec<designllm_core::MigrationCheckpointSummary>> {
    designllm_core::list_migration_checkpoints(root)
}

/// Restores a registered checkpoint into a new workspace folder using the product's full engine
/// set. The returned workspace holds the destination lock until the caller drops it.
pub fn restore_migration_checkpoint(
    source_root: &Path,
    backup_id: &str,
    destination_root: &Path,
) -> DltResult<designllm_core::RestoredWorkspace> {
    let mut restored = designllm_core::Workspace::restore_backup_to_new_folder(
        source_root,
        backup_id,
        destination_root,
        engine_adapters(),
        render_provider()?,
    )?;
    configure_workspace(&mut restored.workspace, pdf_read);
    Ok(restored)
}

pub fn default_workspace() -> DltResult<PathBuf> {
    if let Some(path) = std::env::var_os("DESIGNLLM_WORKSPACE") { return Ok(PathBuf::from(path)); }
    let base = std::env::var_os("LOCALAPPDATA").ok_or_else(|| DltError::invalid("LOCALAPPDATA is not set; pass --workspace"))?;
    Ok(PathBuf::from(base).join("DesignLLMTool/workspaces/default"))
}

pub fn web_runtime_directory() -> Option<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(path) = std::env::var_os("DESIGNLLM_WEBVIEW2_DIR") { candidates.push(PathBuf::from(path)); }
    if let Ok(exe) = std::env::current_exe() { if let Some(parent) = exe.parent() { candidates.push(parent.join("runtime/webview2")); } }
    if cfg!(debug_assertions) { candidates.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../runtime/webview2")); }
    for path in candidates {
        if path.join("msedgewebview2.exe").is_file() { return Some(path); }
        if let Ok(entries) = std::fs::read_dir(&path) {
            for entry in entries.flatten() {
                if entry.path().join("msedgewebview2.exe").is_file() { return Some(entry.path()); }
            }
        }
    }
    None
}

/// The mock agent (`designllm-host-harness`) a UI-test window gives the host it starts (task 69). Set once by the window's
/// launch, read by every `ensure_host` of this process (the first start and `다시 연결`).
static UI_TEST_MOCK_AGENT: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

pub fn set_ui_test_mock_agent(path: PathBuf) {
    let _ = UI_TEST_MOCK_AGENT.set(path);
}

/// Whether this process was started for UI tests (`DESIGNLLM_UI_TEST_INPUT` = 1, on or offscreen): only then may a host take
/// a mock agent.
pub fn ui_test_launch() -> bool {
    std::env::var("DESIGNLLM_UI_TEST_INPUT").is_ok_and(|value| matches!(value.trim(), "1" | "on" | "offscreen"))
}

/// A mock agent path a UI test may name: an absolute local path of an existing `designllm-host-harness` executable. The
/// host runs it for `mock` jobs, so nothing else is taken.
pub fn ui_test_mock_agent_path(raw: &std::ffi::OsStr) -> DltResult<PathBuf> {
    let text = raw.to_str().ok_or_else(|| DltError::invalid("the mock agent path is not Unicode"))?;
    let path = PathBuf::from(text);
    let name = path.file_name().map(|name| name.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
    if !(name == "designllm-host-harness" || name == "designllm-host-harness.exe") {
        return Err(DltError::invalid("the mock agent must be the designllm-host-harness test executable"));
    }
    // The agent path rules of a dialog answer (66c): a local drive (not a network drive), no UNC or device path, no link or
    // reparse point from the root to the file, a regular file.
    if let Some(reason) = designllm_contracts::local_path::agent_file_refusal(text) {
        return Err(DltError::invalid(format!("the mock agent: {reason}")));
    }
    Ok(path)
}

pub fn ensure_host(workspace: &Path) -> DltResult<()> {
    // GUI/smoke launch is authorized to create its workspace. MCP remains attach-only.
    std::fs::create_dir_all(workspace)?;
    let client = cli_client(workspace);
    match client.call("system.hello", serde_json::json!({})) {
        Ok(_) => return Ok(()),
        Err(error) if error.code == ErrorCode::HostNotRunning => {}
        Err(error) => return Err(error),
    }
    std::fs::create_dir_all(workspace.join("logs"))?;
    let log = std::fs::OpenOptions::new().create(true).append(true).open(workspace.join("logs/host.log"))?;
    // A private per-launch directory prevents a stale error from another host being reported here.
    let capture = HostStartupCapture::new(&workspace.join("logs"))?;
    let mut command = std::process::Command::new(std::env::current_exe()?);
    command.arg("--host").arg("--workspace").arg(workspace)
        .args(UI_TEST_MOCK_AGENT.get().map(|path| [std::ffi::OsStr::new("--ui-test-mock-agent"), path.as_os_str()]).into_iter().flatten())
        .env("DESIGNLLM_HOST_STARTUP_ERROR_PATH", &capture.error_path)
        .stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(log);
    #[cfg(windows)] {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000); // CREATE_NO_WINDOW
    }
    let mut child = command.spawn()?;
    // Same rules as designllm_host::ensure_host, but this launcher keeps the host's stderr in logs/host.log.
    // Exit 3 means another process owns the workspace (main maps WORKSPACE_LOCKED to 3): keep trying to
    // join that host until the deadline. Any other exit is a failed start and is reported at once.
    let mut locked_elsewhere = false;
    for _ in 0..300 {
        match client.call("system.hello", serde_json::json!({})) {
            Ok(_) => return Ok(()),
            Err(error) if error.code == ErrorCode::HostNotRunning => {}
            Err(error) => return Err(error),
        }
        if !locked_elsewhere {
            if let Some(exit) = child.try_wait()? {
                if exit.code() == Some(3) {
                    locked_elsewhere = true;
                } else {
                    if let Some(mut error) = capture.read_error() {
                        error.message.push_str(&format!("; see {}", workspace.join("logs/host.log").display()));
                        return Err(error);
                    }
                    return Err(DltError::new(ErrorCode::HostNotRunning, format!("Host exited ({exit}); see {}", workspace.join("logs/host.log").display())));
                }
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    if locked_elsewhere {
        return Err(DltError::new(ErrorCode::WorkspaceLocked, "Another process owns this workspace and its host did not accept a connection within 30 seconds"));
    }
    Err(DltError::new(ErrorCode::HostNotRunning, "Host did not become ready within 30 seconds"))
}

struct HostStartupCapture { directory: PathBuf, error_path: PathBuf }

impl HostStartupCapture {
    fn new(logs: &Path) -> DltResult<Self> {
        let directory = logs.join(format!(".host-startup-{}", new_id()));
        std::fs::create_dir(&directory)?;
        Ok(Self { error_path: directory.join("error.json"), directory })
    }

    fn read_error(&self) -> Option<DltError> {
        let metadata = std::fs::metadata(&self.error_path).ok()?;
        if metadata.len() > 65_536 { return None; }
        serde_json::from_slice(&std::fs::read(&self.error_path).ok()?).ok()
    }
}

impl Drop for HostStartupCapture {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.error_path);
        let _ = std::fs::remove_dir(&self.directory);
    }
}

#[cfg(test)]
mod startup_capture_tests {
    use super::*;

    #[test]
    fn captures_only_this_launchs_structured_error() {
        let logs = std::env::temp_dir().join(format!("designllm-host-capture-{}", new_id()));
        std::fs::create_dir(&logs).unwrap();
        let first = HostStartupCapture::new(&logs).unwrap();
        let second = HostStartupCapture::new(&logs).unwrap();
        let error = DltError::new(ErrorCode::SchemaUnsupported, "workspace is newer than this app");
        std::fs::write(&first.error_path, serde_json::to_vec(&error).unwrap()).unwrap();
        assert_eq!(first.read_error().unwrap().code, ErrorCode::SchemaUnsupported);
        assert!(second.read_error().is_none());
        std::fs::write(&second.error_path, vec![b'x'; 65_537]).unwrap();
        assert!(second.read_error().is_none());
        drop(first);
        drop(second);
        std::fs::remove_dir(logs).unwrap();
    }
}

/// Keeps a managed host alive while this GUI process runs. The host leaves after `idle_grace` (5 s)
/// without clients once any client has connected, and `ensure_host` itself is such a client, so a
/// slow window start could otherwise lose the host before the desktop's first call. The thread pings
/// every 2 s, never starts a host, and ends with the process.
pub fn start_gui_heartbeat(workspace: &Path) {
    let workspace = workspace.to_path_buf();
    let _ = std::thread::Builder::new().name("designllm-gui-heartbeat".into()).spawn(move || {
        let client = cli_client(&workspace);
        loop {
            let _ = client.call("system.hello", serde_json::json!({}));
            std::thread::sleep(std::time::Duration::from_secs(2));
        }
    });
}

pub fn prepare_smoke_canvas(workspace: &Path) -> DltResult<ArtifactSummary> {
    let client = cli_client(workspace);
    let hello = client.call("system.hello", serde_json::json!({}))?;
    let workspace_id = hello.get("workspace_id").and_then(|v| v.as_str()).ok_or_else(|| DltError::new(ErrorCode::Internal, "Host omitted workspace_id"))?.to_owned();
    let create = CreateArtifactRequest { request_id: new_id(), actor: "smoke".into(), options: CreateRequest { kind: ArtifactKind::Canvas, title: "공동 편집 검증".into(), size: Some(Size::slide()), template: None } };
    let summary: ArtifactSummary = serde_json::from_value(client.call("artifact.create", serde_json::to_value(create).unwrap())?).map_err(|e| DltError::invalid(e.to_string()))?;
    let artifact: Artifact = serde_json::from_value(client.call("artifact.inspect", serde_json::json!({"artifact_id":summary.id}))?).map_err(|e| DltError::invalid(e.to_string()))?;
    let page_id = match &artifact.body { ArtifactBody::Canvas(body) => body.pages.first().map(|p| p.id.clone()), _ => None }.ok_or_else(|| DltError::invalid("Canvas has no page"))?;
    let nodes = vec![
        Node { id: new_id(), kind: NodeKind::Rectangle, bounds: Rect { x: 25*EMU_PER_MM, y: 27*EMU_PER_MM, width: 210*EMU_PER_MM, height: 110*EMU_PER_MM }, text: String::new(), style: Style { fill: "#EAF0FA".into(), stroke: "#B8C7E4".into(), ..Style::default() }, asset: None, children: vec![], layout: None, interactions: vec![], variants: Default::default(), ext: Default::default() },
        Node { id: new_id(), kind: NodeKind::Text, bounds: Rect { x: 38*EMU_PER_MM, y: 42*EMU_PER_MM, width: 180*EMU_PER_MM, height: 22*EMU_PER_MM }, text: "사용자와 AI가 같은 문서를 편집합니다".into(), style: Style { font_size: 24*EMU_PER_PT, bold: true, color: "#24416D".into(), ..Style::default() }, asset: None, children: vec![], layout: None, interactions: vec![], variants: Default::default(), ext: Default::default() },
        Node { id: new_id(), kind: NodeKind::Text, bounds: Rect { x: 38*EMU_PER_MM, y: 79*EMU_PER_MM, width: 180*EMU_PER_MM, height: 30*EMU_PER_MM }, text: "문서 · 슬라이드 · 논문 figure".into(), style: Style { font_size: 17*EMU_PER_PT, color: "#46576F".into(), ..Style::default() }, asset: None, children: vec![], layout: None, interactions: vec![], variants: Default::default(), ext: Default::default() },
    ];
    let change = ChangeSet { request_id: new_id(), actor: "smoke".into(), workspace_id, artifact_id: summary.id.clone(), base_revision: summary.revision, allowed_intents: [Intent::Content].into_iter().collect(), job_ref: None, read_dependencies: vec![], operations: nodes.into_iter().map(|node| Operation::AddNode { page_id: page_id.clone(), parent_id: None, node }).collect() };
    let result: ChangeResult = serde_json::from_value(client.call("changes.apply", serde_json::to_value(change).unwrap())?).map_err(|e| DltError::invalid(e.to_string()))?;
    Ok(ArtifactSummary { revision: result.revision, dirty: true, ..summary })
}

#[cfg(test)]
mod ui_test_mock_agent_tests {
    use super::*;

    /// Task 69: the host takes a mock agent only as an explicit path to the test harness, never another program.
    #[test]
    fn only_an_existing_local_harness_is_taken_as_the_mock_agent() {
        // A UNC path, a missing file and another program are refused by name or by the agent path rules.

        let dir = std::env::temp_dir().join(format!("designllm-mock-agent-{}", new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let harness = dir.join("designllm-host-harness.exe");
        let other = dir.join("cmd.exe");
        std::fs::write(&harness, b"x").unwrap();
        std::fs::write(&other, b"x").unwrap();
        assert_eq!(ui_test_mock_agent_path(harness.as_os_str()).unwrap(), harness);
        for bad in [other.clone(), dir.join("designllm-host-harness-missing.exe"), PathBuf::from("designllm-host-harness.exe"), PathBuf::from(r"\\server\share\designllm-host-harness.exe")] {
            assert!(ui_test_mock_agent_path(bad.as_os_str()).is_err(), "{}", bad.display());
        }
        assert_eq!(ui_test_mock_agent_path(dir.join("designllm-host-harness.exe-not").as_os_str()).unwrap_err().code, ErrorCode::InvalidArgument);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
