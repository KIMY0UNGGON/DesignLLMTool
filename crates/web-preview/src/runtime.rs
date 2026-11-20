//! WebView2 runtime selection.
//!
//! Order:
//! 1. `DESIGNLLM_WEBVIEW2_DIR` (absolute, non-UNC folder containing `msedgewebview2.exe`). Invalid means an error,
//!    never a silent fallback.
//! 2. `<exe folder>/runtime/webview2` containing `msedgewebview2.exe`. Same rule: invalid means an error.
//! 3. Installed Evergreen runtime, labelled `InstalledDevFallback`. It is for development only.
//!    `DESIGNLLM_WEBVIEW2_REQUIRE_FIXED=1` turns this fallback off, so packaged builds can prove they ship their own runtime.

use designllm_contracts::{DltError, DltResult, ErrorCode};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

pub const WEBVIEW2_DIR_ENV: &str = "DESIGNLLM_WEBVIEW2_DIR";
pub const WEBVIEW2_USER_DATA_ENV: &str = "DESIGNLLM_WEBVIEW2_USER_DATA";
pub const WEBVIEW2_REQUIRE_FIXED_ENV: &str = "DESIGNLLM_WEBVIEW2_REQUIRE_FIXED";
/// Fixed Version runtime folder shipped next to the program.
pub const RUNTIME_RELATIVE_DIR: &str = "runtime/webview2";
/// File that marks a folder as a WebView2 runtime.
pub const RUNTIME_EXECUTABLE: &str = "msedgewebview2.exe";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeSource {
    /// `DESIGNLLM_WEBVIEW2_DIR`.
    EnvironmentVariable,
    /// `runtime/webview2` next to the program.
    ExecutableFolder,
    /// The installed Evergreen runtime. Development only, and never proof of packaging.
    InstalledDevFallback,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeSelection {
    pub source: RuntimeSource,
    /// The fixed-version folder passed to WebView2. None for the installed runtime.
    pub fixed_folder: Option<PathBuf>,
}

impl RuntimeSelection {
    pub fn is_fixed_version(&self) -> bool {
        self.fixed_folder.is_some()
    }
}

fn runtime_error(message: impl Into<String>) -> DltError {
    DltError::new(ErrorCode::UnsupportedOperation, message)
}

fn check_fixed_folder(folder: &Path, origin: &str) -> DltResult<PathBuf> {
    let text = folder.to_string_lossy();
    if text.starts_with(r"\\") {
        return Err(runtime_error(format!(
            "{origin} points to a network path ({text}); WebView2 Fixed Version cannot run from UNC paths"
        )));
    }
    if !folder.is_absolute() {
        return Err(runtime_error(format!("{origin} must be an absolute path, got {text}")));
    }
    if !folder.join(RUNTIME_EXECUTABLE).is_file() {
        return Err(runtime_error(format!(
            "{origin} ({text}) does not contain {RUNTIME_EXECUTABLE}; the WebView2 Fixed Version runtime is incomplete"
        )));
    }
    Ok(folder.to_path_buf())
}

/// Pure selection rule. `env_dir` is the value of `DESIGNLLM_WEBVIEW2_DIR`, `exe_dir` the folder of the program,
/// and `require_fixed` the value of `DESIGNLLM_WEBVIEW2_REQUIRE_FIXED`.
pub fn select_runtime(env_dir: Option<&OsStr>, exe_dir: Option<&Path>, require_fixed: bool) -> DltResult<RuntimeSelection> {
    if let Some(value) = env_dir {
        if !value.is_empty() {
            let folder = check_fixed_folder(Path::new(value), WEBVIEW2_DIR_ENV)?;
            return Ok(RuntimeSelection { source: RuntimeSource::EnvironmentVariable, fixed_folder: Some(folder) });
        }
    }
    if let Some(dir) = exe_dir {
        let candidate = dir.join(RUNTIME_RELATIVE_DIR);
        if candidate.exists() {
            let folder = check_fixed_folder(&candidate, "runtime folder next to the program")?;
            return Ok(RuntimeSelection { source: RuntimeSource::ExecutableFolder, fixed_folder: Some(folder) });
        }
    }
    if require_fixed {
        return Err(runtime_error(format!(
            "no WebView2 Fixed Version runtime: set {WEBVIEW2_DIR_ENV} or place the runtime in {RUNTIME_RELATIVE_DIR} next to the program"
        )));
    }
    Ok(RuntimeSelection { source: RuntimeSource::InstalledDevFallback, fixed_folder: None })
}

/// Reads the environment and the program folder, then applies `select_runtime`.
pub fn resolve_runtime() -> DltResult<RuntimeSelection> {
    let env_dir = std::env::var_os(WEBVIEW2_DIR_ENV);
    let exe_dir = std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf));
    let require = std::env::var(WEBVIEW2_REQUIRE_FIXED_ENV).map(|v| v == "1").unwrap_or(false);
    select_runtime(env_dir.as_deref(), exe_dir.as_deref(), require)
}

/// Why this program cannot open WebView2 previews, or `None` when it can (A-110): the runtime `resolve_runtime` selects must
/// be usable, a Fixed Version folder with its executable or, for the development fallback, an installed WebView2 runtime.
/// Nothing is started; callers ask once and keep the answer.
pub fn runtime_unavailable_reason() -> Option<String> {
    match resolve_runtime() {
        Err(error) => Some(error.message),
        Ok(selection) if selection.fixed_folder.is_none() && !installed_runtime_present() => Some(format!(
            "no WebView2 runtime: no Fixed Version runtime in {RUNTIME_RELATIVE_DIR} next to the program and none installed"
        )),
        Ok(_) => None,
    }
}

#[cfg(windows)]
fn installed_runtime_present() -> bool {
    use webview2_com::Microsoft::Web::WebView2::Win32::GetAvailableCoreWebView2BrowserVersionString;
    use windows::core::{PCWSTR, PWSTR};
    let mut version = PWSTR::null();
    let asked = unsafe { GetAvailableCoreWebView2BrowserVersionString(PCWSTR::null(), &mut version) };
    let present = asked.is_ok() && !version.is_null();
    if !version.is_null() {
        unsafe { windows::Win32::System::Com::CoTaskMemFree(Some(version.0 as *const std::ffi::c_void)) };
    }
    present
}

#[cfg(not(windows))]
fn installed_runtime_present() -> bool {
    false
}

/// User-data folder for WebView2. Never the default profile of another application.
pub fn user_data_folder() -> DltResult<PathBuf> {
    if let Some(value) = std::env::var_os(WEBVIEW2_USER_DATA_ENV) {
        if !value.is_empty() {
            return Ok(PathBuf::from(value));
        }
    }
    let base = std::env::var_os("LOCALAPPDATA")
        .ok_or_else(|| runtime_error("LOCALAPPDATA is not set; set DESIGNLLM_WEBVIEW2_USER_DATA"))?;
    Ok(PathBuf::from(base).join("DesignLLMTool").join("webview2"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_runtime(root: &Path) -> PathBuf {
        let folder = root.join("fixed");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join(RUNTIME_EXECUTABLE), b"stub").unwrap();
        folder
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("dlm-runtime-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn env_var_wins_over_the_program_folder_when_valid() {
        let root = scratch("env");
        let folder = fake_runtime(&root);
        let exe = root.join("app");
        std::fs::create_dir_all(exe.join(RUNTIME_RELATIVE_DIR)).unwrap();
        let selection = select_runtime(Some(folder.as_os_str()), Some(&exe), false).unwrap();
        assert_eq!(selection.source, RuntimeSource::EnvironmentVariable);
        assert_eq!(selection.fixed_folder.as_deref(), Some(folder.as_path()));
    }

    #[test]
    fn program_folder_is_used_when_no_env_var_is_set() {
        let root = scratch("exe");
        let exe = root.join("app");
        let runtime = exe.join(RUNTIME_RELATIVE_DIR);
        std::fs::create_dir_all(&runtime).unwrap();
        std::fs::write(runtime.join(RUNTIME_EXECUTABLE), b"stub").unwrap();
        let selection = select_runtime(None, Some(&exe), false).unwrap();
        assert_eq!(selection.source, RuntimeSource::ExecutableFolder);
        assert!(selection.is_fixed_version());
    }

    #[test]
    fn an_incomplete_fixed_folder_is_an_error_not_a_fallback() {
        let root = scratch("bad");
        let empty = root.join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        let error = select_runtime(Some(empty.as_os_str()), None, false).unwrap_err();
        assert_eq!(error.code, ErrorCode::UnsupportedOperation);
        assert!(error.message.contains("does not contain"));
    }

    #[test]
    fn unc_and_relative_paths_are_refused() {
        assert!(select_runtime(Some(OsStr::new(r"\\server\share\webview2")), None, false).is_err());
        assert!(select_runtime(Some(OsStr::new("relative/webview2")), None, false).is_err());
    }

    #[test]
    fn without_any_fixed_runtime_the_installed_runtime_is_a_labelled_dev_fallback() {
        let root = scratch("none");
        let selection = select_runtime(None, Some(&root), false).unwrap();
        assert_eq!(selection.source, RuntimeSource::InstalledDevFallback);
        assert_eq!(selection.fixed_folder, None);
    }

    #[test]
    fn require_fixed_turns_the_fallback_off() {
        let root = scratch("require");
        let error = select_runtime(None, Some(&root), true).unwrap_err();
        assert_eq!(error.code, ErrorCode::UnsupportedOperation);
        assert!(error.message.contains(WEBVIEW2_DIR_ENV));
    }
}
