//! The workspace information (68e), under the workspace name of the home top bar: the migration backups of the open
//! workspace (path, the store version it came from, the least version that may restore it) and the warnings about them.
//! The migration notice itself is shown once per backup: the backup is remembered (`seen_store_backups`) when the user
//! confirms the notice, and it stays listed here. The two warnings (`store_recovery_warning`, `store_backup_missing`)
//! are notices every time.

use std::collections::{BTreeMap, BTreeSet};

use designllm_contracts::Diagnostic;
use eframe::egui::{self, Ui};
use serde_json::Value;

use crate::{theme, widgets};

/// The caution of every migration backup (core `store_migrated`).
const BACKUP_CAUTION: &str = "이 백업은 현재 버전 이상의 앱에서 검증된 복원 절차로만 사용하세요. 이전 버전으로 열면 일부 내용이 사라질 수 있습니다.";

/// One migration backup of the workspace (`store_migrated` params).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct MigrationBackup {
    pub path: String,
    pub from_version: Option<i64>,
    pub restore_min_version: Option<i64>,
}

#[derive(Debug, Default)]
pub(super) struct WorkspaceInfo {
    /// Backups whose migration notice the user confirmed: read from the settings once, at start (`with_workspace`).
    pub seen_backups: BTreeSet<String>,
    /// Notice key → backup, for the migration notices on screen and not confirmed yet.
    shown: BTreeMap<String, String>,
    /// The backups and warnings of the last listing.
    pub backups: Vec<MigrationBackup>,
    pub warnings: Vec<String>,
    /// A failed write of `seen_backups` was told this session.
    pub save_failure_told: bool,
}

impl WorkspaceInfo {
    pub fn is_empty(&self) -> bool {
        self.backups.is_empty() && self.warnings.is_empty()
    }

    /// Takes the backups and warnings of a listing's `workspace_notices`.
    pub fn read(&mut self, diagnostics: &[Diagnostic]) {
        self.backups.clear();
        self.warnings.clear();
        for diagnostic in diagnostics {
            match diagnostic.code.as_str() {
                "store_migrated" => {
                    if let Some(path) = migrated_backup(diagnostic) {
                        self.backups.push(MigrationBackup {
                            path: path.to_owned(),
                            from_version: diagnostic.params.get("from_store_version").and_then(Value::as_i64),
                            restore_min_version: diagnostic.params.get("restore_min_store_version").and_then(Value::as_i64),
                        });
                    }
                }
                "store_recovery_warning" | "store_backup_missing" if !self.warnings.contains(&diagnostic.message) => {
                    self.warnings.push(diagnostic.message.clone());
                }
                _ => {}
            }
        }
    }

    /// Whether the notice `diagnostic` (shown under `key`) is still due. A migration notice whose backup was confirmed is
    /// not; one that is due waits for its confirmation (`confirm`).
    pub fn notice_due(&mut self, key: &str, diagnostic: &Diagnostic) -> bool {
        let Some(backup) = migrated_backup(diagnostic) else {
            return true;
        };
        if self.seen_backups.contains(backup) {
            return false;
        }
        self.shown.insert(key.to_owned(), backup.to_owned());
        true
    }

    /// The notice of `keys` was confirmed: its migration backups are seen. Returns whether one is new (to be saved).
    pub fn confirm(&mut self, keys: &[String]) -> bool {
        let mut changed = false;
        for key in keys {
            if let Some(backup) = self.shown.remove(key) {
                changed |= self.seen_backups.insert(backup);
            }
        }
        changed
    }
}

/// The backup of a `store_migrated` notice (`backup_path`). Another code, or a notice without it, has none.
fn migrated_backup(diagnostic: &Diagnostic) -> Option<&str> {
    if diagnostic.code != "store_migrated" {
        return None;
    }
    diagnostic.params.get("backup_path").and_then(Value::as_str)
}

/// `원래 버전 1 · 복원 최소 버전 3`, without the part the core did not give.
fn versions_line(backup: &MigrationBackup) -> String {
    let parts: Vec<String> = [("원래 버전", backup.from_version), ("복원 최소 버전", backup.restore_min_version)]
        .into_iter()
        .filter_map(|(name, version)| version.map(|version| format!("{name} {version}")))
        .collect();
    parts.join(" · ")
}

/// The popover under the workspace name: each backup (path 13 `ink`, versions 12 `ink-3`), the caution, the warnings.
pub(super) fn show(ui: &mut Ui, palette: &theme::Palette, info: &WorkspaceInfo) {
    egui::Frame::new().inner_margin(egui::Margin::symmetric(8, 6)).show(ui, |ui| {
        ui.spacing_mut().item_spacing = egui::vec2(0.0, 2.0);
        for backup in &info.backups {
            ui.add(egui::Label::new(egui::RichText::new(&backup.path).font(theme::font(theme::Type::Body)).color(palette.ink)).wrap());
            let versions = versions_line(backup);
            if !versions.is_empty() {
                ui.add(egui::Label::new(egui::RichText::new(versions).font(theme::font(theme::Type::Caption)).color(palette.ink_3)).wrap());
            }
            ui.add_space(6.0);
        }
        ui.spacing_mut().item_spacing = egui::vec2(0.0, 6.0);
        if !info.backups.is_empty() {
            widgets::callout(ui, palette, BACKUP_CAUTION, &[], true);
        }
        for warning in &info.warnings {
            widgets::callout(ui, palette, warning, &[], false);
        }
    });
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use designllm_contracts::{new_id, Diagnostic};
    use eframe::egui::Context;
    use serde_json::{json, Value};

    use super::super::{settings, test_support::*, DesktopApp, PendingAction};
    use crate::rpc::RpcWorker;

    fn notice(code: &str, message: String, params: Value) -> Diagnostic {
        let params = params.as_object().map(|object| object.iter().map(|(key, value)| (key.clone(), value.clone())).collect()).unwrap_or_default();
        Diagnostic { code: code.into(), message, node_id: None, blocking: false, params }
    }

    /// `store_migrated` as the core words it (`routes.rs` `workspace_notices`).
    fn migrated(path: &str) -> Diagnostic {
        notice(
            "store_migrated",
            format!("작업공간을 새 형식으로 업데이트했습니다. 복구용 데이터베이스 백업: {path}. 이 백업은 현재 버전 이상의 앱에서 검증된 복원 절차로만 사용하세요. 이전 버전으로 열면 일부 내용이 사라질 수 있습니다."),
            json!({ "from_store_version": 1, "to_store_version": 3, "backup_path": path, "restore_min_store_version": 3, "older_core_compatible": false }),
        )
    }

    fn unprotected(path: &str) -> Diagnostic {
        notice(
            "store_recovery_warning",
            format!("이 백업 파일에는 구 버전 Core의 직접 열기를 막는 보호 표식이 없습니다. 현재 버전 이상의 앱에서 검증된 새 폴더 복원 경로만 사용하세요. 이전 버전으로 수동 열면 일부 내용이 사라질 수 있습니다: {path}"),
            json!({ "backup_path": path, "restore_min_store_version": 3 }),
        )
    }

    fn missing(path: &str) -> Diagnostic {
        notice(
            "store_backup_missing",
            format!("복구용 데이터베이스 백업을 찾을 수 없습니다: {path}. 이 백업을 사용할 수 없으며, 이전 버전 앱으로 복원하면 일부 내용이 사라질 수 있습니다."),
            json!({ "from_store_version": 1, "backup_path": path, "restore_min_store_version": 3 }),
        )
    }

    fn recovered() -> Diagnostic {
        notice("pending_saves_recovered", "시작 중 저장 복구를 실행했습니다. 임시 파일 정리 1개, 저장 완료 확인 0건.".into(), json!({ "removed_pending_temps": 1, "recovered_saves": 0 }))
    }

    /// A window started on the workspace folder `root`, with its settings (`with_workspace`), as the app starts.
    fn start(root: &Path) -> Harness {
        let (rpc, replies, work) = RpcWorker::detached();
        Harness { app: DesktopApp::new(rpc, "작업공간".into(), None, None).with_workspace(root), replies, work, ctx: Context::default() }
    }

    /// Answers a `workspace.list` the window sent, the way the host answers it (`payload.rs` `workspace_listed`).
    fn list(harness: &mut Harness, requests: &[Request], notices: &[Diagnostic]) {
        harness.answer(requests, "workspace.list", Ok(json!({ "workspace_id": "w1", "artifacts": [], "workspace_notices": notices })));
    }

    /// The window lists the workspace again (after a create, an event…) and gets `notices`.
    fn relist(harness: &mut Harness, notices: &[Diagnostic]) {
        harness.app.call("workspace.list", json!({}), PendingAction::WorkspaceList).expect("the list is sent");
        let requests = harness.sent();
        list(harness, &requests, notices);
    }

    fn saved_seen(root: &Path) -> Vec<String> {
        settings::load(&settings::settings_path(root)).seen_store_backups.into_iter().collect()
    }

    fn shown(harness: &Harness) -> String {
        harness.message().map(|(_, text)| text).unwrap_or_default()
    }

    #[test]
    fn migration_notice_waits_for_confirmation_and_stays_away_after_a_restart() {
        let root = std::env::temp_dir().join(format!("designllm-migration-notice-{}", new_id()));
        let mut harness = start(&root);
        let startup = harness.sent();
        list(&mut harness, &startup, &[migrated("backups/a.db"), recovered()]);
        let first = shown(&harness);
        assert!(first.contains("새 형식으로") && first.contains("backups/a.db") && first.contains("저장 복구"), "{first}");
        assert!(saved_seen(&root).is_empty(), "showing the notice is not confirming it");

        // Listed again before the confirmation, with the recovery gone: the migration text stays.
        relist(&mut harness, &[migrated("backups/a.db")]);
        let again = shown(&harness);
        assert!(again.contains("새 형식으로") && again.contains("backups/a.db"), "{again}");
        relist(&mut harness, &[migrated("backups/a.db")]);
        assert_eq!(shown(&harness), again);

        harness.app.acknowledge_workspace_notice();
        assert!(harness.message().is_none());
        assert_eq!(saved_seen(&root), vec!["backups/a.db".to_owned()], "the confirmation is saved");
        // The window keeps what it read at start: the settings file is not read again for each listing.
        std::fs::write(settings::settings_path(&root), "{}").expect("the settings file is reset outside the window");
        relist(&mut harness, &[migrated("backups/a.db")]);
        assert!(harness.message().is_none());
        harness.app.save_settings();
        assert_eq!(saved_seen(&root), vec!["backups/a.db".to_owned()], "a later settings save keeps the confirmation");

        // A restart: the same backup brings no notice; the two warnings still come, and the information keeps the backup.
        let mut harness = start(&root);
        let startup = harness.sent();
        list(&mut harness, &startup, &[migrated("backups/a.db"), unprotected("backups/a.db"), missing("backups/b.db")]);
        let warnings = shown(&harness);
        assert!(!warnings.contains("새 형식으로"), "{warnings}");
        assert!(warnings.contains("보호 표식이 없습니다") && warnings.contains("찾을 수 없습니다: backups/b.db"), "{warnings}");
        let info = &harness.app.home.workspace_info;
        assert_eq!(info.backups.len(), 1);
        assert_eq!((info.backups[0].path.as_str(), info.backups[0].from_version, info.backups[0].restore_min_version), ("backups/a.db", Some(1), Some(3)));
        assert_eq!(info.warnings.len(), 2);
        harness.app.acknowledge_workspace_notice();
        let mut harness = start(&root);
        let startup = harness.sent();
        list(&mut harness, &startup, &[migrated("backups/a.db"), unprotected("backups/a.db"), missing("backups/b.db")]);
        let warnings = shown(&harness);
        assert!(warnings.contains("보호 표식이 없습니다") && warnings.contains("찾을 수 없습니다"), "the warnings come at every start: {warnings}");

        // Another backup has a notice of its own.
        relist(&mut harness, &[migrated("backups/a.db"), migrated("backups/c.db")]);
        let other = shown(&harness);
        assert!(other.contains("backups/c.db") && !other.contains("backups/a.db"), "{other}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_failed_save_of_the_confirmation_is_told_once() {
        let root = std::env::temp_dir().join(format!("designllm-migration-notice-unsaved-{}", new_id()));
        std::fs::create_dir_all(&root).expect("the workspace folder");
        // `.gui` is a file: the settings cannot be written.
        std::fs::write(root.join(".gui"), "").expect("a file in the way");
        let mut harness = start(&root);
        let startup = harness.sent();
        list(&mut harness, &startup, &[migrated("backups/a.db")]);
        harness.app.acknowledge_workspace_notice();
        assert!(shown(&harness).contains("저장하지 못했습니다"), "{}", shown(&harness));
        harness.app.notices.clear_transient();
        relist(&mut harness, &[migrated("backups/a.db")]);
        assert!(harness.message().is_none(), "the confirmed notice stays away this session");
        relist(&mut harness, &[migrated("backups/a.db"), migrated("backups/b.db")]);
        assert!(shown(&harness).contains("backups/b.db"));
        harness.app.acknowledge_workspace_notice();
        assert!(harness.message().is_none(), "the failure is not told again");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_workspace_name_opens_the_backups_and_their_warnings() {
        let mut harness = Harness::new();
        let ctx = Context::default();
        crate::theme::install(&ctx);
        let home = |app: &mut DesktopApp| drawn_text(&ctx.run(screen(1.0, 1440.0, 900.0), |ctx| app.draw_home(ctx)));
        harness.app.home.smoke_open = Some("workspace".into());
        let plain = home(&mut harness.app);
        assert!(!plain.iter().any(|text| text.contains("backups/")), "no backups, no information");

        harness.app.update_workspace_notices("w1", Some(vec![migrated("backups/a.db"), missing("backups/b.db")]));
        harness.app.acknowledge_workspace_notice();
        harness.app.home.smoke_open = Some("workspace".into());
        home(&mut harness.app);
        let shown = home(&mut harness.app);
        for expected in ["backups/a.db", "원래 버전 1 · 복원 최소 버전 3", super::BACKUP_CAUTION] {
            assert!(shown.iter().any(|text| text == expected), "{expected:?} in {shown:?}");
        }
        assert!(shown.iter().any(|text| text.starts_with("복구용 데이터베이스 백업을 찾을 수 없습니다: backups/b.db")), "{shown:?}");
    }
}
