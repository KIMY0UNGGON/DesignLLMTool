//! A newer DesignLLMTool release, told to the agent once per MCP connection. The check itself runs in the application
//! (`designllm_application::update_check`) on a background thread; it fills the slot at most once, and the server adds the
//! notice to the first successful tool result after that. Nothing is downloaded or replaced.

use std::sync::{Arc, OnceLock};

use serde_json::{json, Value};

/// A release newer than this build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateInfo {
    /// The newer version, without the tag's `v` (for example `0.1.2`).
    pub latest: String,
    /// This build's version.
    pub current: String,
    /// The release page.
    pub html_url: String,
}

/// Set at most once, by the background update check; empty while no newer release is known.
pub type UpdateSlot = Arc<OnceLock<UpdateInfo>>;

impl UpdateInfo {
    /// The text block added to a tool result.
    pub fn notice_text(&self) -> String {
        format!(
            "DesignLLMTool {} is available (this is {}). To update: in the DesignLLMTool clone run \"git pull\" then \"./install.sh\" (macOS/Linux) or \"powershell -ExecutionPolicy Bypass -File .\\install.ps1\" (Windows), and restart the AI client. {}",
            self.latest, self.current, self.html_url
        )
    }

    /// The `update_available` field added to the result's `structuredContent`.
    pub fn structured(&self) -> Value {
        json!({
            "latest_version": self.latest,
            "current_version": self.current,
            "url": self.html_url,
            "how_to_update": "In the DesignLLMTool clone: git pull, then ./install.sh (macOS/Linux) or install.ps1 (Windows); restart the AI client.",
        })
    }
}
