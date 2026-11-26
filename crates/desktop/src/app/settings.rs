//! The window's own settings, kept in the workspace folder (`.gui/settings.json`): starred projects and
//! the last runner choice (GUI_DESIGN §3: "별표는 작업공간의 GUI 설정에 저장한다", "마지막 선택을 기억한다").
//! The file is the window's alone; the core never reads it. A missing or unreadable file means defaults.

use std::{
    collections::{BTreeMap, BTreeSet},
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
    /// The UI language (`ko`, `en`); missing or unknown is Korean (`i18n::Language::from_code`).
    pub language: String,
    /// Per Figure artifact id: the output profile chosen for it and its PNG export DPI. They are the window's choices, not
    /// the document's, so they live here and come back when the Figure opens again.
    pub figure_outputs: BTreeMap<String, FigureOutput>,
}

/// What this window changed in its settings and has not written yet. A write reads the file again and puts only these into
/// it, so two windows on one workspace keep each other's choices (another window's stars, languages or Figure outputs). A
/// change stays here until its write succeeds, so a failed write is tried again by the next save.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct SettingsChanges {
    /// Projects whose star this window toggled.
    pub starred: BTreeSet<String>,
    pub runner: bool,
    pub language: bool,
    /// Figures whose output profile this window chose, and those whose export DPI it chose: each field is merged on its own,
    /// so a DPI chosen in one window keeps the profile another window chose.
    pub figure_profiles: BTreeSet<String>,
    pub figure_dpis: BTreeSet<String>,
}

impl SettingsChanges {
    fn is_empty(&self) -> bool {
        *self == SettingsChanges::default()
    }
}

/// The output choices of one Figure (`GuiSettings::figure_outputs`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub(super) struct FigureOutput {
    pub profile: Option<String>,
    pub dpi: u32,
}

/// What the window says while one of its settings changes could not be written. It is said again on every document the
/// window shows until a save succeeds, so opening a document (which clears the notices) does not hide it.
pub(super) const SETTINGS_SAVE_FAILED: &str = "화면 설정을 저장하지 못했습니다";

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

/// The settings on disk for a merge: the defaults when there is no file yet. A file that cannot be read (locked, no access)
/// is an error, so a write never replaces settings it could not read. A file that is read but is no settings JSON (hand-edited,
/// cut short) would fail every later save; it is moved aside to `settings.json.bad-<id>`, kept byte for byte, and the merge
/// starts from the defaults. The second value names the moved file. Callers hold the settings lock.
fn load_for_merge(path: &Path) -> std::io::Result<(GuiSettings, Option<PathBuf>)> {
    match std::fs::read_to_string(path) {
        Ok(text) => match serde_json::from_str(&text) {
            Ok(settings) => Ok((settings, None)),
            Err(_) => {
                let name = path.file_name().map_or_else(String::new, |name| name.to_string_lossy().into_owned());
                let aside = path.with_file_name(format!("{name}.bad-{}", designllm_contracts::new_id()));
                std::fs::rename(path, &aside)?;
                Ok((GuiSettings::default(), Some(aside)))
            }
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok((GuiSettings::default(), None)),
        Err(error) => Err(error),
    }
}

/// What the window says when the settings file could not be parsed and was moved aside (`{file}` is its new name, as it is).
pub(super) const SETTINGS_MOVED_ASIDE: &str = "화면 설정 파일을 읽을 수 없어 '{file}'(으)로 옮기고 기본값에서 다시 저장했습니다";

/// How long a window waits for another window's settings write before it gives up (its changes stay pending). The deadline
/// holds on every path: a lock that never frees never keeps the window waiting longer.
pub(super) const LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(2);

/// The workspace's settings lock, `settings.json.lock` next to the settings, held by an open handle across the read, the merge
/// and the replace. On Windows the handle is opened with no sharing, so the system itself keeps a second window out and frees
/// the lock when the handle closes, also when the window's process ends; the file stays and is never deleted, so no window
/// ever takes a lock another one holds, however long it holds it.
pub(super) struct SettingsLock {
    _handle: std::fs::File,
    #[cfg(not(windows))]
    path: PathBuf,
}

impl SettingsLock {
    pub(super) fn acquire(settings: &Path) -> std::io::Result<Self> {
        if let Some(parent) = settings.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let lock = settings.with_extension("json.lock");
        let started = std::time::Instant::now();
        loop {
            match Self::open(&lock) {
                Ok(lock) => return Ok(lock),
                Err(error) if Self::busy(&error) => {
                    if started.elapsed() >= LOCK_WAIT {
                        return Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "the settings are being written by another window"));
                    }
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                Err(error) => return Err(error),
            }
        }
    }

    #[cfg(windows)]
    fn open(lock: &Path) -> std::io::Result<Self> {
        use std::os::windows::fs::OpenOptionsExt;
        let handle = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).share_mode(0).open(lock)?;
        Ok(Self { _handle: handle })
    }

    /// Another window holds the lock: a sharing violation (Windows error 32), or the access refusal Windows gives meanwhile.
    #[cfg(windows)]
    fn busy(error: &std::io::Error) -> bool {
        error.raw_os_error() == Some(32) || error.kind() == std::io::ErrorKind::PermissionDenied
    }

    /// Elsewhere the lock is a file only one window can create; it is removed by the window that made it, never by another.
    #[cfg(not(windows))]
    fn open(lock: &Path) -> std::io::Result<Self> {
        let handle = std::fs::OpenOptions::new().write(true).create_new(true).open(lock)?;
        Ok(Self { _handle: handle, path: lock.to_owned() })
    }

    #[cfg(not(windows))]
    fn busy(error: &std::io::Error) -> bool {
        error.kind() == std::io::ErrorKind::AlreadyExists
    }
}

#[cfg(not(windows))]
impl Drop for SettingsLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Reads the settings of `path`, lets `change` put this window's changes in, and writes them, all under the workspace's
/// settings lock, so that two windows saving at once each keep the other's change.
/// The returned path names a settings file that could not be parsed and was moved aside before the write.
pub(super) fn update(path: &Path, change: impl FnOnce(&mut GuiSettings)) -> std::io::Result<Option<PathBuf>> {
    let _lock = SettingsLock::acquire(path)?;
    let (mut settings, moved) = load_for_merge(path)?;
    change(&mut settings);
    store(path, &settings)?;
    Ok(moved)
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
        self.home.project_folder = crate::project_folder(workspace);
        let path = settings_path(workspace);
        let settings = load(&path);
        self.home.starred = settings.starred.clone();
        if matches!(settings.runtime.as_str(), "claude" | "agy" | "codex") {
            self.ai_runtime = settings.runtime.clone();
        }
        if matches!(settings.effort.as_str(), "minimal" | "low" | "medium" | "high" | "xhigh" | "max") {
            self.ai_effort = settings.effort.clone();
        }
        self.ai_model = settings.model.clone();
        self.home.workspace_info.seen_backups = settings.seen_store_backups;
        self.language = crate::i18n::Language::from_code(&settings.language);
        // A Figure opened later starts with the profile and DPI chosen for it before (`board::EditorView`).
        for (artifact_id, output) in &settings.figure_outputs {
            let view = self.editor_views.entry(artifact_id.clone()).or_default();
            view.figure_output_profile = output.profile.clone();
            if (72..=1200).contains(&output.dpi) {
                view.figure_export_dpi = output.dpi;
            }
        }
        self.figure_outputs = settings.figure_outputs;
        crate::i18n::set_language(self.language);
        self.settings_path = Some(path);
        self.drafts = super::drafts::DraftBook::open(super::drafts::drafts_path(workspace));
        self
    }

    /// Writes this window's unsaved choices to the settings file (nothing happens without a workspace path).
    pub(super) fn save_settings(&mut self) {
        match self.write_settings() {
            Ok(Some(moved)) => self.tell_settings_moved(&moved),
            Ok(None) => {}
            Err(_) => self.set_error(String::from(SETTINGS_SAVE_FAILED)),
        }
    }

    /// Says that an unparsable settings file was moved aside to `moved` (its file name shown as it is).
    fn tell_settings_moved(&mut self, moved: &Path) {
        let file = moved.file_name().map_or_else(String::new, |name| name.to_string_lossy().into_owned());
        self.notices.warning(SETTINGS_MOVED_ASIDE.replace("{file}", &file), std::time::Instant::now());
    }

    /// Saves a star this window toggled on `artifact_id`.
    pub(super) fn save_star(&mut self, artifact_id: &str) {
        self.settings_changes.starred.insert(artifact_id.to_owned());
        self.save_settings();
    }

    /// Saves the runner choice (runtime, model, effort).
    pub(super) fn save_runner(&mut self) {
        self.settings_changes.runner = true;
        self.save_settings();
    }

    /// Writes the confirmed migration notices (`seen_store_backups`). A failure is told once a session: the notice comes
    /// back at the next start, and a later confirmation does not raise the error again.
    pub(super) fn save_seen_store_backups(&mut self) {
        match self.write_settings() {
            Ok(Some(moved)) => self.tell_settings_moved(&moved),
            Ok(None) => {}
            Err(_) => {
                if !std::mem::replace(&mut self.home.workspace_info.save_failure_told, true) {
                    self.set_error(String::from("이주 알림 확인을 저장하지 못했습니다. 다음에 열 때 다시 보일 수 있습니다"));
                }
            }
        }
    }

    /// Writes the settings file as it is on disk now with this window's changes put in (`SettingsChanges`): another window's
    /// choices stay. Confirmed migration notices are joined, never dropped. The changes count as saved once the write succeeds.
    /// The returned path names an unparsable settings file that was moved aside before the write.
    fn write_settings(&mut self) -> std::io::Result<Option<PathBuf>> {
        let Some(path) = self.settings_path.clone() else {
            return Ok(None);
        };
        let changes = self.settings_changes.clone();
        let moved = update(&path, |settings| {
            for artifact_id in &changes.starred {
                if self.home.starred.contains(artifact_id) {
                    settings.starred.insert(artifact_id.clone());
                } else {
                    settings.starred.remove(artifact_id);
                }
            }
            if changes.runner {
                settings.runtime = self.ai_runtime.clone();
                settings.model = self.ai_model.clone();
                settings.effort = self.ai_effort.clone();
            }
            if changes.language {
                settings.language = self.language.code().to_owned();
            }
            for artifact_id in changes.figure_profiles.union(&changes.figure_dpis) {
                let Some(mine) = self.figure_outputs.get(artifact_id) else { continue };
                let saved = settings.figure_outputs.entry(artifact_id.clone()).or_insert_with(|| mine.clone());
                if changes.figure_profiles.contains(artifact_id) {
                    saved.profile = mine.profile.clone();
                }
                if changes.figure_dpis.contains(artifact_id) {
                    saved.dpi = mine.dpi;
                }
            }
            settings.seen_store_backups.extend(self.home.workspace_info.seen_backups.iter().cloned());
        })?;
        // Only what was written is cleared: a change made meanwhile (none, on one thread) would stay.
        if self.settings_changes == changes {
            self.settings_changes = SettingsChanges::default();
        }
        Ok(moved)
    }

    /// Whether a settings change of this window is still to be written.
    pub(super) fn settings_unsaved(&self) -> bool {
        !self.settings_changes.is_empty()
    }

    /// Keeps the output profile and the DPI the view of Figure `artifact_id` has now. Only an output profile choice (applied,
    /// or given at creation) calls it, and a profile brings its DPI, so both are written even when one equals what this window
    /// read before. Exports never write the settings: an export's DPI is that export's.
    pub(super) fn remember_figure_output(&mut self, artifact_id: &str) {
        let Some(view) = self.editor_views.get(artifact_id) else { return };
        let output = FigureOutput { profile: view.figure_output_profile.clone(), dpi: view.figure_export_dpi };
        self.settings_changes.figure_profiles.insert(artifact_id.to_owned());
        self.settings_changes.figure_dpis.insert(artifact_id.to_owned());
        self.figure_outputs.insert(artifact_id.to_owned(), output);
        self.save_settings();
    }

    /// Switches the UI language: the next frame draws in it, and it is kept with the other settings.
    pub(crate) fn set_language(&mut self, language: crate::i18n::Language) {
        // The same language again is skipped only once it is written: a failed save is tried again.
        if self.language == language && !self.settings_changes.language {
            return;
        }
        self.language = language;
        crate::i18n::set_language(language);
        self.settings_changes.language = true;
        self.save_settings();
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
            language: "en".into(),
            figure_outputs: [("fig".to_owned(), FigureOutput { profile: Some("high_res_print".into()), dpi: 600 })].into_iter().collect(),
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
