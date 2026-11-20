//! The window's own settings, kept in the workspace folder (`.gui/settings.json`): starred projects and
//! the last runner choice (GUI_DESIGN §3: "별표는 작업공간의 GUI 설정에 저장한다", "마지막 선택을 기억한다").
//! The file is the window's alone; the core never reads it. A missing or unreadable file means defaults.

use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use super::DesktopApp;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub(super) struct GuiSettings {
    pub starred: BTreeSet<String>,
    pub runtime: String,
    pub model: String,
    pub effort: String,
    /// Backups (`backup_path` of `store_migrated`) whose migration notice the user confirmed, so it is not shown again.
    pub seen_store_backups: BTreeSet<String>,
}

/// Where the settings of `workspace` are kept.
pub(super) fn settings_path(workspace: &Path) -> PathBuf {
    workspace.join(".gui").join("settings.json")
}

pub(super) fn load(path: &Path) -> GuiSettings {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// Writes `text` through a temporary file, synced before it takes the place of the old file: a crash leaves the old file
/// or the new one, never half a file. The temporary file has a name of its own (two windows writing the same file never share
/// one), and it is removed when the write fails.
pub(super) fn write_atomic(path: &Path, text: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let name = path.file_name().map_or_else(String::new, |name| name.to_string_lossy().into_owned());
    let temporary = path.with_file_name(format!("{name}.{}.tmp", designllm_contracts::new_id()));
    let result = write_then_replace(&temporary, path, text);
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

fn write_then_replace(temporary: &Path, path: &Path, text: &str) -> std::io::Result<()> {
    {
        // A new file only: an existing one is never written through, and the handle is closed before the rename.
        let mut file = std::fs::File::create_new(temporary)?;
        std::io::Write::write_all(&mut file, text.as_bytes())?;
        file.sync_all()?;
    }
    std::fs::rename(temporary, path)
}

/// Writes the settings through a temporary file, so a crash never leaves half a file.
pub(super) fn store(path: &Path, settings: &GuiSettings) -> std::io::Result<()> {
    let text = serde_json::to_string_pretty(settings).map_err(std::io::Error::other)?;
    write_atomic(path, &text)
}

impl DesktopApp {
    /// Reads the workspace's GUI settings and keeps their path for later writes. The unsent drafts of the workspace
    /// come back here too, as recovered drafts (`drafts.rs`).
    pub(crate) fn with_workspace(mut self, workspace: &Path) -> Self {
        let path = settings_path(workspace);
        let settings = load(&path);
        self.home.starred = settings.starred.clone();
        if matches!(settings.runtime.as_str(), "claude" | "codex") {
            self.ai_runtime = settings.runtime.clone();
        }
        if matches!(settings.effort.as_str(), "minimal" | "low" | "medium" | "high" | "xhigh" | "max") {
            self.ai_effort = settings.effort.clone();
        }
        self.ai_model = settings.model.clone();
        self.home.workspace_info.seen_backups = settings.seen_store_backups;
        self.settings_path = Some(path);
        self.drafts = super::drafts::DraftBook::open(super::drafts::drafts_path(workspace));
        self
    }

    /// Writes the current choices to the settings file (nothing happens without a workspace path).
    pub(super) fn save_settings(&mut self) {
        if self.write_settings().is_err() {
            self.set_error(String::from("화면 설정을 저장하지 못했습니다"));
        }
    }

    /// Writes the confirmed migration notices (`seen_store_backups`). A failure is told once a session: the notice comes
    /// back at the next start, and a later confirmation does not raise the error again.
    pub(super) fn save_seen_store_backups(&mut self) {
        if self.write_settings().is_err() && !std::mem::replace(&mut self.home.workspace_info.save_failure_told, true) {
            self.set_error(String::from("이주 알림 확인을 저장하지 못했습니다. 다음에 열 때 다시 보일 수 있습니다"));
        }
    }

    /// Writes the settings as the window holds them (nothing happens without a workspace path).
    fn write_settings(&self) -> std::io::Result<()> {
        let Some(path) = self.settings_path.as_deref() else {
            return Ok(());
        };
        let settings = GuiSettings {
            starred: self.home.starred.clone(),
            runtime: self.ai_runtime.clone(),
            model: self.ai_model.clone(),
            effort: self.ai_effort.clone(),
            seen_store_backups: self.home.workspace_info.seen_backups.clone(),
        };
        store(path, &settings)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_round_trip_and_a_missing_file_gives_defaults() {
        let root = std::env::temp_dir().join(format!("designllm-gui-settings-{}", designllm_contracts::new_id()));
        let path = settings_path(&root);
        assert_eq!(load(&path), GuiSettings::default());
        let settings = GuiSettings {
            starred: ["a".to_owned(), "b".to_owned()].into_iter().collect(),
            runtime: "codex".into(),
            model: String::new(),
            effort: "max".into(),
            seen_store_backups: ["backups/a.db".to_owned()].into_iter().collect(),
        };
        store(&path, &settings).expect("written");
        assert_eq!(load(&path), settings);
        std::fs::write(&path, "not json").expect("overwritten");
        assert_eq!(load(&path), GuiSettings::default());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_settings_file_without_seen_backups_loads_with_none_seen() {
        let root = std::env::temp_dir().join(format!("designllm-gui-old-settings-{}", designllm_contracts::new_id()));
        let path = settings_path(&root);
        write_atomic(&path, r#"{"starred":["p"],"runtime":"claude","model":"","effort":""}"#).expect("written");
        let settings = load(&path);
        assert_eq!(settings.starred.len(), 1);
        assert!(settings.seen_store_backups.is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn write_atomic_replaces_an_existing_file_and_leaves_no_temporary_file() {
        let root = std::env::temp_dir().join(format!("designllm-gui-atomic-{}", designllm_contracts::new_id()));
        std::fs::create_dir_all(&root).expect("the folder");
        let path = root.join("drafts.json");
        std::fs::write(&path, "old text").expect("the old file");
        write_atomic(&path, "new text").expect("the replacement is written");
        assert_eq!(std::fs::read_to_string(&path).expect("the file"), "new text");
        let names: Vec<String> = std::fs::read_dir(&root)
            .expect("the folder")
            .map(|entry| entry.expect("an entry").file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["drafts.json".to_owned()], "the temporary file is gone after the rename");
        let _ = std::fs::remove_dir_all(&root);
    }
}
