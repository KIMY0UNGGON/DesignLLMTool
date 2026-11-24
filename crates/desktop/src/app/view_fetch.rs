use super::*;
use super::messages::{engine_warning_message, workspace_notice_message};

impl DesktopApp {
    pub(super) fn show_engine_diagnostics(&mut self, value: &Value) {
        let diagnostics: Vec<Diagnostic> = value
            .get("diagnostics")
            .cloned()
            .and_then(|value| serde_json::from_value(value).ok())
            .unwrap_or_default();
        if let Some(message) = engine_warning_message(&diagnostics) {
            self.notices.warning(message, Instant::now());
        }
    }

    pub(super) fn update_workspace_notices(&mut self, workspace_id: &str, diagnostics: Option<Vec<Diagnostic>>) {
        let Some(diagnostics) = diagnostics else { return };
        self.home.workspace_info.read(&diagnostics);
        let mut keys = Vec::new();
        let mut messages = Vec::new();
        for diagnostic in diagnostics.into_iter().filter(|diagnostic| !diagnostic.blocking) {
            let message = workspace_notice_message(&diagnostic);
            let fingerprint = serde_json::to_string(&diagnostic).unwrap_or_default();
            let key = format!("{}:{}", workspace_id, sha256(fingerprint.as_bytes()));
            if self.acknowledged_workspace_notices.contains(&key) {
                continue;
            }
            // The migration notice comes once per backup, until the user confirms it; the core keeps reporting it (MCP,
            // records) and the workspace information keeps showing the backup.
            if !self.home.workspace_info.notice_due(&key, &diagnostic) {
                continue;
            }
            if !messages.contains(&message) {
                messages.push(message);
            }
            keys.push(key);
        }
        if !messages.is_empty() {
            self.notices.workspace_warning(messages.join("\n"), keys, Instant::now());
        }
    }

    /// The user confirmed the workspace notice: it stays away for this session, and a migration notice in it for good
    /// (`seen_store_backups`).
    pub(super) fn acknowledge_workspace_notice(&mut self) {
        let keys = self.notices.acknowledge_workspace();
        if self.home.workspace_info.confirm(&keys) {
            self.save_seen_store_backups();
        }
        self.acknowledged_workspace_notices.extend(keys);
    }

    pub(super) fn activate(&mut self, summary: ArtifactSummary) {
        if self.inline_text_edit.is_some() {
            self.set_error("텍스트 편집을 먼저 끝내 주세요".into());
            return;
        }
        if self.video_insert.as_ref().is_some_and(|pending| pending.worker_cancel.is_some()) {
            let _ = self.cancel_canvas_video_insert();
            self.set_error("동영상 검사 또는 추출이 취소되고 정리될 때까지 작업물 전환을 기다리세요".into());
            return;
        }
        // Figure work held back from another document is asked about first (figure_close.rs); the switch waits for the answer.
        if !self.auto_following_figure_job_only() && self.ask_before_switch(&summary) {
            return;
        }
        self.show_artifact(summary);
    }

    /// Shows `summary` as the open document. `activate` calls it once nothing is left to ask about.
    pub(super) fn show_artifact(&mut self, summary: ArtifactSummary) {
        if self.active.as_ref().is_none_or(|active| active.id != summary.id) { self.close_document_preview(); }
        self.dismiss_agent_notice_for(&summary.id);
        let left_start_note = self.unanswered_start_note(&summary);
        self.release_waits_of_left_documents(&summary.id);
        self.pending_switch = None;
        self.cancel_video_insert_before_switch();
        self.pending_image_replacement = None;
        if self.active.as_ref().is_none_or(|active| active.id != summary.id) {
            self.leave_drafts_for_switch();
            self.layer_find_query.clear();
            self.layer_find_open = false;
            self.layer_find_focus = false;
            self.canvas_find_reveal = None;
        }
        self.active = Some(summary);
        self.place_jobs(None);
        self.forget_figure_ai_list_of_other_document();
        self.reset_flow_find_search();
        self.flow_find_cached_page = None;
        self.notices.clear_transient();
        if let Some(message) = left_start_note {
            self.notices.warning(message, Instant::now());
        } else if self.active.as_ref().is_some_and(|active| self.late_start_waits(&active.id)) {
            // The answer of a start the window no longer waits for is still due: the words say so again on return.
            self.notices.warning(figure_close::LATE_START_WAIT, Instant::now());
        }
        self.agent_batches.clear();
        self.reservations.clear();
        self.user_reservations.clear();
        self.reservations_for = None;
        self.refusal_ids.clear();
        self.held_refusal_id = None;
        self.lint_warnings.clear();
        if let Some(id) = self.active.as_ref().map(|item| item.id.clone()) {
            self.call("targets.list", json!({ "artifact_id": id }), PendingAction::Reservations);
        }
        self.artifact = None;
        self.context = None;
        self.render = None;
        self.reset_board();
        self.screen = Screen::Editor;
        self.refusal = None;
        self.view_id = new_id();
        self.agent_opened_view();
        self.discover_listed_jobs();
        self.ui.user_selection = None;
        self.page_index = 0;
        self.total_pages = 0;
        self.set_selected_ids(Vec::new());
        self.local_selection = None;
        self.dataset_edit = None;
        self.csv_draft = None;
        self.figure_form = None;
        self.connect_from = None;
        self.connect_to = None;
        self.reselect = None;
        self.select_after_commit = None;
        self.select_after_commit_ticket = None;
        self.select_after_commit_guard = None;
        self.active_web_path = None;
        self.text_target = None;
        self.text_draft.clear();
        self.token_panel = properties::TokenPanelState::default();
        if self.active.as_ref().is_some_and(|active| active.kind == ArtifactKind::Canvas) {
            self.fetch_design_systems();
        }
        // The core knows a view only after its first transform; reads of an unknown view are refused.
        let initial_transform = ViewTransform {
            id: new_id(),
            dpi: RenderProfile::default().dpi,
            zoom_milli: 1000,
            offset_x_micropx: 0,
            offset_y_micropx: 0,
            crop: None,
        };
        self.call(
            "view.transform.set",
            json!({
                "artifact_id": self.active.as_ref().map(|item| item.id.clone()),
                "view_id": self.view_id,
                "page_index": 0,
                "transform": initial_transform.clone(),
            }),
            PendingAction::Transform,
        );
        self.fetch_context();
        self.fetch_inspect();
        self.fetch_render(0);
    }

    /// Web code has no page scene; its preview is the separate web window, not a rendered page.
    pub(super) fn has_page_scene(&self) -> bool {
        self.active
            .as_ref()
            .is_some_and(|active| active.kind != ArtifactKind::WebCode)
    }

    pub(super) fn fetch_context(&mut self) {
        let Some(active) = &self.active else {
            return;
        };
        self.call(
            "context.read",
            json!({
                "artifact_id": active.id,
                "view_id": self.view_id,
                "include_render": false,
            }),
            PendingAction::Context,
        );
    }

    pub(super) fn fetch_inspect(&mut self) {
        let Some(active) = &self.active else {
            return;
        };
        self.call(
            "artifact.inspect",
            json!({ "artifact_id": active.id }),
            PendingAction::Inspect,
        );
    }

    pub(super) fn fetch_design_systems(&mut self) {
        self.call("design_system.list", json!({}), PendingAction::DesignSystemList);
    }

    pub(super) fn fetch_linked_design_system_source(&mut self) {
        let current = self.artifact.as_ref().and_then(|consumer| {
            consumer.design_system.as_ref().map(|reference| {
                (consumer.id.clone(), reference.artifact_id.clone(), reference.revision)
            })
        });
        let Some((consumer_id, source_id, source_revision)) = current else {
            self.token_panel.clear_shared_source();
            return;
        };
        let state = &self.token_panel;
        let same_snapshot = state.shared_source_id.as_deref() == Some(source_id.as_str())
            && state.shared_source_revision == Some(source_revision);
        if same_snapshot && (state.shared_source_loading || state.shared_source_loaded) {
            return;
        }
        let generation = self.token_panel.begin_shared_source_load(&source_id, source_revision);
        let ticket = self.call(
            "artifact.inspect",
            json!({ "artifact_id": source_id, "revision": source_revision }),
            PendingAction::DesignSystemSource { consumer_id, source_id, source_revision, generation },
        );
        if ticket.is_none() {
            self.token_panel.fail_shared_source(generation);
        }
    }

    /// Asks again for `page_index` at the resolution the board shows now. The board also requests the
    /// pages it draws; this one covers a page that has not been drawn yet after a load or a change.
    pub(super) fn fetch_render(&mut self, page_index: u32) {
        if self.has_page_scene() {
            self.requested_pages.remove(&page_index);
            let dpi = self.board_dpi;
            self.request_page(page_index, dpi);
        }
    }

    pub(super) fn refresh_active(&mut self) {
        self.call("workspace.list", json!({}), PendingAction::WorkspaceList);
        self.refresh_active_contents();
    }

    pub(super) fn refresh_active_contents(&mut self) {
        self.fetch_context();
        self.fetch_inspect();
        if self.active.is_some() {
            self.fetch_render(self.page_index.min(self.total_pages.saturating_sub(1)));
        }
    }

    /// A message for something that really failed. It replaces the one on screen and goes away
    /// with the situation that caused it (see `Notices`).
    pub(super) fn raise_error(&mut self, origin: Option<&str>, message: String) {
        if self.smoke_output.is_some() && self.smoke_failure.is_none() {
            self.smoke_failure = Some(format!("desktop startup: {message}"));
        }
        self.notices.error(message, origin, Instant::now());
    }

    pub(super) fn set_error(&mut self, message: String) {
        self.raise_error(None, message);
    }

    /// Whether the window shows revision `revision` of `artifact_id`. An answer about another document or revision is stale.
    pub(super) fn shows_revision(&self, artifact_id: &str, revision: u64) -> bool {
        self.active.as_ref().is_some_and(|active| active.id == artifact_id && active.revision == revision)
    }

    /// Asks the core for the pixels of a rendered image of the open document `artifact_id` (`asset.read`), answered to `action`.
    pub(super) fn send_asset_read(&mut self, asset: AssetRef, artifact_id: String, action: PendingAction) -> DltResult<()> {
        if self.host_lost() {
            return Ok(());
        }
        let ticket = self.next_ticket;
        self.next_ticket += 1;
        self.rpc.read_asset(ticket, asset)?;
        let in_flight = self.pending.len();
        if let Some(test) = self.ui_test.as_mut() { test.perf.sent(ticket, "asset.read", in_flight); }
        self.pending.insert(ticket, PendingRequest {
            action,
            method: "asset.read".into(),
            artifact_id: Some(artifact_id),
            require_active: true,
            apply: None,
            ui_run: None,
        });
        Ok(())
    }
}
