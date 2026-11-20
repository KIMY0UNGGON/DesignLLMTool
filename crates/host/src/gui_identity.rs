//! The gui role is verified, not claimed. A `client_role: "gui"` hello is accepted only from the application
//! executable (`DesignLLMTool.exe` in the folder of the running host), and never from a process that runs inside
//! one of the host's job objects. Job processes run the host's own executable (the job's MCP bridge is
//! `DesignLLMTool --mcp`), so the executable check alone cannot tell them apart. Job membership can.

use std::path::{Path, PathBuf};

use designllm_bridge::pipe::Pipe;
use designllm_contracts::{DltError, DltResult};

use crate::jobs::JobSupervisor;

/// The application executable: `DesignLLMTool.exe`, next to the running host executable.
pub const APP_EXECUTABLE: &str = "DesignLLMTool.exe";

/// Verifies a gui hello on `pipe`. `configured` lists extra executables allowed to hold the gui role (tests and
/// development only; the product sets none). The refusal names the rule that failed.
pub(crate) fn verify_gui_client(pipe: &Pipe, jobs: &JobSupervisor, configured: &[PathBuf]) -> DltResult<()> {
    let refuse = |why: String| DltError::invalid(format!("client_role gui is refused: {why}"));
    let pid = pipe
        .client_process_id()
        .map_err(|e| refuse(format!("the client process could not be identified ({})", e.message)))?;
    let image = pipe
        .client_image_path()
        .map_err(|e| refuse(format!("the client image could not be read ({})", e.message)))?;
    if !allowed_executables(configured).iter().any(|allowed| same_executable(&image, allowed)) {
        return Err(refuse("the client is not the application executable".into()));
    }
    match jobs.owns_pid(pid) {
        Ok(false) => Ok(()),
        Ok(true) => Err(refuse("this process is a runtime of a job or one of its descendants".into())),
        Err(e) => Err(refuse(format!("the job processes could not be checked ({})", e.message))),
    }
}

/// The executables that may hold the gui role: `configured`, then the application next to the host.
pub(crate) fn allowed_executables(configured: &[PathBuf]) -> Vec<PathBuf> {
    let mut allowed = configured.to_vec();
    if let Some(dir) = std::env::current_exe().ok().as_deref().and_then(Path::parent) {
        allowed.push(dir.join(APP_EXECUTABLE));
    }
    allowed
}

/// True when both paths name the same file. Both sides are resolved first, so links and letter case do not matter,
/// and a missing file never matches.
pub(crate) fn same_executable(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a.to_string_lossy().to_lowercase() == b.to_string_lossy().to_lowercase(),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_same_file_matches_in_any_letter_case_and_a_missing_file_never_matches() {
        let exe = std::env::current_exe().expect("test executable");
        let shouted = PathBuf::from(exe.to_string_lossy().to_uppercase());
        assert!(same_executable(&exe, &shouted));
        assert!(!same_executable(&exe, Path::new(r"C:\no\such\folder\DesignLLMTool.exe")));
    }
}
