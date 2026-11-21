use super::*;
use super::{messages::change_result_unknown, state::{ui_run_success_result, SaveRpcOutcome}};

impl DesktopApp {
    pub(super) fn process_replies(&mut self, ctx: &Context) {
        let replies: Vec<RpcReply> = self.rpc.poll().collect();
        if let Some(test) = self.ui_test.as_mut() {
            for timing in self.rpc.timings() {
                test.perf.timed(timing);
            }
            for reply in &replies {
                test.perf.answered(reply.ticket, reply.result.is_err());
            }
        }
        for reply in replies {
            self.note_host_reply(reply.ticket, reply.result.as_ref().err());
            if let Some(sent) = self.job_starts.remove(&reply.ticket) {
                self.remember_job_prompt(&sent, &reply.result);
                self.settle_job_start(sent, &reply.result);
            }
            // A request sent from a draft is answered here, whether or not the window still shows its editor (drafts.rs).
            self.answer_sent_drafts(reply.ticket, &reply.result);
            // A refused commit an agent's run started brings no selection, so its ticket is forgotten now (`remember_agent_commit`).
            if reply.result.is_err() {
                self.ui.take_agent_commit(reply.ticket);
            }
            let Some(pending) = self.pending.remove(&reply.ticket) else {
                if self.late_starts.remove(&reply.ticket).is_some() {
                    self.late_start_answered(reply.result);
                }
                continue;
            };
            self.awaited_starts.remove(&reply.ticket);
            let method = pending.method.clone();
            let apply_context = pending.apply.clone();
            let mut ui_run = pending.ui_run;
            if let (Some(run), Ok(RpcPayload::Json(value))) = (&mut ui_run, &reply.result) {
                let asset_hash = match &pending.action {
                    PendingAction::FigureImageAsset { .. } => value.get("sha256"),
                    PendingAction::FigureRender { .. } => value.pointer("/asset/sha256"),
                    _ => None,
                };
                if let Some(hash) = asset_hash.and_then(Value::as_str) {
                    run.prepared_asset = Some(hash.to_owned());
                }
            }
            if pending.require_active && pending.artifact_id.as_ref().is_some_and(|id| {
                self.active.as_ref().is_none_or(|active| active.id != *id)
            }) {
                self.figure_ai_reply_dropped(&pending.action);
                self.pdf_evidence_reply_dropped(&pending.action, reply.ticket, &reply.result);
                // The window left the document, so the commit's selection is not applied here either.
                self.ui.take_agent_commit(reply.ticket);
                // A job the host started is recorded even though its window moved on: it runs whether or not it is shown.
                if matches!(&pending.action, PendingAction::StartJob | PendingAction::FigureAiJob { .. }) {
                    if let Ok(RpcPayload::Json(value)) = &reply.result {
                        self.late_job_start(value.clone());
                    }
                }
                if let PendingAction::BoardMemosChange { token } = &pending.action {
                    match &reply.result {
                        Ok(_) => self.finish_board_memo_change_offscreen(token),
                        Err(error) => self.fail_board_memo_change(token, error),
                    }
                }
                if let PendingAction::PageSectionsChange { token } = &pending.action {
                    match &reply.result {
                        Ok(_) => self.finish_page_sections_change_offscreen(token),
                        Err(error) => self.fail_page_sections_change(token, error),
                    }
                }
                match &pending.action {
                    PendingAction::ImageCropLookup { token } => match &reply.result {
                        Ok(RpcPayload::Json(value)) => self.complete_image_crop_lookup(token, value),
                        Err(error) => self.fail_image_crop_lookup(token, error),
                        _ => {}
                    },
                    PendingAction::ImageCropApply { token } => match &reply.result {
                        Ok(RpcPayload::Json(_)) => {
                            if self.image_crop_editor.as_ref().is_some_and(|editor| editor.intent.token == *token) {
                                self.clear_crop_draft(token);
                                self.image_crop_editor = None;
                                self.notices.info("이미지 자르기 변경을 원래 작업물에 적용했습니다", Instant::now());
                            }
                        }
                        Err(error) => self.fail_image_crop_apply(token, error, false),
                        _ => {}
                    },
                    _ => {}
                }
                self.handle_video_offscreen_reply(&pending.action, &reply.result);
                if let PendingAction::FlowFindPage { request } = &pending.action {
                    self.flow_find_page_dropped(request);
                }
                if !matches!(&pending.action,
                    PendingAction::BoardMemosChange { .. } | PendingAction::PageSectionsChange { .. }
                        | PendingAction::ImageCropApply { .. } | PendingAction::VideoInsertApply { .. }) {
                    if let (Some(apply), Err(error)) = (&apply_context, &reply.result) {
                        if let Some(held_id) = apply.held_edit_id.as_deref() {
                            self.rehold_edit_after_failure(held_id, apply.clone(), error);
                            self.set_error("다른 작업물에서 결과가 돌아왔습니다. 보류한 변경은 원래 작업물에서 확인하세요".into());
                        } else if error.code == ErrorCode::TargetBusy && apply.user_edit {
                            self.hold_direct_edit(apply.clone(), error.affected_ids.clone(), error);
                            self.set_error("다른 작업물에서 편집이 거부되었습니다. 원래 작업물에서 보류한 변경을 확인하세요".into());
                        }
                    }
                }
                if let PendingAction::HeldEditReservations { held_id } = &pending.action {
                    self.fail_held_edit_check(held_id, &DltError::new(ErrorCode::NotFound, "보류한 변경의 대상 작업물이 바뀌었습니다"));
                }
                if let PendingAction::InlineTextCloseReservationCheck { session_id, .. } = &pending.action {
                    self.fail_close_reservation_check(*session_id);
                }
                if matches!(&pending.action, PendingAction::Context | PendingAction::Inspect | PendingAction::Reservations) {
                    self.fail_image_crop_refresh(pending.artifact_id.as_deref());
                }
                if matches!(&pending.action, PendingAction::Context | PendingAction::Inspect) {
                    self.reset_held_edit_checks("최신 작업물 상태를 읽지 못했습니다. 변경을 보존했습니다");
                }
                if reply.result.is_ok() {
                    if let Some(held_id) = apply_context.as_ref().and_then(|apply| apply.held_edit_id.as_deref()) {
                        self.finish_held_edit_retry(held_id);
                    }
                }
                if apply_context.is_some() && reply.result.is_ok() {
                    self.call("workspace.list", json!({}), PendingAction::WorkspaceList);
                }
                if reply.result.is_ok() && matches!(&pending.action, PendingAction::DesignSystemChange { .. }) {
                    self.call("workspace.list", json!({}), PendingAction::WorkspaceList);
                    if matches!(&pending.action, PendingAction::DesignSystemChange { refresh_systems: true }) {
                        self.fetch_design_systems();
                    }
                }
                if let Some(run) = &ui_run {
                    match &reply.result {
                        Ok(_) if matches!(pending.action, PendingAction::MaterializeWeb) => self.complete_ui_run(&run.run_id, None, Some(
                            DltError::new(ErrorCode::RevisionConflict, "미리보기를 준비하는 동안 작업물이 바뀌었습니다"),
                        )),
                        Ok(_) if pending.action.prepares_figure_change() => self.complete_ui_run(&run.run_id, None, Some(
                            run.prepare_error(DltError::new(ErrorCode::RevisionConflict, "그림을 준비하는 동안 작업물이 바뀌어 변경을 적용하지 않았습니다")),
                        )),
                        Ok(payload) => self.complete_ui_run(&run.run_id, Some(ui_run_success_result(run, payload)), None),
                        Err(error) => self.complete_ui_run(&run.run_id, None, Some(run.prepare_error(error.clone()))),
                    }
                }
                continue;
            }
            let action = pending.action;
            let action = match action {
                PendingAction::Thumb(ticket) => {
                    self.thumb_reply(ctx, ticket, reply.result);
                    continue;
                }
                other => other,
            };
            let user_action = action.user_initiated();
            match reply.result {
                Err(error) => {
                    // A failure of a PDF import the window no longer waits for (replaced or discarded) shows nothing.
                    if matches!(action, PendingAction::PdfAssetImport { .. } | PendingAction::PdfEvidenceImport)
                        && !self.pdf_reply_is_current(reply.ticket)
                    {
                        continue;
                    }
                    match &action {
                        PendingAction::GetParkedJob { job_id } => {
                            // The host does not know the job, so it can never finish: its warnings and polls go with it.
                            if error.code == ErrorCode::NotFound {
                                self.forget_job(job_id);
                            }
                            continue;
                        }
                        // The same for the shown job: dropped, so that its poll stops instead of failing every two seconds.
                        PendingAction::GetJob { job_id } if error.code == ErrorCode::NotFound => {
                            self.forget_job(job_id);
                            continue;
                        }
                        PendingAction::BoardMemosChange { token } => {
                            self.fail_board_memo_change(token, &error);
                            self.state_moved_on(&error, user_action);
                            continue;
                        }
                        PendingAction::PageSectionsChange { token } => {
                            self.fail_page_sections_change(token, &error);
                            self.state_moved_on(&error, user_action);
                            continue;
                        }
                        PendingAction::ImageReplacementImport { token } => {
                            self.fail_image_replacement_import(token, &error);
                            continue;
                        }
                        PendingAction::VideoSourceImport { token } => {
                            self.finish_video_source_import_error(token, &error);
                            continue;
                        }
                        PendingAction::VideoPosterImport { token, temp_path } => {
                            self.finish_video_poster_import_error(token, temp_path, &error);
                            continue;
                        }
                        PendingAction::VideoInsertApply { token } => {
                            self.finish_video_insert_apply_error(token, &error);
                            continue;
                        }
                        PendingAction::ImageReplacementApply { token } => {
                            if self.pending_image_replacement.as_ref().is_some_and(|pending| pending.intent.token == *token) {
                                self.pending_image_replacement = None;
                            }
                            self.set_error(format!("이미지 교체를 적용하지 못했습니다. 원본 이미지는 유지했습니다. 가져온 자산은 사용되지 않은 상태로 남을 수 있습니다: {}", user_error(&error)));
                            self.refresh_active();
                            continue;
                        }
                        PendingAction::ImageCropLookup { token } => {
                            self.fail_image_crop_lookup(token, &error);
                            continue;
                        }
                        PendingAction::ImageCropApply { token } => {
                            self.fail_image_crop_apply(token, &error, true);
                            continue;
                        }
                        PendingAction::InlineTextChange { session_id } => {
                            self.fail_inline_text_change(*session_id, &error);
                            continue;
                        }
                        PendingAction::InlineTextReserve { session_id } => {
                            self.fail_inline_text_reservation(*session_id, &error);
                            continue;
                        }
                        PendingAction::InlineTextRelease { session_id, reservation_id } if error.code == ErrorCode::NotFound => {
                            if self.close_after_inline_cleanup && self.close_inline_session_id == Some(*session_id) {
                                self.close_reservation_id = Some(reservation_id.clone());
                                self.finalize_close_after_inline_cleanup(ctx);
                            } else {
                                self.complete_inline_text_release(*session_id, reservation_id, false);
                            }
                            continue;
                        }
                        PendingAction::InlineTextRelease { session_id, reservation_id } => {
                            if self.close_after_inline_cleanup && self.close_inline_session_id == Some(*session_id) {
                                self.close_reservation_id = Some(reservation_id.clone());
                                self.fail_close_inline_release(*session_id);
                                continue;
                            }
                            self.complete_inline_text_release(*session_id, reservation_id, true);
                            continue;
                        }
                        PendingAction::InlineTextCloseRelease { session_id } if error.code == ErrorCode::NotFound => {
                            if self.close_inline_session_id == Some(*session_id) { self.finalize_close_after_inline_cleanup(ctx); }
                            continue;
                        }
                        PendingAction::InlineTextCloseRelease { session_id } => {
                            self.fail_close_inline_release(*session_id);
                            continue;
                        }
                        // A path the host refused goes back into its form, unless a held edit keeps the change.
                        PendingAction::PathAdd { artifact_id, d, view_box } if apply_context.as_ref().is_none_or(|apply| apply.held_edit_id.is_none()) => {
                            tools::path_add_refused(ctx, artifact_id, d, view_box, &user_error(&error));
                        }
                        PendingAction::InlineTextCloseReservationCheck { session_id, .. } => {
                            self.fail_close_reservation_check(*session_id);
                            continue;
                        }
                        PendingAction::HeldEditReservations { held_id } => {
                            self.fail_held_edit_check(held_id, &error);
                            continue;
                        }
                        PendingAction::FigureAiImport { token, .. } => {
                            self.fail_figure_ai_import(token, &error);
                            continue;
                        }
                        PendingAction::FigureAiJob { token } => {
                            self.fail_figure_ai_job(token, &error);
                            continue;
                        }
                        _ => {}
                    }
                    if matches!(&action, PendingAction::Context | PendingAction::Inspect) {
                        self.reset_held_edit_checks("최신 작업물 상태를 읽지 못했습니다. 다시 확인하거나 변경을 취소하세요");
                    }
                    if matches!(&action, PendingAction::Context | PendingAction::Inspect | PendingAction::Reservations) {
                        self.fail_image_crop_refresh(pending.artifact_id.as_deref());
                    }
                    if let Some(apply) = apply_context.as_ref().filter(|apply| apply.held_edit_id.is_some()) {
                        let held_id = apply.held_edit_id.as_deref().expect("held edit id");
                        self.rehold_edit_after_failure(held_id, apply.clone(), &error);
                        if self.state_moved_on(&error, user_action) {
                            continue;
                        }
                        if error.code != ErrorCode::TargetBusy {
                            self.set_error(user_error(&error));
                        }
                        continue;
                    }
                    if let Some(run) = &ui_run {
                        let mut reported = run.prepare_error(error.clone());
                        if matches!(&action, PendingAction::HomeCreate { .. }) && change_result_unknown(&error) {
                            reported.retryable = false;
                            reported.message.push_str(" 작업물이 생성되고 작업이 시작됐을 수 있습니다. 다시 요청하기 전에 작업 목록을 확인해 주세요.");
                        }
                        self.complete_ui_run(&run.run_id, None, Some(reported));
                    }
                    match &action {
                        PendingAction::DesignSystemList => {
                            self.token_panel.design_systems_error = true;
                            self.token_panel.design_systems_loaded = false;
                            continue;
                        }
                        PendingAction::DesignSystemSource { generation, .. } => {
                            self.token_panel.fail_shared_source(*generation);
                            continue;
                        }
                        PendingAction::DesignSystemChange { .. } if error.code == ErrorCode::RevisionConflict => {
                            self.fetch_design_systems();
                        }
                        _ => {}
                    }
                    if let PendingAction::FlowFindPage { request } = &action {
                        self.flow_find_page_failed(request, &error);
                        continue;
                    }
                    if let PendingAction::PageReorder { token } = &action {
                        self.cancel_pending_page_reorder_selection(token);
                    }
                    if let PendingAction::Save { request_id, artifact_id } = &action {
                        if change_result_unknown(&error) {
                            if self.save_event_confirms(request_id, artifact_id) {
                                continue;
                            }
                            if let Some(pending) = self.pending_save_notices.get_mut(request_id) {
                                if pending.artifact_id.as_str() == artifact_id.as_str() {
                                    pending.outcome = SaveRpcOutcome::Unknown;
                                }
                            }
                        } else {
                            self.pending_save_notices.remove(request_id);
                        }
                    }
                    // The channel's own calls never raise a message; the next publish or poll retries.
                    match action {
                        PendingAction::UiPublish => {
                            self.ui.commands_sent = false;
                            self.ui.last_publish = None;
                            self.ui.state_seq = None;
                            self.ui.run_guard_version = None;
                            continue;
                        }
                        PendingAction::UiComplete | PendingAction::UiAsset { .. } | PendingAction::GetParkedJob { .. } | PendingAction::HostProbe => continue,
                        // A failed loss preview leaves the list empty; the export itself still dry-runs first.
                        PendingAction::ExportPreview { .. } => {
                            self.export_preview_pending = None;
                            continue;
                        }
                        // A region the core could not render leaves the whole-page texture on screen; it is not asked again
                        // for the same rectangle (the request key stays).
                        PendingAction::RenderRegion { .. } | PendingAction::RegionAsset { .. } => continue,
                        // An engine without a style list: the panel keeps the model's styles (the cache entry stays,
                        // so it is not asked again for this revision).
                        PendingAction::FlowStyles { .. } => continue,
                        // A template that does not fit the chosen size is explained under 만들기 on the home screen.
                        PendingAction::Create | PendingAction::HomeCreate { .. } if self.home_template_refused(&error) => continue,
                        _ => {}
                    }
                    // The declared import is checked column by column; its refusal names the column
                    // and the rule, which is the whole message the user needs.
                    let friendly = match action {
                        PendingAction::CsvDeclaredImport => error.message.clone(),
                        // A document that cannot take a picture says why in its own words.
                        PendingAction::FigureInfo { .. }
                        | PendingAction::FigureRender { .. }
                        | PendingAction::LinkApply => links::refusal(&error),
                        // A page the reader did not read safely names the page and the reader's diagnostics.
                        PendingAction::PdfEvidenceImport => figure_tools::pdf_read_failure_message(&error),
                        // A proposal's apply whose answer was lost may have landed; a refusal says why (proposals.rs).
                        PendingAction::Apply if method == "proposal.apply" => proposals::apply_failure_message(&error),
                        _ => user_error(&error),
                    };
                    if matches!(action, PendingAction::LinkApply | PendingAction::Apply | PendingAction::ApplyWithSelectionIfUnchanged { .. })
                        && self.select_after_commit_ticket.is_none_or(|ticket| ticket == reply.ticket)
                    {
                        self.select_after_commit = None;
                        self.select_after_commit_ticket = None;
                        self.select_after_commit_guard = None;
                    }
                    if self.smoke_output.is_some() && self.smoke_failure.is_none() {
                        self.smoke_failure = Some(smoke::smoke_failure_text(&method, &friendly, &error));
                    }
                    self.smoke_note_pdf_refusal(&action, &method, &friendly, &error);
                    if matches!(&action, PendingAction::Export)
                        && error.code == ErrorCode::ExportLossRequired
                    {
                        if let Some(request) = self.pending_export.take() {
                            self.export_confirmation = Some(ExportConfirmation {
                                request,
                                diagnostics: error.diagnostics.clone(),
                                allowed: BTreeSet::new(),
                            });
                            continue;
                        }
                    }
                    if let PendingAction::Render { page_index } = &action {
                        self.requested_pages.remove(page_index);
                        if self.inline_text_edit.as_ref().is_some_and(|session| {
                            session.page_index == *page_index && session.reservation_node_ids.len() > 1
                        }) {
                            // A render of a revision that moved on is asked again once the document is read again; any
                            // other failure keeps the cell's queued input as the draft (68 item 11).
                            if matches!(error.code, ErrorCode::RevisionConflict | ErrorCode::StaleRender | ErrorCode::RenderPending) {
                                self.refresh_active();
                            } else {
                                self.inline_cell_render_failed(*page_index);
                            }
                        }
                    }
                    // A reserved node refuses the edit: the callout names the node, and the status line stays clear.
                    let on_node = error.code == ErrorCode::TargetBusy;
                    if matches!(action, PendingAction::Apply | PendingAction::TokenChange { .. } | PendingAction::ApplyWithSelectionIfUnchanged { .. }) {
                        let kind = self.active.as_ref().map_or(ArtifactKind::Canvas, |item| item.kind);
                        let (line, fixes) = selection::refusal_for(&error, &friendly, kind, self.artifact.as_ref());
                        self.refusal_fixes = if on_node { Vec::new() } else { fixes };
                        if on_node {
                            if let Some(apply) = apply_context.filter(|apply| apply.user_edit) {
                                self.hold_direct_edit(apply, error.affected_ids.clone(), &error);
                            } else {
                                self.refusal = Some(self.busy_reason(&error.affected_ids));
                                self.refusal_ids = error.affected_ids.clone();
                                self.held_refusal_id = None;
                            }
                        } else {
                            self.refusal = Some(line);
                            self.refusal_ids.clear();
                            self.held_refusal_id = None;
                        }
                    }
                    self.pdf_evidence_import_failed(&action, reply.ticket, &friendly);
                    self.release_drag_on_failure(&action);
                    self.forget_core_transform_on_failure(&action);
                    if self.state_moved_on(&error, user_action) {
                        continue;
                    }
                    if on_node && matches!(action, PendingAction::Apply | PendingAction::TokenChange { .. } | PendingAction::ApplyWithSelectionIfUnchanged { .. }) {
                        continue;
                    }
                    if matches!(action, PendingAction::StopForEdit) {
                        continue;
                    }
                    self.raise_error(Some(&method), friendly);
                }
                Ok(payload) => {
                    // An answer to a request the window no longer holds changes nothing, so the notice on screen stays.
                    if !self.reply_is_superseded(&action, reply.ticket) {
                        self.notices.succeeded(&method);
                        if user_action {
                            self.notices.clear_transient();
                        }
                    }
                    if matches!(action, PendingAction::MaterializeWeb) {
                        if let Err(error) = self.open_materialized_web_preview(payload, ui_run.as_deref()) {
                            self.set_error(error.message.clone());
                            if let Some(run) = ui_run {
                                self.complete_ui_run(&run.run_id, None, Some(error));
                            }
                        }
                        continue;
                    }
                    if action.prepares_figure_change() {
                        if let Some(mut run) = ui_run {
                            if let Some(error) = self.ui_expected_refusal(run.expected.as_ref()) {
                                self.complete_ui_run(&run.run_id, None, Some(run.prepare_error(error)));
                                continue;
                            }
                            // The render/import prepares data. Only the final document commit completes this run.
                            run.rpc_ticket = None;
                            self.run_context = Some(*run);
                            self.handle_payload(ctx, reply.ticket, action, payload);
                            let mut run = self.run_context.take().expect("handling a figure preparation");
                            if run.rpc_ticket.is_none() {
                                let error = run.enqueue_error.take().unwrap_or_else(|| DltError::new(
                                    ErrorCode::InvalidArgument, "그림을 준비했지만 다음 요청을 보낼 수 없어 변경을 적용하지 않았습니다",
                                ));
                                self.complete_ui_run(&run.run_id, None, Some(run.prepare_error(error)));
                            }
                            continue;
                        }
                    }
                    let ui_result = ui_run.as_ref().map(|run| ui_run_success_result(run, &payload));
                    if let Some(held_id) = apply_context.as_ref().and_then(|apply| apply.held_edit_id.as_deref()) {
                        self.finish_held_edit_retry(held_id);
                    }
                    if let PendingAction::PathAdd { artifact_id, d, view_box } = &action {
                        tools::path_add_placed(ctx, artifact_id, d, view_box);
                    }
                    self.handle_payload(ctx, reply.ticket, action, payload);
                    if let (Some(run), Some(mut result)) = (&ui_run, ui_result) {
                        if matches!(run.command_id.as_str(), "nav.open_file" | "home.create" | "home.use_template") {
                            // A user can start an edit while the host creates the artifact; activate() then keeps the old view.
                            let Some(opened) = result.get("id").or_else(|| result.pointer("/artifact/id")).and_then(Value::as_str) else {
                                self.complete_ui_run(&run.run_id, None, Some(DltError::new(ErrorCode::IntegrityError, "작업물 ID를 확인할 수 없습니다. 작업 목록을 확인해 주세요")));
                                continue;
                            };
                            result["opened_in_view"] = json!(self.active.as_ref().is_some_and(|active| active.id == opened));
                        }
                        self.complete_ui_run(&run.run_id, Some(result), None);
                    }
                }
            }
        }
        self.show_dropped_unknown_starts();
        self.resume_selection_after_reconnect();
    }

    /// Whether an answer is to a request the window no longer holds: a PDF import that was replaced or discarded, or an evidence
    /// import or job start of a Figure AI request that ended.
    pub(super) fn reply_is_superseded(&self, action: &PendingAction, ticket: u64) -> bool {
        match action {
            PendingAction::PdfAssetImport { .. } | PendingAction::PdfEvidenceImport => !self.pdf_reply_is_current(ticket),
            PendingAction::FigureAiImport { token, .. } | PendingAction::FigureAiJob { token } => !self.figure_ai_request_holds(token),
            _ => false,
        }
    }

    /// The host moved past what this window knew (another editor committed, a render expired).
    /// Reload quietly instead of reporting a failure nobody caused; a user's own request that lost
    /// the race still gets its message, because their edit was not applied.
    pub(super) fn state_moved_on(&mut self, error: &DltError, user_action: bool) -> bool {
        if !matches!(
            error.code,
            ErrorCode::StaleRender | ErrorCode::RevisionConflict | ErrorCode::RenderPending
        ) {
            return false;
        }
        let due = self
            .last_resync
            .is_none_or(|at| at.elapsed() >= Duration::from_secs(1));
        if !due {
            return false;
        }
        self.last_resync = Some(Instant::now());
        self.refresh_active();
        !user_action
    }

    pub(super) fn release_drag_on_failure(&mut self, action: &PendingAction) {
        if let PendingAction::ReserveDrag { token } = action {
            if self.drag.as_ref().is_some_and(|drag| drag.token == *token) {
                self.drag = None;
            }
        }
    }

    /// A change set (or undo/redo) was committed by the host: adopt its revision and reload.
    pub(super) fn committed(&mut self, value: &Value) {
        self.adopt_commit(value, true);
    }

    /// `committed` for an evidence change, which leaves the figure's tables as they were: a cell draft the user started
    /// while the change was on its way stays. `draw_dataset_cell_editor` still drops it when its table has changed.
    pub(super) fn committed_keeping_cell_draft(&mut self, value: &Value) {
        self.adopt_commit(value, false);
    }

    pub(super) fn adopt_commit(&mut self, value: &Value, clear_cell_draft: bool) {
        let publisher_state_changed = value
            .get("changed_ids")
            .and_then(Value::as_array)
            .is_some_and(|changed| changed.iter().any(|id| id.as_str() == Some("artifact:publishes_design_system")));
        let refresh_design_systems = publisher_state_changed
            || self.artifact.as_ref().is_some_and(|artifact| artifact.publishes_design_system);
        if let Some(revision) = value.get("revision").and_then(Value::as_u64) {
            if let Some(active) = self.active.as_mut() {
                active.revision = revision;
                active.dirty = true;
            }
        }
        self.drag = None;
        if clear_cell_draft {
            self.dataset_edit = None;
        }
        self.show_engine_diagnostics(value);
        self.refresh_active();
        if refresh_design_systems {
            self.fetch_design_systems();
        }
    }
}
