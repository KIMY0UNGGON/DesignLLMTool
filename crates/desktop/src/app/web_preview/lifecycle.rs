use super::*;
use crate::theme;
use std::time::Duration;

#[derive(Default)]
pub(in crate::app) struct PreviewState {
    pub(super) generation: u64,
    pub(super) child_generation: u64,
    pub(super) deferred_exit_notice: Option<String>,
    #[cfg(windows)]
    pub(super) focus_guard: Option<super::FocusGuard>,
    artifact_id: Option<String>,
    displayed_revision: u64,
    observed_revision: u64,
    changed_at: Option<Instant>,
    requested_revision: Option<u64>,
    refresh_ticket: Option<u64>,
    explicit_ticket: Option<u64>,
    refresh_failures: u8,
    opening: bool,
}

#[cfg(all(test, windows))]
#[path = "lifecycle_tests.rs"]
mod tests;

impl DesktopApp {
    pub(super) fn collect_exited_preview_before_request(&mut self) {
        if self.web_preview_child.as_mut().is_some_and(|child| matches!(child.try_wait(), Ok(Some(_)))) {
            self.poll_web_preview_child();
        }
    }

    pub(in crate::app) fn draw_web_mapping_entry(&mut self, ui: &mut Ui, board_rect: Rect) {
        let Some(document) = self.web_document() else { return };
        let path = self.open_web_path(document).to_owned();
        let palette = theme::palette(ui.ctx());
        let center = board_rect.center() + egui::vec2(0.0, theme::CONTROL_HEIGHT + 8.0);
        let rect = Rect::from_center_size(center, Vec2::new(200.0, theme::CONTROL_HEIGHT));
        let mut child = ui.new_child(egui::UiBuilder::new().max_rect(rect));
        if crate::widgets::labeled_button(&mut child, &palette, None, "원본 요소 편집", false).clicked() {
            self.left_tab_to(board::LeftTab::Layers);
            self.invoke_ui("web.open_file", json!({ "path": path }));
        }
    }

    pub(super) fn cancel_materializing_previews(&mut self) {
        self.web_preview_state.requested_revision = None;
        self.web_preview_state.refresh_ticket = None;
        self.web_preview_state.explicit_ticket = None;
        self.web_preview_state.deferred_exit_notice = None;
        self.web_preview_state.refresh_failures = 0;
        let canceled: Vec<_> = self.pending.extract_if(|_, request| matches!(request.action, PendingAction::MaterializeWeb))
            .filter_map(|(_, request)| request.ui_run).collect();
        for run in canceled {
            self.complete_ui_run(&run.run_id, None, Some(DltError::new(ErrorCode::Canceled, "미리보기 요청이 취소되었습니다")));
        }
    }

    pub(super) fn cancel_preview_refresh(&mut self) {
        if let Some(ticket) = self.web_preview_state.refresh_ticket.take() {
            self.pending.remove(&ticket);
        }
        self.web_preview_state.requested_revision = None;
    }

    pub(super) fn send_web_materialization(&mut self) -> Option<u64> {
        let active = self.active.as_ref().filter(|active| active.kind == ArtifactKind::WebCode)?;
        self.call("web.materialize", json!({ "artifact_id": active.id, "expected_revision": active.revision }), PendingAction::MaterializeWeb)
    }

    pub(super) fn track_explicit_materialization(&mut self, ticket: Option<u64>) {
        self.web_preview_state.generation = next_generation();
        self.web_preview_state.explicit_ticket = ticket;
        if ticket.is_none() { self.web_preview_state.deferred_exit_notice = None; }
    }

    pub(super) fn preview_is_refresh(&self) -> bool {
        self.web_preview_state.opening && self.web_preview_state.refresh_ticket.is_some()
    }

    pub(super) fn begin_preview_launch(&mut self) {
        self.web_preview_state.opening = true;
    }

    pub(super) fn end_preview_launch(&mut self, success: bool) {
        self.web_preview_state.opening = false;
        if !success {
            if self.web_preview_state.refresh_ticket.is_some() {
                self.web_preview_state.refresh_failures = 2;
            }
            self.web_preview_state.refresh_ticket = None;
            self.web_preview_state.requested_revision = None;
            self.web_preview_state.explicit_ticket = None;
            self.web_preview_state.deferred_exit_notice = None;
        }
    }

    pub(in crate::app) fn close_document_preview(&mut self) {
        self.cancel_materializing_previews();
        let canceled: Vec<_> = self.pending.extract_if(|_, request| matches!(request.action, PendingAction::PlayDryRun { .. } | PendingAction::PlayExport { .. }))
            .filter_map(|(_, request)| request.ui_run).collect();
        for run in canceled {
            self.complete_ui_run(&run.run_id, None, Some(DltError::new(ErrorCode::Canceled, "문서를 떠나 재생 요청이 취소되었습니다")));
        }
        if let Some(run) = self.web_preview_run.take() {
            self.complete_ui_run(&run.id, None, Some(DltError::new(ErrorCode::Canceled, "문서를 떠나 미리보기를 닫았습니다")));
        }
        if let Err(error) = self.stop_web_preview_child() {
            self.set_error(format!("웹 미리보기 창을 닫지 못했습니다 ({error})"));
        }
        self.web_preview_state = PreviewState::default();
    }

    pub(super) fn track_web_preview(&mut self) {
        let generation = next_generation();
        if let Some(active) = &self.active {
            self.web_preview_state = PreviewState {
                generation,
                child_generation: generation,
                artifact_id: Some(active.id.clone()),
                displayed_revision: active.revision,
                observed_revision: active.revision,
                ..Default::default()
            };
        } else {
            self.web_preview_state.generation = generation;
            self.web_preview_state.child_generation = generation;
        }
    }

    pub(super) fn refresh_child_has_closed(&mut self, revision: u64) -> bool {
        self.web_preview_state.requested_revision == Some(revision)
            && self.web_preview_child.as_mut().is_none_or(|child| !matches!(child.try_wait(), Ok(None)))
    }

    pub(super) fn update_web_preview(&mut self) {
        if !self.web_preview_state.opening && self.web_preview_state.explicit_ticket.is_some_and(|ticket| !self.pending.contains_key(&ticket)) {
            self.web_preview_state.explicit_ticket = None;
            self.web_preview_state.deferred_exit_notice = None;
            if self.web_preview_child.is_some() {
                self.web_preview_state.generation = self.web_preview_state.child_generation;
            }
        }
        if self.web_preview_child.is_none()
            && self.web_preview_state.refresh_ticket.is_some_and(|ticket| !self.pending.contains_key(&ticket)) {
            self.web_preview_state.refresh_ticket = None;
            self.web_preview_state.requested_revision = None;
        }
        if self.host_lost() || self.web_preview_state.artifact_id.is_none() || self.web_preview_state.opening { return; }
        if self.web_preview_child.as_mut().is_none_or(|child| !matches!(child.try_wait(), Ok(None))) { return; }
        let Some(active) = self.active.as_ref().filter(|active| self.screen_is_editor()
            && self.web_preview_state.artifact_id.as_deref() == Some(active.id.as_str())) else {
            self.close_document_preview();
            return;
        };
        if active.kind != ArtifactKind::WebCode { return; }
        let revision = active.revision;
        let state = &mut self.web_preview_state;
        if state.refresh_ticket.is_some_and(|ticket| !self.pending.contains_key(&ticket)) {
            state.refresh_ticket = None;
            if state.requested_revision.take() == Some(revision) {
                state.refresh_failures += 1;
                state.changed_at = Some(Instant::now());
                if state.refresh_failures == 2 {
                    self.set_error("웹 미리보기를 갱신하지 못했습니다. 미리보기를 다시 열어 주세요".into());
                }
            }
        }
        let state = &mut self.web_preview_state;
        if revision > state.observed_revision {
            state.observed_revision = revision;
            state.changed_at = Some(Instant::now());
            state.refresh_failures = 0;
        }
        if state.explicit_ticket.is_some() { return; }
        if state.refresh_ticket.is_some() && state.requested_revision != Some(state.observed_revision) {
            self.cancel_preview_refresh();
        }
        let state = &self.web_preview_state;
        if state.observed_revision > state.displayed_revision
            && state.refresh_ticket.is_none()
            && state.refresh_failures < 2
            && state.changed_at.is_some_and(|at| at.elapsed() >= if state.refresh_failures == 0 { Duration::from_millis(300) } else { Duration::from_secs(1) }) {
            let revision = state.observed_revision;
            if let Some(ticket) = self.send_web_materialization() {
                self.web_preview_state.requested_revision = Some(revision);
                self.web_preview_state.refresh_ticket = Some(ticket);
            } else {
                self.web_preview_state.refresh_failures += 1;
                self.web_preview_state.changed_at = Some(Instant::now());
                if self.web_preview_state.refresh_failures == 2 {
                    self.set_error("웹 미리보기를 갱신하지 못했습니다. 미리보기를 다시 열어 주세요".into());
                }
            }
        }
    }
}

fn next_generation() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}
