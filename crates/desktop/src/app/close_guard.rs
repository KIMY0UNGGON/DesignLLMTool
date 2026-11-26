use super::*;
use super::state::HeldEditState;

impl DesktopApp {
    pub(super) fn held_edit_close_warning(&self) -> Option<String> {
        let inline_pending = self.inline_text_has_close_work();
        let release_pending = self.pending.values().any(|pending| matches!(&pending.action, PendingAction::InlineTextRelease { .. } | PendingAction::InlineTextCloseRelease { .. }));
        let inline_rpc_pending = self.pending.values().any(|pending| matches!(&pending.action, PendingAction::InlineTextChange { .. } | PendingAction::InlineTextReserve { .. }));
        let inline_change_pending = self.pending.values().any(|pending| matches!(&pending.action, PendingAction::InlineTextChange { .. }));
        let crop_pending = self.image_crop_apply_pending();
        let crop_work = self.image_crop_editor.as_ref().is_some_and(ImageCropEditor::has_close_work);
        let video_work = self.video_insert.as_ref().is_some_and(PendingVideoInsert::close_work);
        let video_wait = self.video_insert.as_ref().is_some_and(PendingVideoInsert::close_wait_required);
        let memo_pending = self.board_memo_request_in_flight();
        let memo_work = self.board_memo_editor.is_some() || self.board_memo_mutation.is_some() || self.board_memo_gesture.is_some();
        let section_pending = self.page_section_request_in_flight();
        let section_work = self.page_section_editor.is_some() || self.page_section_mutation.is_some();
        let export_pending = self.export_in_flight();
        let export_asked = self.export_confirmations_open();
        let figure = self.figure_close_warning();
        if self.held_edits.is_empty() && figure.is_none() && !inline_pending && !release_pending && !inline_rpc_pending && !memo_work
            && !section_work && !crop_work && !video_work && !export_pending && !export_asked && !self.close_release_failed
            && !self.close_after_inline_cleanup
        {
            return None;
        }
        if export_pending {
            return Some("내보내기 요청의 결과를 기다리고 있습니다. 결과가 돌아오기 전에는 창을 닫을 수 없습니다.".into());
        }
        if export_asked {
            return Some("손실 확인에 답하지 않은 내보내기가 있습니다. 닫으려면 그 내보내기를 취소하세요.".into());
        }
        if video_wait {
            return Some("동영상 가져오기 또는 노드 추가 요청의 결과를 기다리고 있습니다. 결과가 돌아오기 전에는 창을 닫을 수 없습니다.".into());
        }
        if crop_pending {
            return Some("이미지 자르기 적용 결과를 기다리고 있습니다. 요청이 끝나기 전에는 창을 닫을 수 없습니다.".into());
        }
        if memo_pending || section_pending {
            return Some("보드 메모 또는 페이지 섹션 저장 요청의 결과를 확인하고 있습니다. 요청 결과가 확정될 때까지 창을 닫을 수 없습니다.".into());
        }
        if self.close_release_failed {
            let outcome = if self.close_outcome_uncertain { " 앞서 전송된 변경은 이미 작업물에 반영됐을 수 있습니다." } else { "" };
            let board_drafts = if memo_work || section_work { " 보드 메모 또는 섹션 초안도 아직 적용되지 않았습니다." } else { "" };
            return Some(format!("텍스트 예약 해제를 확인하지 못했습니다. 닫으면 예약이 만료될 때까지 다른 창의 편집이 막힐 수 있습니다.{outcome}{board_drafts}"));
        }
        if self.close_after_inline_cleanup || self.close_release_failed {
            return Some(if self.close_outcome_uncertain {
                "전송 중이던 입력은 이미 작업물에 반영됐을 수 있습니다. 예약 해제를 확인한 뒤 창을 닫습니다.".into()
            } else {
                "확정되지 않은 입력을 버리고 예약 해제를 확인하고 있습니다.".into()
            });
        }
        // Held PDF records and the figure work are worded by `figure_close_warning`, not counted here.
        let count = self.held_edits.len()
            + if inline_pending || release_pending || inline_rpc_pending { 1 } else { 0 }
            + if memo_work || section_work { 1 } else { 0 }
            + if crop_work { 1 } else { 0 }
            + if video_work { 1 } else { 0 };
        let uncertain = self.close_outcome_uncertain
            || self.held_edits.iter().any(|held| held.state == HeldEditState::Sending || held.retry_same_request)
            || self.inline_text_edit.as_ref().is_some_and(|session| session.in_flight.is_some() || session.retry_same_request)
            || self.board_memo_mutation.as_ref().is_some_and(|mutation| mutation.outcome_uncertain)
            || self.page_section_mutation.as_ref().is_some_and(|mutation| mutation.outcome_uncertain)
            || crop_pending || self.image_crop_editor.as_ref().is_some_and(|editor| editor.outcome_uncertain)
            || self.video_insert.as_ref().is_some_and(|pending| pending.stage == VideoInsertStage::OutcomeUncertain)
            || inline_change_pending
            || release_pending;
        let generic = (count > 0).then(|| {
            if uncertain {
                format!("보류된 변경, 텍스트 입력, 메모, 섹션, 이미지 자르기 또는 동영상 추가 요청 {count}건 중 요청이 전송 중이거나 결과가 불명확한 항목이 있습니다. 이미 작업물에 반영됐을 수 있으며, 창을 닫아도 되돌려지지 않습니다.")
            } else {
                format!("아직 적용되지 않은 보류 변경, 텍스트 입력, 메모, 섹션, 이미지 자르기 또는 동영상 추가 초안 {count}건이 있습니다. 창을 닫으면 동영상 추가 초안을 버립니다.")
            }
        });
        match (generic, figure) {
            (Some(generic), Some(figure)) => Some(format!("{generic} {figure}")),
            (Some(generic), None) => Some(generic),
            (None, figure) => figure,
        }
    }

    pub(super) fn board_memo_request_in_flight(&self) -> bool {
        self.board_memo_mutation.as_ref().is_some_and(|mutation| mutation.pending)
            || self.pending.values().any(|pending| matches!(&pending.action, PendingAction::BoardMemosChange { .. }))
            || self.held_edits.iter().any(|held| {
                held.state == HeldEditState::Sending
                    && matches!(&held.apply.after_action, PendingAction::BoardMemosChange { .. })
            })
    }

    pub(super) fn page_section_request_in_flight(&self) -> bool {
        self.page_section_mutation.as_ref().is_some_and(|mutation| mutation.pending)
            || self.pending.values().any(|pending| matches!(&pending.action, PendingAction::PageSectionsChange { .. }))
            || self.held_edits.iter().any(|held| {
                held.state == HeldEditState::Sending
                    && matches!(&held.apply.after_action, PendingAction::PageSectionsChange { .. })
            })
    }

    pub(super) fn inline_text_has_close_work(&self) -> bool {
        self.inline_text_edit.as_ref().is_some_and(|session| {
            session.composing || session.ime_commit_pending || session.reservation_pending
                || session.reservation_release_pending || session.reservation_release_failed
                || session.in_flight.is_some() || !session.queue.is_empty()
                || session.reservation_id.is_some() || self.text_adapter.is_ime_active() || self.text_draft != session.committed_text
        })
            || self.pending.values().any(|pending| matches!(&pending.action,
                PendingAction::InlineTextChange { .. } | PendingAction::InlineTextReserve { .. }
                    | PendingAction::InlineTextRelease { .. } | PendingAction::InlineTextCloseRelease { .. }))
    }

    /// A submitted export still waits for its dry run or its write: only that reply starts or ends the write, so the window
    /// does not close under it.
    pub(super) fn export_in_flight(&self) -> bool {
        self.pending.values().any(|pending| matches!(&pending.action, PendingAction::ExportDryRun | PendingAction::Export | PendingAction::UiExport))
            || self.export_completions.keys().any(|ticket| self.pending.contains_key(ticket))
            || !self.export_reports_waiting.is_empty()
    }

    pub(super) fn has_close_work(&self) -> bool {
        self.has_non_figure_close_work() || self.figure_close_warning().is_some()
    }

    pub(super) fn has_non_figure_close_work(&self) -> bool {
        !self.held_edits.is_empty() || self.inline_text_has_close_work()
            || self.board_memo_editor.is_some() || self.board_memo_mutation.is_some() || self.board_memo_gesture.is_some()
            || self.page_section_editor.is_some() || self.page_section_mutation.is_some()
            || self.image_crop_editor.as_ref().is_some_and(ImageCropEditor::has_close_work)
            || self.video_insert.as_ref().is_some_and(PendingVideoInsert::close_work)
            || self.pending.values().any(|pending| matches!(&pending.action, PendingAction::InlineTextRelease { .. } | PendingAction::InlineTextCloseRelease { .. }))
            || self.pending.values().any(|pending| matches!(&pending.action, PendingAction::InlineTextChange { .. }))
            || self.board_memo_request_in_flight()
            || self.page_section_request_in_flight()
            || self.export_in_flight()
            || self.export_confirmations_open()
            || self.close_release_failed || self.close_after_inline_cleanup
    }

    pub(super) fn guard_held_edit_close(&mut self, ctx: &Context) {
        if !ctx.input(|input| input.viewport().close_requested()) { return; }
        // `종료` after the host was lost (host_link.rs): held work cannot be released without the host, and the user chose to close.
        // The window's own close (X, Alt+F4) while the host is gone is the same choice.
        if self.host_link.quitting || self.host_lost() {
            self.close_after_host_loss();
            return;
        }
        if self.permit_close_once && !self.has_close_work_besides_figure_jobs() {
            self.permit_close_once = false;
            return;
        }
        self.permit_close_once = false;
        if self.video_insert.as_ref().is_some_and(|pending| {
            !pending.cancel_requested
                && !matches!(pending.stage, VideoInsertStage::Applying | VideoInsertStage::OutcomeUncertain)
        }) {
            let _ = self.cancel_canvas_video_insert();
        }
        // Drafts that cannot be written keep the window open: the dialog asks before they are lost.
        if self.has_close_work() || !self.drafts.flush() {
            self.close_confirmation_open = true;
            ctx.send_viewport_cmd(ViewportCommand::CancelClose);
        }
    }

    /// Closing past drafts that cannot be saved also drops the held edits (`close_discarding(false)`).
    fn held_drops_on_close(&self, drafts_unsaved: bool) -> bool {
        drafts_unsaved && !self.held_edits.is_empty()
    }

    pub(super) fn close_discard_label(&self, drafts_unsaved: bool) -> &'static str {
        if self.held_drops_on_close(drafts_unsaved) {
            "보류 변경을 버리고 닫기"
        } else if drafts_unsaved {
            "그래도 닫기"
        } else if self.figure_only_job_remains() {
            "닫기"
        } else {
            "보류 변경 버리고 닫기"
        }
    }

    pub(super) fn held_drop_note(&self, drafts_unsaved: bool) -> Option<&'static str> {
        self.held_drops_on_close(drafts_unsaved).then_some("보류 중인 변경도 버려집니다.")
    }

    /// Drops the exports still waiting for their loss answer and closes, unless other work still holds the window: the
    /// question then goes on with that work.
    pub(super) fn cancel_exports_and_close(&mut self, ctx: &Context) {
        self.cancel_export_confirmations();
        if self.has_close_work() || !self.drafts.flush() {
            return;
        }
        self.close_confirmation_open = false;
        self.permit_close_once = true;
        ctx.send_viewport_cmd(ViewportCommand::Close);
    }

    pub(super) fn keep_working_after_close_request(&mut self, ctx: &Context) {
        self.close_confirmation_open = false;
        ctx.send_viewport_cmd(ViewportCommand::CancelClose);
    }

    /// The drafts of the board memo (the editor, and the memo of a change on its way) and of the inline text sessions (the one on
    /// screen, and the one being closed) are taken out of the file, before those editors are dropped: the user agreed to lose their
    /// text, and `Drop` would write it back otherwise.
    fn drop_editor_drafts(&mut self) {
        let memo_ids: Vec<String> = self
            .board_memo_editor
            .iter()
            .chain(self.board_memo_mutation.as_ref().and_then(|mutation| mutation.editor.as_ref()))
            .map(|editor| editor.memo.id.clone())
            .collect();
        for memo_id in memo_ids {
            self.clear_note_draft(&memo_id);
        }
        let sessions: BTreeSet<u64> = self.inline_text_edit.iter().map(|session| session.id).chain(self.close_inline_session_id).collect();
        for session_id in sessions {
            self.clear_inline_draft(session_id);
        }
    }

    pub(super) fn discard_held_edits_and_close(&mut self, ctx: &Context) {
        self.close_discarding(ctx, true);
    }

    /// Closes after dropping the held edits. With `save_drafts`, a draft file that cannot be written keeps the window open.
    fn close_discarding(&mut self, ctx: &Context, save_drafts: bool) {
        if self.board_memo_request_in_flight() || self.page_section_request_in_flight() || self.image_crop_apply_pending()
            || self.export_in_flight()
            || self.video_insert.as_ref().is_some_and(PendingVideoInsert::close_wait_required) {
            self.close_confirmation_open = true;
            ctx.send_viewport_cmd(ViewportCommand::CancelClose);
            return;
        }
        self.close_outcome_uncertain = self.held_edits.iter().any(|held| held.state == HeldEditState::Sending || held.retry_same_request)
            || self.held_pdf_evidence.iter().any(|held| held.ticket.is_some())
            || self.inline_text_edit.as_ref().is_some_and(|session| session.in_flight.is_some() || session.retry_same_request)
            || self.pending.values().any(|pending| matches!(&pending.action, PendingAction::InlineTextChange { .. } | PendingAction::DatasetEdit))
            || self.pending.values().any(|pending| matches!(&pending.action, PendingAction::InlineTextRelease { .. } | PendingAction::InlineTextCloseRelease { .. }))
            || self.video_insert.as_ref().is_some_and(|pending| pending.stage == VideoInsertStage::OutcomeUncertain);
        // Every draft of what is dropped below goes with it (`Drop` would write them back otherwise).
        self.drop_editor_drafts();
        if let Some(token) = self.image_crop_editor.as_ref().map(|editor| editor.intent.token.clone()) {
            self.clear_crop_draft(&token);
        }
        self.drafts.sync_group("pdf:", Vec::new(), Instant::now());
        let saved = self.drafts.flush();
        if save_drafts && !saved {
            self.close_confirmation_open = true;
            ctx.send_viewport_cmd(ViewportCommand::CancelClose);
            return;
        }
        self.held_edits.clear();
        self.held_pdf_evidence.clear();
        self.held_pdf_warnings.clear();
        self.cancel_export_confirmations();
        self.discard_figure_work();
        self.held_refusal_id = None;
        self.refusal = None;
        self.refusal_ids.clear();
        self.refusal_fixes.clear();
        self.board_memo_editor = None;
        self.board_memo_mutation = None;
        self.board_memo_gesture = None;
        self.page_section_editor = None;
        self.page_section_mutation = None;
        self.image_crop_editor = None;
        self.discard_video_insert_for_close();
        if self.inline_text_has_close_work() {
            self.close_after_inline_cleanup = true;
            self.close_confirmation_open = true;
            self.start_inline_text_close_cleanup(ctx);
            return;
        }
        self.close_confirmation_open = false;
        self.permit_close_once = true;
        ctx.send_viewport_cmd(ViewportCommand::Close);
    }

    pub(super) fn start_inline_text_close_cleanup(&mut self, ctx: &Context) {
        let close_session = self.inline_text_edit.as_mut().map(|session| {
            let session_id = session.id;
            let committed_text = session.committed_text.clone();
            session.queue.clear();
            session.composing = false;
            session.ime_commit_pending = false;
            session.ime_cancelled_frame = true;
            session.finish_requested = true;
            session.conflict = None;
            session.retry_allowed = false;
            session.retry_expected = false;
            session.release_when_reserved = true;
            let pending = session.in_flight.is_some() || session.reservation_pending;
            (session_id, committed_text, pending)
        });
        if let Some((session_id, committed_text, pending)) = close_session {
            self.close_inline_session_id = Some(session_id);
            self.text_draft = committed_text;
            self.text_adapter = GraphemeAdapter::default();
            self.text_editor_focused = false;
            if pending { return; }
            if self.pending.values().any(|pending| matches!(&pending.action,
                PendingAction::InlineTextRelease { session_id: pending_id, .. }
                    | PendingAction::InlineTextCloseRelease { session_id: pending_id }
                    if *pending_id == session_id))
            {
                self.close_release_pending = true;
                return;
            }
            self.advance_inline_text_close_cleanup(ctx);
            return;
        }
        if let Some((session_id, release_pending)) = self.pending.values().find_map(|pending| match &pending.action {
            PendingAction::InlineTextRelease { session_id, .. }
            | PendingAction::InlineTextCloseRelease { session_id } => Some((*session_id, true)),
            PendingAction::InlineTextChange { session_id } | PendingAction::InlineTextReserve { session_id } => Some((*session_id, false)),
            _ => None,
        }) {
            self.close_inline_session_id = Some(session_id);
            self.close_release_pending = release_pending;
            return;
        }
        self.finalize_close_after_inline_cleanup(ctx);
    }

    pub(super) fn advance_inline_text_close_cleanup(&mut self, ctx: &Context) {
        if !self.close_after_inline_cleanup || self.close_release_pending || self.close_release_failed { return; }
        let Some(session_id) = self.close_inline_session_id else {
            self.finalize_close_after_inline_cleanup(ctx);
            return;
        };
        let release_pending = self.pending.values().any(|pending| matches!(
            &pending.action,
            PendingAction::InlineTextRelease { session_id: pending_id, .. }
                | PendingAction::InlineTextCloseRelease { session_id: pending_id }
                if *pending_id == session_id
        ));
        if release_pending {
            self.close_release_pending = true;
            return;
        }
        let inline_call_pending = self.pending.values().any(|pending| matches!(
            &pending.action,
            PendingAction::InlineTextChange { session_id: pending_id } | PendingAction::InlineTextReserve { session_id: pending_id }
                if *pending_id == session_id
        ));
        if inline_call_pending { return; }
        let session_exists = self.inline_text_edit.as_ref().is_some_and(|session| session.id == session_id);
        if session_exists {
            let reservation_id = {
                let session = self.inline_text_edit.as_mut().expect("session checked");
                if session.in_flight.is_some() || session.reservation_pending { return; }
                session.reservation_expires = None;
                session.release_when_reserved = false;
                session.reservation_id.take()
            };
            if let Some(reservation_id) = reservation_id {
                self.release_inline_reservation(session_id, reservation_id);
                return;
            }
            self.inline_text_edit = None;
        } else {
        }
        self.finalize_close_after_inline_cleanup(ctx);
    }

    pub(super) fn check_close_inline_reservation(&mut self, session_id: u64, view_id: String, actor: String, node_id: String, artifact_id: String) {
        self.close_inline_session_id = Some(session_id);
        self.close_release_pending = true;
        if self.call("targets.list", json!({ "artifact_id": artifact_id.clone() }), PendingAction::InlineTextCloseReservationCheck {
            session_id, view_id, actor, node_id, artifact_id,
        }).is_none() {
            self.close_release_pending = false;
            self.close_release_failed = true;
            self.close_after_inline_cleanup = false;
            self.close_confirmation_open = true;
        }
    }

    pub(super) fn complete_close_reservation_check(&mut self, ctx: &Context, session_id: u64, view_id: &str, actor: &str, node_id: &str, expected_artifact: &str, value: &Value) {
        let Some(artifact_id) = value.get("artifact_id").and_then(Value::as_str) else {
            self.fail_close_reservation_check(session_id);
            return;
        };
        if artifact_id != expected_artifact {
            self.fail_close_reservation_check(session_id);
            return;
        }
        if self.close_inline_session_id != Some(session_id) {
            self.close_release_pending = false;
            return;
        }
        let reservation_id = value.get("reservations").and_then(Value::as_array).into_iter().flatten().find_map(|reservation| {
            let belongs_to_session = reservation.get("view_id").and_then(Value::as_str) == Some(view_id)
                && reservation.get("owner").and_then(Value::as_str) == Some(actor)
                && reservation.get("node_ids").and_then(Value::as_array).is_some_and(|nodes| nodes.iter().any(|id| id.as_str() == Some(node_id)));
            belongs_to_session.then(|| reservation.get("reservation_id").and_then(Value::as_str)).flatten()
        }).map(str::to_owned);
        self.close_release_pending = false;
        if let Some(reservation_id) = reservation_id {
            self.close_reservation_id = Some(reservation_id.clone());
            if let Some(session) = self.inline_text_edit.as_mut().filter(|session| session.id == session_id) {
                session.reservation_id = Some(reservation_id);
            } else {
                self.close_release_pending = true;
                if self.call("targets.release", json!({ "reservation_id": reservation_id }), PendingAction::InlineTextCloseRelease { session_id }).is_none() {
                    self.close_release_pending = false;
                    self.close_release_failed = true;
                    self.close_after_inline_cleanup = false;
                    self.close_confirmation_open = true;
                }
                return;
            }
        }
        self.advance_inline_text_close_cleanup(ctx);
    }

    pub(super) fn fail_close_reservation_check(&mut self, session_id: u64) {
        self.close_release_pending = false;
        self.close_release_failed = true;
        self.close_after_inline_cleanup = false;
        self.close_inline_session_id = Some(session_id);
        self.close_confirmation_open = true;
    }

    pub(super) fn retry_close_reservation_release(&mut self) {
        let Some(reservation_id) = self.close_reservation_id.clone() else { return };
        let Some(session_id) = self.close_inline_session_id else { return };
        self.close_release_failed = false;
        self.close_release_pending = true;
        if self.call("targets.release", json!({ "reservation_id": reservation_id }), PendingAction::InlineTextCloseRelease { session_id }).is_none() {
            self.close_release_pending = false;
            self.close_release_failed = true;
        }
    }

    pub(super) fn retry_close_reservation_check(&mut self) {
        let Some(session) = self.inline_text_edit.as_ref() else { return };
        let (session_id, view_id, actor, node_id, artifact_id) = (
            session.id, session.view_id.clone(), session.actor.clone(), session.node_id.clone(), session.artifact_id.clone(),
        );
        self.close_release_failed = false;
        self.close_after_inline_cleanup = true;
        self.check_close_inline_reservation(session_id, view_id, actor, node_id, artifact_id);
    }

    pub(super) fn fail_close_inline_release(&mut self, session_id: u64) {
        self.close_release_pending = false;
        self.close_release_failed = true;
        self.close_after_inline_cleanup = false;
        self.close_inline_session_id = Some(session_id);
        self.close_confirmation_open = true;
        self.inline_text_edit = None;
        self.text_editor_focused = false;
        self.text_adapter = GraphemeAdapter::default();
        self.refresh_active();
    }

    pub(super) fn finalize_close_after_inline_cleanup(&mut self, ctx: &Context) {
        self.drop_editor_drafts();
        self.inline_text_edit = None;
        self.text_editor_focused = false;
        self.text_adapter = GraphemeAdapter::default();
        self.close_after_inline_cleanup = false;
        self.close_release_pending = false;
        self.close_release_failed = false;
        self.close_inline_session_id = None;
        self.close_reservation_id = None;
        self.close_outcome_uncertain = false;
        self.close_confirmation_open = false;
        self.permit_close_once = true;
        ctx.send_viewport_cmd(ViewportCommand::Close);
    }

    pub(super) fn force_close_after_release_failure(&mut self, ctx: &Context) {
        self.drop_editor_drafts();
        self.held_edits.clear();
        self.inline_text_edit = None;
        self.text_adapter = GraphemeAdapter::default();
        self.text_editor_focused = false;
        self.held_refusal_id = None;
        self.refusal = None;
        self.refusal_ids.clear();
        self.refusal_fixes.clear();
        self.board_memo_editor = None;
        self.board_memo_mutation = None;
        self.board_memo_gesture = None;
        self.page_section_editor = None;
        self.page_section_mutation = None;
        self.close_release_failed = false;
        self.close_after_inline_cleanup = false;
        self.close_release_pending = false;
        self.discard_video_insert_for_close();
        self.close_inline_session_id = None;
        self.close_reservation_id = None;
        self.close_outcome_uncertain = false;
        self.close_confirmation_open = false;
        self.permit_close_once = true;
        ctx.send_viewport_cmd(ViewportCommand::Close);
    }

    /// Whether a dialog that answers Enter and Esc is open (68d: the close and switch questions, the lost host, the agent's
    /// model-run approval, the export losses, the figure form, the page section editor). While one is, the board and the
    /// window's shortcuts leave the keys to it (`handle_shortcuts`, the board's Enter and the inline editor's Esc, the memo keys).
    pub(super) fn key_dialog_open(&self) -> bool {
        self.close_confirmation_open
            || self.pending_switch.is_some()
            || self.host_link.lost
            || self.has_home_confirmation()
            || self.export_confirmation.is_some()
            || self.figure_form.is_some()
            || self.page_section_editor.is_some()
    }

    pub(super) fn draw_held_edit_close_dialog(&mut self, ctx: &Context) {
        if !self.close_confirmation_open { return; }
        let warning = self.held_edit_close_warning();
        let drafts_unsaved = self.drafts.save_failing();
        if warning.is_none() && !drafts_unsaved {
            self.close_confirmation_open = false;
            return;
        }
        let palette = crate::theme::palette(ctx);
        use crate::widgets::{DialogButton, DialogChoice};
        /// What each button of the question does.
        #[derive(Clone, Copy, PartialEq, Eq)]
        enum Answer { Keep, CancelVideo, CancelExports, RetryRelease, RetryCheck, CloseAfterFailure, Discard }
        let keep = (Answer::Keep, DialogChoice::new("계속 작업", DialogButton::Primary));
        let cancel_exports = self.cancel_exports_label();
        // The buttons stay under the text, in view however long it is (`dialog_with_choices`).
        let shown = crate::widgets::dialog_with_choices(ctx, &palette, Id::new("held-edit-close-confirmation"), "보류된 변경", crate::widgets::DialogPlace::Modal, |ui| {
            if let Some(text) = warning.as_deref() {
                ui.label(crate::i18n::tr(&text));
            }
            if drafts_unsaved {
                ui.label(crate::i18n::tr("초안을 저장하지 못했습니다. 그래도 닫을까요?"));
            }
            let mut answers = Vec::new();
            if self.image_crop_apply_pending() {
                ui.label(crate::i18n::tr("이미지 자르기 적용 결과를 기다리는 동안에는 닫을 수 없습니다."));
                answers.push(keep);
            } else if self.video_insert.as_ref().is_some_and(PendingVideoInsert::close_wait_required) {
                let cancel_allowed = self.video_insert.as_ref().is_some_and(|pending| {
                    !pending.cancel_requested
                        && !matches!(pending.stage, VideoInsertStage::Applying | VideoInsertStage::OutcomeUncertain)
                });
                ui.label(crate::i18n::tr("동영상 검사·가져오기 또는 노드 추가 결과가 돌아오는 동안에는 닫을 수 없습니다."));
                if cancel_allowed {
                    answers.push((Answer::CancelVideo, DialogChoice::new("동영상 추가 취소 후 정리 기다리기", DialogButton::Plain)));
                }
                answers.push(keep);
            } else if self.board_memo_request_in_flight() || self.page_section_request_in_flight() {
                ui.label(crate::i18n::tr("메모 또는 섹션 저장 결과를 기다리는 동안에는 닫을 수 없습니다."));
                answers.push(keep);
            } else if self.export_in_flight() {
                answers.push(keep);
            } else if self.export_confirmations_open() {
                answers.push(keep);
                answers.push((Answer::CancelExports, DialogChoice::new(&cancel_exports, DialogButton::Danger).human_only()));
            } else if self.close_after_inline_cleanup || self.close_release_pending {
                ui.label(crate::i18n::tr("진행 중인 입력 결과와 예약 해제를 확인 중입니다."));
            } else if self.close_release_failed {
                if self.close_reservation_id.is_some() {
                    answers.push((Answer::RetryRelease, DialogChoice::new("예약 해제 다시 시도", DialogButton::Plain)));
                } else if self.inline_text_edit.is_some() {
                    answers.push((Answer::RetryCheck, DialogChoice::new("예약 상태 다시 확인", DialogButton::Plain)));
                }
                let discards_board = self.board_memo_editor.is_some() || self.board_memo_mutation.is_some() || self.board_memo_gesture.is_some()
                    || self.page_section_editor.is_some() || self.page_section_mutation.is_some();
                let close_label = if discards_board {
                    "보드 초안을 버리고 예약 만료를 기다리며 닫기"
                } else {
                    "예약 만료를 기다리며 닫기"
                };
                let kind = if discards_board { DialogButton::Danger } else { DialogButton::Plain };
                answers.push((Answer::CloseAfterFailure, DialogChoice::new(close_label, kind).human_only()));
            } else {
                // A figure job that runs on its own is not discarded by closing: the button then only closes.
                let discard_label = self.close_discard_label(drafts_unsaved);
                if let Some(note) = self.held_drop_note(drafts_unsaved) { ui.label(crate::i18n::tr(&note)); }
                answers.push(keep);
                answers.push((Answer::Discard, DialogChoice::new(discard_label, DialogButton::Danger).human_only()));
            }
            let choices = answers.iter().map(|(_, choice)| *choice).collect();
            (answers.into_iter().map(|(answer, _)| answer).collect::<Vec<_>>(), choices)
        });
        let (answers, pressed) = shown.inner;
        let pressed: Vec<Answer> = answers.iter().zip(pressed).filter(|(_, pressed)| *pressed).map(|(answer, _)| *answer).collect();
        // Where `계속 작업` is offered it is the main and the safe choice: Enter, Esc and a press on the backdrop keep working.
        let keep_working = pressed.contains(&Answer::Keep) || (answers.contains(&Answer::Keep) && (shown.enter || shown.dismissed));
        if keep_working { self.keep_working_after_close_request(ctx); }
        if pressed.contains(&Answer::CancelVideo) { let _ = self.cancel_canvas_video_insert(); }
        if pressed.contains(&Answer::CancelExports) { self.cancel_exports_and_close(ctx); }
        if pressed.contains(&Answer::Discard) {
            if drafts_unsaved { self.close_discarding(ctx, false); } else { self.discard_held_edits_and_close(ctx); }
        }
        if pressed.contains(&Answer::RetryRelease) { self.retry_close_reservation_release(); }
        if pressed.contains(&Answer::RetryCheck) { self.retry_close_reservation_check(); }
        if pressed.contains(&Answer::CloseAfterFailure) { self.force_close_after_release_failure(ctx); }
    }
}

// The dialogs and notices of task 68d through `App::update` (`dialog_tests.rs` beside this file).
#[cfg(test)]
#[path = "dialog_tests.rs"]
mod dialog_tests;
