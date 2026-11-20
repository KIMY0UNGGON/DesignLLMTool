use super::*;
use super::{
    doc_tree::{canvas_table_anchor_for_covered_cell, find_node, find_paragraph, global_caret, inline_text_target_node_ids, last_text_position, paragraph_text, slide_text_position},
    state::inline_text_actor,
};

impl DesktopApp {
    pub(super) fn sync_text_target(&mut self) {
        if self.inline_text_edit.is_some() {
            return;
        }
        let Some(artifact) = self.artifact.as_ref() else {
            self.text_target = None;
            return;
        };

        let target = if let ArtifactBody::WebCode(document) = &artifact.body {
            let path = self.open_web_path(document).to_owned();
            document.files.get(&path).map(|file| {
                (
                    TextTarget::WebFile {
                        path,
                        expected_hash: file.sha256.clone(),
                        original: file.content.clone(),
                        editable: self.feature_edit_supported(Feature::WebSource),
                    },
                    file.content.clone(),
                )
            })
        } else if let Some(range) = self
            .context
            .as_ref()
            .and_then(|context| context.selection.text_range.as_ref())
        {
            find_paragraph(artifact, &range.paragraph_id).map(|paragraph| {
                let text = paragraph_text(paragraph);
                (
                    TextTarget::Paragraph {
                        paragraph_id: paragraph.id.clone(),
                        original: text.clone(),
                        editable: paragraph.runs.iter().all(|run| !run.protected)
                            && self.feature_edit_supported(Feature::Text),
                    },
                    text,
                )
            })
        } else {
            self.selected_node
                .as_deref()
                .and_then(|id| self.selection_text_target(artifact, id))
        };

        let Some((target, text)) = target else {
            self.text_target = None;
            self.text_editor_focused = false;
            return;
        };
        if self.text_target.as_ref() != Some(&target) {
            self.text_draft = text;
            self.text_target = Some(target);
            self.text_editor_focused = false;
            self.text_adapter = GraphemeAdapter::default();
            self.sync_paragraph_fields();
        }
    }

    pub(super) fn inline_text_operation(session: &InlineTextEdit, edit: &TextInputEdit) -> Result<Operation, String> {
        if session.kind == ArtifactKind::Canvas {
            return Ok(Operation::ReplaceText {
                range: TextRange { paragraph_id: session.node_id.clone(), start: edit.range.start, end: edit.range.end },
                text: edit.text.clone(),
            });
        }
        let range = edit.paragraph_range.as_ref().ok_or_else(|| "슬라이드의 여러 문단을 가로지르는 편집은 지원되지 않습니다".to_owned())?;
        Ok(Operation::ReplaceText {
            range: TextRange {
                paragraph_id: format!("{}#p{}", session.node_id, edit.paragraph_index),
                start: range.start,
                end: range.end,
            },
            text: edit.text.clone(),
        })
    }

    pub(super) fn queue_inline_text_edits(&mut self, edits: Vec<TextInputEdit>) {
        let Some(session) = self.inline_text_edit.as_mut() else { return };
        for mut edit in edits {
            if edit.ime_commit {
                if edit.ime_generation != session.ime_generation {
                    session.conflict = Some("IME 조합 상태가 바뀌었습니다. 입력을 유지했습니다".into());
                    session.retry_allowed = false;
                    return;
                }
                session.composing = false;
                session.ime_commit_pending = true;
            }
            if edit.request_id.is_empty() {
                edit.request_id = new_id();
            }
            session.queue.push_back(edit);
        }
        if self.inline_text_edit.as_ref().is_some_and(|session| session.conflict.is_none()) {
            self.start_next_inline_text_change();
        }
    }

    pub(super) fn process_inline_text_adapter_input(&mut self, ctx: &Context, input: &mut egui::RawInput) {
        // A key that moves a table cell's edit to another cell ends this edit: the rest of the frame's input is not this cell's.
        if self.take_inline_cell_navigation(ctx, input) {
            input.events.retain(|event| !matches!(event,
                egui::Event::Text(_) | egui::Event::Paste(_) | egui::Event::Ime(_) | egui::Event::Cut | egui::Event::Key { pressed: true, .. }
            ));
            return;
        }
        let _ = self.text_adapter.raw_input_hook(ctx, input, self.text_editor_id, &mut self.text_draft);
        let ime_started = self.text_adapter.take_ime_started();
        let ime_finished = self.text_adapter.take_ime_finished();
        let ime_cancelled = self.text_adapter.take_ime_cancelled();
        let edits = self.text_adapter.take_input_edits();
        if ime_started {
            if let Some(session) = self.inline_text_edit.as_mut() {
                session.composing = true;
                session.ime_generation = self.text_adapter.ime_generation();
            }
        }
        if ime_cancelled {
            if let Some(session) = self.inline_text_edit.as_mut() {
                session.composing = false;
                session.ime_cancelled_frame = true;
            }
        } else if ime_finished {
            if let Some(session) = self.inline_text_edit.as_mut() {
                session.composing = false;
            }
        }
        self.queue_inline_text_edits(edits);
        if ime_finished || ime_cancelled {
            if let Some(session_id) = self.inline_text_edit.as_ref().map(|session| session.id) {
                self.release_inline_text_target(session_id);
            }
        }
    }

    pub(super) fn start_next_inline_text_change(&mut self) {
        let Some(session) = self.inline_text_edit.as_ref() else { return };
        if session.reservation_node_ids.len() > 1 {
            let session_id = session.id;
            let readiness = self.inline_table_cell_readiness(session);
            if !self.apply_inline_cell_readiness(session_id, readiness) { return; }
        }
        let Some(session) = self.inline_text_edit.as_ref() else { return };
        if session.in_flight.is_some() || session.conflict.is_some() || session.reservation_pending || session.reservation_release_pending {
            return;
        }
        if (session.composing || session.queue.front().is_some_and(|edit| edit.ime_commit)) && session.reservation_id.is_none() {
            self.reserve_inline_text_target();
            return;
        }
        if session.composing && session.queue.is_empty() {
            self.reserve_inline_text_target();
            return;
        }
        let Some(edit) = session.queue.front().cloned() else {
            let finish = session.finish_requested && !session.ime_commit_pending;
            if finish { self.finish_inline_text_edit(); }
            return;
        };
        if edit.ime_commit && session.reservation_id.is_none() {
            let session_id = session.id;
            if let Some(session) = self.inline_text_edit.as_mut().filter(|session| session.id == session_id) {
                session.conflict = Some("IME 조합 대상 예약을 확인하지 못했습니다. 입력을 유지했습니다".into());
                session.retry_allowed = false;
            }
            return;
        }
        let operation = match Self::inline_text_operation(session, &edit) {
            Ok(operation) => operation,
            Err(message) => {
                let session_id = session.id;
                if let Some(session) = self.inline_text_edit.as_mut() {
                    session.conflict = Some(message);
                    session.retry_allowed = false;
                }
                self.release_inline_text_target_now(session_id);
                return;
            }
        };
        let (session_id, revision, artifact_id, actor) = (session.id, session.revision, session.artifact_id.clone(), session.actor.clone());
        let Some((active_id, workspace_id)) = self.active.as_ref().zip(self.context.as_ref())
            .filter(|(active, context)| active.id == artifact_id && context.artifact_id == artifact_id)
            .map(|(active, context)| (active.id.clone(), context.workspace_id.clone())) else {
            if let Some(session) = self.inline_text_edit.as_mut() {
                session.conflict = Some("작업공간 연결이 바뀌어 입력을 적용하지 않았습니다".into());
                session.retry_allowed = false;
            }
            return;
        };
        if active_id != artifact_id { return; }
        {
            let Some(session) = self.inline_text_edit.as_mut().filter(|session| session.id == session_id) else { return };
            let Some(edit) = session.queue.pop_front() else { return };
            session.in_flight = Some(edit);
        }
        let edit = self.inline_text_edit.as_ref().and_then(|session| session.in_flight.as_ref()).expect("change is in flight");
        let change = ChangeSet {
            request_id: edit.request_id.clone(),
            actor,
            workspace_id,
            artifact_id,
            base_revision: revision,
            allowed_intents: [Intent::Content].into_iter().collect(),
            job_ref: None,
            read_dependencies: Vec::new(),
            operations: vec![operation],
        };
        let params = match serde_json::to_value(change) {
            Ok(params) => params,
            Err(_) => {
                if let Some(session) = self.inline_text_edit.as_mut() {
                    if let Some(edit) = session.in_flight.take() { session.queue.push_front(edit); }
                    session.conflict = Some("텍스트 입력 요청을 만들지 못했습니다. 입력은 유지했습니다".into());
                }
                return;
            }
        };
        let ticket = self.call("changes.apply", params, PendingAction::InlineTextChange { session_id });
        if ticket.is_none() {
            if let Some(session) = self.inline_text_edit.as_mut() {
                if let Some(edit) = session.in_flight.take() { session.queue.push_front(edit); }
                session.conflict = Some("텍스트 입력을 전송하지 못했습니다. 입력은 유지했습니다".into());
                session.retry_allowed = false;
            }
        }
    }

    pub(super) fn reserve_inline_text_target(&mut self) {
        self.request_inline_text_reservation(false);
    }

    pub(super) fn request_inline_text_reservation(&mut self, renew: bool) {
        if self.inline_text_edit.as_ref().is_some_and(|session| session.reservation_release_pending) {
            return;
        }
        let Some(session) = self.inline_text_edit.as_mut() else { return };
        if (!renew && session.reservation_id.is_some()) || session.reservation_pending || !(session.composing || session.ime_commit_pending) {
            return;
        }
        session.reservation_pending = true;
        let id = session.id;
        let request = json!({
            "artifact_id": session.artifact_id,
            "expected_revision": session.revision,
            "view_id": session.view_id,
            "owner": session.actor,
            "node_ids": session.reservation_node_ids,
            "ttl_ms": 300000,
        });
        if self.call("targets.reserve", request, PendingAction::InlineTextReserve { session_id: id }).is_none() {
            if let Some(session) = self.inline_text_edit.as_mut().filter(|session| session.id == id) {
                session.reservation_pending = false;
                session.reservation_expires = None;
                session.conflict = Some("텍스트 편집 예약을 전송하지 못했습니다. 입력을 유지했습니다".into());
                session.retry_allowed = true;
                session.retry_expected = false;
                if !self.text_adapter.is_ime_active() {
                    session.composing = false;
                }
            }
        }
    }

    pub(super) fn renew_inline_text_target_if_needed(&mut self) {
        let due = self.inline_text_edit.as_ref().is_some_and(|session| {
            (session.composing || session.ime_commit_pending) && session.reservation_id.is_some() && !session.reservation_pending
                && session.reservation_expires.is_some_and(|expires| expires <= Instant::now() + Duration::from_secs(60))
        });
        if due {
            self.request_inline_text_reservation(true);
        }
    }

    pub(super) fn release_inline_text_target(&mut self, session_id: u64) {
        let reservation_id = {
            let Some(session) = self.inline_text_edit.as_mut().filter(|session| session.id == session_id) else { return };
            session.release_when_reserved = true;
            if session.reservation_pending || session.composing || session.ime_commit_pending
                || session.in_flight.is_some() || !session.queue.is_empty() || session.reservation_release_pending
            {
                return;
            }
            session.release_when_reserved = false;
            session.reservation_id.take()
        };
        if let Some(reservation_id) = reservation_id {
            self.release_inline_reservation(session_id, reservation_id);
        }
    }

    pub(super) fn release_inline_text_target_now(&mut self, session_id: u64) {
        let reservation_id = {
            let Some(session) = self.inline_text_edit.as_mut().filter(|session| session.id == session_id) else { return };
            if session.reservation_release_pending { return; }
            let reservation_id = session.reservation_id.take();
            session.release_when_reserved = session.reservation_pending;
            session.reservation_expires = None;
            reservation_id
        };
        if let Some(reservation_id) = reservation_id {
            self.release_inline_reservation(session_id, reservation_id);
        }
    }

    pub(super) fn finish_inline_text_edit(&mut self) {
        let id = {
            let Some(session) = self.inline_text_edit.as_mut() else { return };
            if session.in_flight.is_some() || !session.queue.is_empty() || session.composing || session.reservation_pending
                || session.reservation_release_pending || session.conflict.is_some()
            {
                session.finish_requested = true;
                return;
            }
            session.finish_requested = true;
            session.id
        };
        self.release_inline_text_target(id);
        if self.inline_text_edit.as_ref().is_some_and(|session| session.id == id
            && (session.reservation_id.is_some() || session.reservation_release_pending || session.reservation_release_failed))
        {
            return;
        }
        self.clear_inline_draft(id);
        self.inline_text_edit = None;
        self.text_editor_focused = false;
        self.refresh_active();
    }

    pub(super) fn discard_inline_text_draft(&mut self) {
        let id = {
            let Some(session) = self.inline_text_edit.as_mut() else { return };
            if session.in_flight.is_some() || session.reservation_pending || session.reservation_release_pending
                || session.composing || self.text_adapter.is_ime_active()
            {
                return;
            }
            self.text_draft = session.committed_text.clone();
            session.queue.clear();
            session.ime_commit_pending = false;
            session.finish_requested = true;
            session.conflict = None;
            session.retry_allowed = false;
            session.id
        };
        self.release_inline_text_target(id);
        if self.inline_text_edit.as_ref().is_some_and(|session| session.id == id
            && !session.reservation_pending && !session.reservation_release_pending
            && session.reservation_id.is_none() && !session.reservation_release_failed)
        {
            self.clear_inline_draft(id);
            self.inline_text_edit = None;
            self.text_editor_focused = false;
            self.refresh_active();
        }
    }

    pub(super) fn retry_inline_text_edits(&mut self) {
        let Some(session) = self.inline_text_edit.as_ref() else { return };
        if session.reservation_node_ids.len() > 1 {
            let session_id = session.id;
            let readiness = self.inline_table_cell_readiness(session);
            if !self.apply_inline_cell_readiness(session_id, readiness) { return; }
        }
        let Some(session) = self.inline_text_edit.as_ref() else { return };
        if !session.retry_allowed || session.queue.is_empty() || session.in_flight.is_some() || session.composing
            || session.reservation_pending || session.reservation_release_pending
        { return; }
        let (session_id, artifact_id, node_id, committed_text, expected_targets, kind, page_index, needs_ime_reservation, retry_same_request) = (
            session.id,
            session.artifact_id.clone(),
            session.node_id.clone(),
            session.committed_text.clone(),
            session.reservation_node_ids.clone(),
            session.kind,
            session.page_index,
            session.ime_commit_pending && session.reservation_id.is_none(),
            session.retry_same_request,
        );
        let current_text = self.artifact.as_ref().filter(|artifact| artifact.id == artifact_id)
            .and_then(|artifact| find_node(artifact, &node_id)).map(|node| node.text.as_str());
        let current_targets = self.artifact.as_ref().filter(|artifact| artifact.id == artifact_id)
            .and_then(|artifact| inline_text_target_node_ids(artifact, kind, page_index, &node_id));
        if !retry_same_request {
            let synced = self.active.as_ref().zip(self.artifact.as_ref()).zip(self.context.as_ref()).is_some_and(|((active, artifact), context)| {
                active.id == artifact_id && artifact.id == artifact_id && context.artifact_id == artifact_id
                    && active.revision == artifact.revision && artifact.revision == context.revision
            });
            if !synced {
                self.refresh_active();
                return;
            }
        }
        if !retry_same_request && current_text != Some(committed_text.as_str()) {
            if let Some(session) = self.inline_text_edit.as_mut().filter(|session| session.id == session_id) {
                session.retry_allowed = false;
                session.conflict = Some("작업물이 다른 텍스트로 바뀌었습니다. 초안을 버리거나 복사하세요".into());
            }
            return;
        }
        if !retry_same_request && current_targets.as_ref() != Some(&expected_targets) {
            if let Some(session) = self.inline_text_edit.as_mut().filter(|session| session.id == session_id) {
                session.retry_allowed = false;
                session.conflict = Some("편집 대상 표, 소유자 또는 잠금 상태가 바뀌었습니다. 입력을 복사하거나 버리세요".into());
            }
            return;
        }
        let Some(active) = self.active.as_ref().filter(|active| active.id == artifact_id) else { return };
        let revision = active.revision;
        if let Some(session) = self.inline_text_edit.as_mut().filter(|session| session.id == session_id) {
            if !retry_same_request {
                session.revision = revision;
                if let Some(edit) = session.queue.front_mut() {
                    edit.request_id = new_id();
                }
            }
            session.conflict = None;
            session.retry_allowed = false;
            session.retry_expected = false;
            session.retry_same_request = false;
            session.composing = needs_ime_reservation;
        }
        if let Some(selection) = self.local_selection.clone() {
            self.set_selection(selection);
        }
        if needs_ime_reservation {
            self.reserve_inline_text_target();
            return;
        }
        self.start_next_inline_text_change();
    }

    pub(super) fn complete_inline_text_change(&mut self, session_id: u64, value: &Value) {
        let Some(revision) = value.get("revision").and_then(Value::as_u64) else {
            self.fail_inline_text_change(session_id, &DltError::new(ErrorCode::IntegrityError, "텍스트 변경 결과에 revision이 없습니다"));
            return;
        };
        if self.inline_text_edit.as_ref().is_none_or(|session| session.id != session_id) {
            self.refresh_active();
            return;
        }
        let result = {
            let Some(session) = self.inline_text_edit.as_mut().filter(|session| session.id == session_id) else { return };
            let Some(edit) = session.in_flight.take() else { return };
            if revision <= session.revision || !apply_grapheme_edit(&mut session.committed_text, &edit) {
                session.conflict = Some("텍스트 입력 결과가 현재 상태와 맞지 않습니다. 입력을 유지했습니다".into());
                session.retry_allowed = false;
                session.in_flight = Some(edit);
                None
            } else {
                session.revision = revision;
                let kind = session.kind;
                let node_id = session.node_id.clone();
                let artifact_id = session.artifact_id.clone();
                let page_index = session.page_index;
                let table_cell = session.reservation_node_ids.len() > 1;
                let ime_commit = edit.ime_commit;
                let range = if kind == ArtifactKind::Canvas {
                    TextRange { paragraph_id: node_id.clone(), start: edit.caret_after, end: edit.caret_after }
                } else {
                    TextRange {
                        paragraph_id: format!("{}#p{}", node_id, edit.paragraph_after),
                        start: edit.caret_after_in_paragraph,
                        end: edit.caret_after_in_paragraph,
                    }
                };
                if ime_commit { session.ime_commit_pending = false; }
                session.retry_same_request = false;
                let should_release = (session.release_when_reserved || session.reservation_id.is_some()) && session.queue.is_empty()
                    && !session.composing && !session.ime_commit_pending;
                Some((artifact_id, node_id, page_index, table_cell, ime_commit, Selection { node_ids: vec![session.node_id.clone()], text_range: Some(range) }, session.finish_requested && session.queue.is_empty(), should_release))
            }
        };
        let Some((artifact_id, node_id, page_index, table_cell, ime_commit, selection, should_finish, should_release)) = result else {
            self.refresh_active();
            return;
        };
        // A confirmed change of a session that is closing: its editor shows the confirmed text, so no draft of the text before it is
        // written from the old editor text (`fail_inline_text_change` does the same for a refused change).
        if self.close_after_inline_cleanup && self.close_inline_session_id == Some(session_id) {
            if let Some(session) = self.inline_text_edit.as_ref() {
                self.text_draft = session.committed_text.clone();
            }
        }

        if self.active.as_ref().is_some_and(|active| active.id == artifact_id && active.revision > revision) {
            if let Some(session) = self.inline_text_edit.as_mut().filter(|session| session.id == session_id) {
                session.conflict = Some("입력은 확정됐지만 문서가 더 변경됐습니다. 최신 텍스트를 확인한 뒤 계속하세요".into());
                session.retry_expected = true;
                session.retry_allowed = false;
                session.retry_same_request = false;
            }
            self.release_inline_text_target_now(session_id);
            self.refresh_active();
            return;
        }

        if let Some(active) = self.active.as_mut().filter(|active| active.id == artifact_id) {
            active.revision = revision;
            active.dirty = true;
        }
        if let Some(context) = self.context.as_mut().filter(|context| context.artifact_id == artifact_id) {
            context.revision = revision;
            context.selection = selection.clone();
            context.visible_page = page_index;
        }
        self.page_index = page_index;
        self.view().page_index = page_index;
        self.set_selected_ids(vec![node_id]);
        self.set_selection(selection);

        if table_cell {
            self.refresh_active();
        }

        if ime_commit || should_release {
            self.release_inline_text_target(session_id);
        }
        if should_finish {
            self.finish_inline_text_edit();
        } else {
            self.start_next_inline_text_change();
        }
    }

    pub(super) fn fail_inline_text_change(&mut self, session_id: u64, error: &DltError) {
        if self.close_after_inline_cleanup && self.close_inline_session_id == Some(session_id) {
            let committed_text = {
                let Some(session) = self.inline_text_edit.as_mut().filter(|session| session.id == session_id) else { return };
                session.in_flight = None;
                session.queue.clear();
                session.composing = false;
                session.ime_commit_pending = false;
                session.conflict = None;
                session.retry_allowed = false;
                session.retry_same_request = false;
                session.release_when_reserved = true;
                session.committed_text.clone()
            };
            self.text_draft = committed_text;
            self.text_adapter = GraphemeAdapter::default();
            self.text_editor_focused = false;
            self.refresh_active();
            return;
        }
        let release = {
            let Some(session) = self.inline_text_edit.as_mut().filter(|session| session.id == session_id) else { return };
            if let Some(edit) = session.in_flight.take() {
                session.queue.push_front(edit);
            }
            session.conflict = Some(user_error(error));
            session.retry_same_request = matches!(error.code, ErrorCode::Io | ErrorCode::Internal | ErrorCode::HostNotRunning | ErrorCode::Unavailable | ErrorCode::StorageFailure);
            session.retry_expected = matches!(error.code, ErrorCode::TargetBusy | ErrorCode::RevisionConflict);
            session.retry_allowed = session.retry_same_request;
            session.composing = false;
            session.ime_commit_pending = session.queue.front().is_some_and(|edit| edit.ime_commit);
            !session.retry_same_request
        };
        if release {
            self.release_inline_text_target_now(session_id);
        }
        self.refresh_active();
    }

    pub(super) fn fail_inline_text_reservation(&mut self, session_id: u64, error: &DltError) {
        if self.close_after_inline_cleanup && self.close_inline_session_id == Some(session_id) {
            let Some((view_id, actor, node_id, artifact_id, committed_text, reservation_id)) = self.inline_text_edit.as_mut()
                .filter(|session| session.id == session_id)
                .map(|session| {
                    session.reservation_pending = false;
                    session.reservation_expires = None;
                    session.composing = false;
                    session.ime_commit_pending = false;
                    session.queue.clear();
                    session.conflict = None;
                    session.release_when_reserved = true;
                    (session.view_id.clone(), session.actor.clone(), session.node_id.clone(), session.artifact_id.clone(), session.committed_text.clone(), session.reservation_id.clone())
                }) else { return };
            self.text_draft = committed_text;
            self.text_adapter = GraphemeAdapter::default();
            self.text_editor_focused = false;
            if self.close_reservation_id.is_none() { self.close_reservation_id = reservation_id; }
            self.check_close_inline_reservation(session_id, view_id, actor, node_id, artifact_id);
            self.refresh_active();
            return;
        }
        let reservation_id = {
            let Some(session) = self.inline_text_edit.as_mut().filter(|session| session.id == session_id) else {
                self.refresh_active();
                return;
            };
            session.reservation_pending = false;
            session.reservation_expires = None;
            session.conflict = Some(user_error(error));
            session.retry_expected = error.code == ErrorCode::TargetBusy || error.code == ErrorCode::RevisionConflict;
            session.retry_allowed = false;
            if !self.text_adapter.is_ime_active() {
                session.composing = false;
            }
            session.reservation_id.take()
        };
        if let Some(reservation_id) = reservation_id {
            self.release_inline_reservation(session_id, reservation_id);
        }
        self.refresh_active();
    }

    pub(super) fn complete_inline_text_reservation(&mut self, session_id: u64, value: &Value) {
        let Some(reservation_id) = value.get("reservation_id").and_then(Value::as_str).map(str::to_owned) else {
            self.fail_inline_text_reservation(session_id, &DltError::new(ErrorCode::IntegrityError, "텍스트 예약 응답이 올바르지 않습니다"));
            return;
        };
        if self.inline_text_edit.as_ref().is_none_or(|session| session.id != session_id) {
            self.release_inline_reservation(session_id, reservation_id);
            return;
        }
        let expires = value.get("expires_in_ms").and_then(Value::as_u64)
            .map(|millis| Instant::now() + Duration::from_millis(millis));
        if self.close_after_inline_cleanup && self.close_inline_session_id == Some(session_id) {
            if let Some(session) = self.inline_text_edit.as_mut().filter(|session| session.id == session_id) {
                session.reservation_pending = false;
                session.reservation_expires = expires;
                session.reservation_id = Some(reservation_id);
                session.composing = false;
                session.ime_commit_pending = false;
                session.queue.clear();
                session.conflict = None;
                session.finish_requested = true;
                session.release_when_reserved = true;
            }
            return;
        }
        let (release, start) = {
            let session = self.inline_text_edit.as_mut().expect("session checked");
            session.reservation_pending = false;
            session.reservation_expires = expires;
            session.reservation_id = Some(reservation_id.clone());
            if !session.composing && !session.ime_commit_pending && session.queue.is_empty() && session.in_flight.is_none() {
                session.release_when_reserved = false;
                session.reservation_id = None;
                (true, false)
            } else {
                (false, session.conflict.is_none())
            }
        };
        if release {
            self.release_inline_reservation(session_id, reservation_id);
            return;
        }
        if start {
            self.start_next_inline_text_change();
        }
    }

    pub(super) fn reconcile_inline_text_after_refresh(&mut self) {
        let Some(session) = self.inline_text_edit.as_ref().filter(|session| {
            session.in_flight.is_none() && !session.retry_same_request
                && (session.conflict.is_some() && session.retry_expected
                    || session.reservation_node_ids.len() > 1
                        && self.active.as_ref().is_some_and(|active| active.id == session.artifact_id && active.revision != session.revision))
        }) else { return };
        let (session_id, artifact_id, node_id, page_index, kind, session_revision, expected, expected_targets) = (
            session.id, session.artifact_id.clone(), session.node_id.clone(), session.page_index, session.kind,
            session.revision, session.committed_text.clone(), session.reservation_node_ids.clone(),
        );
        let table_cell = session.reservation_node_ids.len() > 1;
        let needs_explicit_retry = session.retry_expected || session.conflict.is_some();
        let synced = self.active.as_ref().zip(self.artifact.as_ref()).zip(self.context.as_ref()).is_some_and(|((active, artifact), context)| {
            active.id == artifact_id && artifact.id == artifact_id && context.artifact_id == artifact_id
                && active.revision == artifact.revision && artifact.revision == context.revision
        });
        if !synced { return; }
        let current = self.artifact.as_ref().filter(|artifact| artifact.id == artifact_id)
            .and_then(|artifact| find_node(artifact, &node_id));
        let current_targets = self.artifact.as_ref().filter(|artifact| artifact.id == artifact_id)
            .and_then(|artifact| inline_text_target_node_ids(artifact, kind, page_index, &node_id));
        let revision = self.active.as_ref().filter(|active| active.id == artifact_id).map(|active| active.revision);
        let selection_current = !table_cell || self.inline_text_edit.as_ref()
            .filter(|session| session.id == session_id)
            .is_some_and(|session| self.inline_table_cell_selection_current(session));
        let scene_current = !table_cell || self.inline_text_edit.as_ref()
            .filter(|session| session.id == session_id)
            .is_some_and(|session| self.inline_table_cell_scene_current(session));
        if table_cell && revision.is_some_and(|revision| revision != session_revision) {
            if let Some(session) = self.inline_text_edit.as_mut().filter(|session| session.id == session_id) {
                session.layout_pending = false;
                session.retry_allowed = false;
                session.retry_expected = false;
                session.conflict = Some("문서 revision이 바뀌어 같은 ID가 같은 표 셀인지 확인할 수 없습니다. 초안을 복사한 뒤 최신 셀에서 다시 입력하세요".into());
            }
            return;
        }
        let same_text = current_targets.as_ref() == Some(&expected_targets)
            && current.is_some_and(|node| node.text == expected);
        if let Some(session) = self.inline_text_edit.as_mut().filter(|session| session.id == session_id) {
            if same_text && selection_current && scene_current {
                if let Some(revision) = revision { session.revision = revision; }
                session.layout_pending = false;
                if needs_explicit_retry {
                    session.retry_allowed = true;
                    session.retry_expected = false;
                    session.conflict = Some("문서 revision이 바뀌었습니다. 같은 텍스트와 표 셀을 확인했습니다. 명시적으로 다시 시도할 수 있습니다".into());
                } else {
                    session.retry_allowed = false;
                    session.retry_expected = false;
                    session.conflict = None;
                }
            } else if same_text && table_cell && selection_current && !scene_current {
                session.layout_pending = true;
                session.retry_allowed = false;
                if needs_explicit_retry {
                    session.conflict = Some("표 셀 배치를 갱신 중입니다. 새 입력은 잠시 중지했고 기존 초안과 대기 입력은 보존했습니다".into());
                }
            } else {
                session.layout_pending = false;
                session.retry_allowed = false;
                session.retry_expected = false;
                session.conflict = Some(if table_cell && !selection_current {
                    "선택한 표 셀이 바뀌었습니다. 입력은 보존했습니다".into()
                } else if table_cell {
                    "표 셀의 텍스트, 소유자, 병합 또는 잠금 상태가 바뀌었습니다. 입력은 보존했습니다".into()
                } else {
                    "편집 중인 텍스트가 바뀌었거나 삭제되었습니다. 입력은 보존했습니다".into()
                });
            }
        }
    }

    pub(crate) fn begin_inline_text_edit(
        &mut self,
        ctx: &Context,
        node_id: &str,
        page_index: u32,
        position: Option<designllm_contracts::TextPosition>,
    ) -> bool {
        if self.inline_text_edit.is_some() {
            return false;
        }
        // Every refusal below says why: a silent refusal looks like a click that did nothing.
        if self.run_context.is_some() {
            self.set_error("AI 작업이 화면을 쓰는 동안에는 텍스트를 편집할 수 없습니다".into());
            return false;
        }
        let covered_origin = self.covered_table_text_selection.as_ref().filter(|selection| {
            self.active.as_ref().is_some_and(|active| active.id == selection.artifact_id)
                && self.selected_nodes.len() == 1
                && self.selected_nodes[0] == node_id
                && self.selected_node.as_deref() == Some(node_id)
                && selection.anchor_cell_id == node_id
        });
        if let Some(selection) = covered_origin {
            let current_mapping = self.artifact.as_ref().and_then(|artifact| {
                let page_index = board::node_page_index(artifact, &selection.covered_cell_id)?;
                canvas_table_anchor_for_covered_cell(artifact, page_index, &selection.covered_cell_id)
            });
            match current_mapping {
                Some((anchor_cell_id, true)) if anchor_cell_id == node_id => {
                    self.set_error("컴포넌트 인스턴스의 표 병합 구조는 원본 페이지에서 변경해야 합니다".into());
                    return false;
                }
                Some((anchor_cell_id, false)) if anchor_cell_id == node_id => {
                    self.set_error("병합된 영역의 추가 텍스트는 바로 편집할 수 없습니다. 병합을 해제한 뒤 다시 선택하세요".into());
                    return false;
                }
                _ => {
                    self.set_error("표 병합 구조가 바뀌었습니다. 현재 대상을 다시 선택하세요".into());
                    return false;
                }
            }
        }
        let (Some(active), Some(artifact), Some(context)) = (&self.active, &self.artifact, &self.context) else {
            self.set_error("문서를 읽는 중입니다. 잠시 뒤 다시 시도하세요".into());
            return false;
        };
        if !matches!(active.kind, ArtifactKind::Canvas | ArtifactKind::Slides) {
            return false;
        }
        if active.id != artifact.id || artifact.id != context.artifact_id
            || active.revision != context.revision || artifact.revision != context.revision
        {
            self.set_error("문서의 새 내용을 읽는 중입니다. 잠시 뒤 다시 시도하세요".into());
            return false;
        }
        if board::node_page_index(artifact, node_id) != Some(page_index) {
            self.set_error("편집하려는 텍스트가 요청한 페이지에 없습니다".into());
            return false;
        }
        let Some(node) = find_node(artifact, node_id) else {
            self.set_error("편집하려는 텍스트를 찾지 못했습니다".into());
            return false;
        };
        let Some(reservation_node_ids) = inline_text_target_node_ids(artifact, active.kind, page_index, node_id) else {
            self.set_error("이 텍스트 또는 표 셀은 편집할 수 없습니다".into());
            return false;
        };
        if !self.feature_edit_supported(Feature::Text) {
            self.set_error("이 텍스트는 편집할 수 없습니다".into());
            return false;
        }
        if reservation_node_ids.len() > 1
            && !self.page_textures.get(&page_index).is_some_and(|page| {
                page.artifact_id == active.id && page.revision == context.revision
                    && page.page.page_index == page_index && !page.snapshot_id.is_empty()
                    && page.frames.contains_key(node_id)
            })
        {
            self.set_error("표 셀의 현재 배치를 확인할 수 없습니다".into());
            return false;
        }
        let target_reserved = self.reservations.iter().any(|reservation| {
            reservation.node_ids.iter().any(|id| reservation_node_ids.iter().any(|target| target == id))
        }) || self.user_reservations.iter().any(|reservation| {
            reservation.node_ids.iter().any(|id| reservation_node_ids.iter().any(|target| target == id))
        });
        if self.reservations_for.as_deref() == Some(active.id.as_str()) && target_reserved {
            self.set_error("다른 작업이 이 텍스트 또는 표를 편집 중입니다".into());
            return false;
        }

        let original = node.text.clone();
        let cell_text_length = original.graphemes(true).count().min(u32::MAX as usize) as u32;
        let (paragraph_id, paragraph_index, local_caret) = match (active.kind, position) {
            (ArtifactKind::Canvas, Some(position)) if position.paragraph_id == node_id => {
                let local = if reservation_node_ids.len() > 1 {
                    position.grapheme_index.min(cell_text_length)
                } else {
                    position.grapheme_index
                };
                (node_id.to_owned(), 0, local)
            }
            (ArtifactKind::Slides, Some(position)) => slide_text_position(node_id, &position.paragraph_id)
                .map(|index| (position.paragraph_id.clone(), index, position.grapheme_index))
                .unwrap_or_else(|| last_text_position(active.kind, node_id, &original)),
            _ => last_text_position(active.kind, node_id, &original),
        };
        let caret = global_caret(&original, paragraph_index, local_caret);
        let id = self.next_text_session_id;
        self.next_text_session_id = self.next_text_session_id.saturating_add(1);
        self.text_target = Some(TextTarget::Node { node_id: node_id.to_owned(), original: original.clone(), editable: true });
        self.text_draft = original.clone();
        self.text_editor_focused = true;
        self.text_adapter = GraphemeAdapter::default();
        self.text_adapter.set_caret_at_grapheme(ctx, self.text_editor_id, &self.text_draft, Some(caret));
        let view_id = self.view_id.clone();
        let actor = inline_text_actor(&view_id);
        self.inline_text_edit = Some(InlineTextEdit {
            id, actor, view_id, artifact_id: active.id.clone(), node_id: node_id.to_owned(), page_index, kind: active.kind,
            revision: context.revision, committed_text: original, queue: VecDeque::new(), in_flight: None,
            composing: false, ime_commit_pending: false, ime_cancelled_frame: false, ime_generation: 0, reservation_id: None,
            reservation_node_ids,
            reservation_pending: false, reservation_release_pending: false, reservation_release_failed: false,
            release_when_reserved: false, finish_requested: false, layout_pending: false,
            conflict: None, retry_allowed: false, retry_expected: false, retry_same_request: false, reservation_expires: None,
        });
        let selection = Selection {
            node_ids: vec![node_id.to_owned()],
            text_range: Some(TextRange { paragraph_id, start: local_caret, end: local_caret }),
        };
        self.set_selected_ids(vec![node_id.to_owned()]);
        self.set_selection(selection);
        true
    }

    pub(super) fn inline_table_cell_selection_current(&self, session: &InlineTextEdit) -> bool {
        if session.reservation_node_ids.len() <= 1 {
            return true;
        }
        let selected = self.selected_nodes.len() == 1 && self.selected_nodes[0] == session.node_id
            && self.selected_node.as_deref() == Some(session.node_id.as_str())
            && self.local_selection.as_ref().is_some_and(|selection| {
                selection.node_ids.len() == 1 && selection.node_ids[0] == session.node_id
            });
        if !selected {
            return false;
        }
        let selection_snapshot_pending = self.pending.values().any(|pending| matches!(pending.method.as_str(), "selection.set" | "context.read"));
        selection_snapshot_pending || self.context.as_ref().is_some_and(|context| {
            context.artifact_id == session.artifact_id
                && context.selection.node_ids.len() == 1
                && context.selection.node_ids[0] == session.node_id
        })
    }

    pub(super) fn inline_table_cell_scene_current(&self, session: &InlineTextEdit) -> bool {
        let (Some(active), Some(artifact), Some(context)) = (&self.active, &self.artifact, &self.context) else { return false };
        if active.id != session.artifact_id || artifact.id != session.artifact_id || context.artifact_id != session.artifact_id
            || active.revision != artifact.revision || artifact.revision != context.revision
        {
            return false;
        }
        let ArtifactBody::Canvas(document) = &artifact.body else { return false };
        let Some(expected_page_id) = document.pages.get(session.page_index as usize).map(|page| page.id.as_str()) else { return false };
        self.page_textures.get(&session.page_index).is_some_and(|page| {
            page.artifact_id == session.artifact_id
                && page.revision == artifact.revision
                && page.page.page_index == session.page_index
                && page.page.id == expected_page_id
                && !page.snapshot_id.is_empty()
                && page.frames.contains_key(&session.node_id)
        })
    }

    pub(super) fn inline_table_cell_readiness(&self, session: &InlineTextEdit) -> InlineCellReadiness {
        if session.reservation_node_ids.len() <= 1 {
            return InlineCellReadiness::Ready;
        }
        let current_targets = self.artifact.as_ref().filter(|artifact| artifact.id == session.artifact_id)
            .and_then(|artifact| inline_text_target_node_ids(artifact, session.kind, session.page_index, &session.node_id));
        if current_targets.as_ref() != Some(&session.reservation_node_ids) {
            return InlineCellReadiness::TargetChanged;
        }
        if !self.inline_table_cell_selection_current(session) {
            return InlineCellReadiness::SelectionChanged;
        }
        // Canvas permits removing and re-adding the same ID. The Desktop has no incarnation token to prove identity across revisions.
        if !self.shows_revision(&session.artifact_id, session.revision) {
            return InlineCellReadiness::RevisionDrift;
        }
        let context_synced = self.active.as_ref().zip(self.artifact.as_ref()).zip(self.context.as_ref()).is_some_and(|((active, artifact), context)| {
            active.id == session.artifact_id && artifact.id == session.artifact_id && context.artifact_id == session.artifact_id
                && active.revision == artifact.revision && artifact.revision == context.revision
        });
        if !context_synced {
            return InlineCellReadiness::WaitingForRender;
        }
        let Some(page) = self.page_textures.get(&session.page_index) else {
            return InlineCellReadiness::WaitingForRender;
        };
        let Some(artifact) = self.artifact.as_ref() else { return InlineCellReadiness::WaitingForRender };
        let ArtifactBody::Canvas(document) = &artifact.body else { return InlineCellReadiness::TargetChanged };
        let Some(expected_page_id) = document.pages.get(session.page_index as usize).map(|page| page.id.as_str()) else {
            return InlineCellReadiness::TargetChanged;
        };
        if page.artifact_id != session.artifact_id || page.revision != artifact.revision
            || page.page.page_index != session.page_index || page.page.id != expected_page_id || page.snapshot_id.is_empty()
        {
            return InlineCellReadiness::WaitingForRender;
        }
        if !page.frames.contains_key(&session.node_id) {
            return InlineCellReadiness::FrameUnavailable;
        }
        let Some(active) = self.active.as_ref() else { return InlineCellReadiness::WaitingForRender };
        if active.revision != session.revision {
            return InlineCellReadiness::WaitingForRender;
        }
        InlineCellReadiness::Ready
    }

    pub(super) fn apply_inline_cell_readiness(&mut self, session_id: u64, readiness: InlineCellReadiness) -> bool {
        let Some(session) = self.inline_text_edit.as_mut().filter(|session| session.id == session_id) else { return false };
        match readiness {
            InlineCellReadiness::Ready => {
                session.layout_pending = false;
                true
            }
            InlineCellReadiness::WaitingForRender => {
                session.layout_pending = true;
                false
            }
            InlineCellReadiness::RevisionDrift => {
                session.layout_pending = false;
                session.conflict = Some("문서 revision이 바뀌어 같은 ID가 같은 표 셀인지 확인할 수 없습니다. 초안을 복사한 뒤 최신 셀에서 다시 입력하세요".into());
                session.retry_allowed = false;
                session.retry_expected = false;
                false
            }
            InlineCellReadiness::FrameUnavailable => {
                session.layout_pending = false;
                session.conflict = Some("현재 표 셀의 배치 정보를 받지 못했습니다. 입력은 보존했습니다".into());
                session.retry_allowed = false;
                session.retry_expected = false;
                false
            }
            InlineCellReadiness::TargetChanged => {
                session.layout_pending = false;
                session.conflict = Some("표 셀의 대상 또는 잠금 상태가 바뀌었습니다. 입력은 보존했습니다".into());
                session.retry_allowed = false;
                session.retry_expected = false;
                false
            }
            InlineCellReadiness::SelectionChanged => {
                session.layout_pending = false;
                session.conflict = Some("선택한 표 셀이 바뀌었습니다. 입력은 보존했습니다".into());
                session.retry_allowed = false;
                session.retry_expected = false;
                false
            }
        }
    }

    pub(super) fn resume_inline_table_cell_after_render(&mut self, page_index: u32) {
        self.reconcile_inline_text_after_refresh();
        let Some(session) = self.inline_text_edit.as_ref().filter(|session| {
            session.page_index == page_index && session.reservation_node_ids.len() > 1
        }) else { return };
        let session_id = session.id;
        let readiness = self.inline_table_cell_readiness(session);
        if !self.apply_inline_cell_readiness(session_id, readiness) { return; }
        if self.inline_text_edit.as_ref().is_some_and(|session| {
            // An edit asked to end (Escape, Tab) while its last change waited for this render ends now.
            session.id == session_id && session.in_flight.is_none() && (!session.queue.is_empty() || session.finish_requested)
                && session.conflict.is_none()
        }) {
            self.start_next_inline_text_change();
        }
    }

    pub(super) fn release_inline_reservation(&mut self, session_id: u64, reservation_id: String) {
        if self.close_after_inline_cleanup && self.close_inline_session_id == Some(session_id) {
            self.close_reservation_id = Some(reservation_id.clone());
            self.close_release_pending = true;
            if self.call("targets.release", json!({ "reservation_id": reservation_id }), PendingAction::InlineTextCloseRelease { session_id }).is_none() {
                self.close_release_pending = false;
                self.close_release_failed = true;
                self.close_after_inline_cleanup = false;
                self.close_confirmation_open = true;
            }
        } else {
            if let Some(session) = self.inline_text_edit.as_mut().filter(|session| session.id == session_id) {
                session.reservation_release_pending = true;
                session.reservation_release_failed = false;
                session.conflict = None;
            }
            if self.call(
                "targets.release",
                json!({ "reservation_id": reservation_id }),
                PendingAction::InlineTextRelease { session_id, reservation_id: reservation_id.clone() },
            ).is_none() {
                self.complete_inline_text_release(session_id, &reservation_id, true);
            }
        }
    }

    pub(super) fn complete_inline_text_release(&mut self, session_id: u64, reservation_id: &str, failed: bool) {
        let finish = {
            let Some(session) = self.inline_text_edit.as_mut().filter(|session| session.id == session_id) else {
                if failed {
                    self.close_reservation_id = Some(reservation_id.to_owned());
                    self.close_inline_session_id = Some(session_id);
                    self.close_release_failed = true;
                    self.close_confirmation_open = true;
                    self.set_error("텍스트 편집 예약 해제에 실패했습니다. 창을 닫을 때 예약을 다시 시도하거나 만료를 기다릴 수 있습니다".into());
                }
                return;
            };
            session.reservation_release_pending = false;
            if failed {
                session.reservation_id = Some(reservation_id.to_owned());
                session.release_when_reserved = false;
                session.reservation_release_failed = true;
                session.conflict = Some("텍스트 편집 예약을 해제하지 못했습니다. 다시 시도하거나 예약 만료를 기다릴 수 있습니다".into());
                session.retry_allowed = false;
                return;
            }

            session.reservation_release_failed = false;
            session.release_when_reserved = false;
            session.reservation_expires = None;
            session.conflict = None;
            session.finish_requested && session.in_flight.is_none() && session.queue.is_empty()
        };
        if finish {
            self.inline_text_edit = None;
            self.text_editor_focused = false;
            self.refresh_active();
        } else {
            self.start_next_inline_text_change();
        }
    }
}

pub(super) fn apply_grapheme_edit(text: &mut String, edit: &TextInputEdit) -> bool {
    if let Some(after) = &edit.after_text {
        *text = after.clone();
        return true;
    }
    let boundaries: Vec<usize> = text.grapheme_indices(true).map(|(byte, _)| byte).chain(std::iter::once(text.len())).collect();
    let start = edit.range.start as usize;
    let end = edit.range.end as usize;
    if start > end || end >= boundaries.len() {
        return false;
    }
    text.replace_range(boundaries[start]..boundaries[end], &edit.text);
    true
}
