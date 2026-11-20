use super::*;
use super::{doc_tree::operation_target_ids, state::ApplyContext};

impl DesktopApp {
    pub(super) fn history(&mut self, redo: bool) -> Result<(), String> {
        let (Some(active), Some(context)) = (&self.active, &self.context) else { return Err("되돌릴 작업물이 열려 있지 않습니다".into()) };
        let (actor, allowed_intents) = self.change_actor_intents([Intent::Content, Intent::Design, Intent::Data].into_iter().collect());
        let request = HistoryRequest {
            request_id: new_id(),
            actor,
            workspace_id: context.workspace_id.clone(),
            artifact_id: active.id.clone(),
            expected_revision: context.revision,
            allowed_intents,
            job_ref: None,
        };
        match serde_json::to_value(request) {
            Ok(params) => {
                self.call(
                    if redo { "history.redo" } else { "history.undo" },
                    params,
                    PendingAction::History,
                );
                Ok(())
            }
            Err(_) => Err("이력을 처리하지 못했습니다".into()),
        }
    }

    pub(super) fn apply_operations(&mut self, operations: Vec<Operation>, allowed_intents: BTreeSet<Intent>) {
        self.apply_operations_for(operations, allowed_intents, PendingAction::Apply);
    }

    /// Sends the operations as one change set against the revision this window shows. `action`
    /// says what to do when the host accepts it (most callers only need `Apply`).
    pub(super) fn apply_operations_for(
        &mut self,
        operations: Vec<Operation>,
        allowed_intents: BTreeSet<Intent>,
        action: PendingAction,
    ) {
        let _ = self.enqueue_operations_for(operations, allowed_intents, action);
    }

    pub(super) fn enqueue_operations_for(
        &mut self,
        operations: Vec<Operation>,
        allowed_intents: BTreeSet<Intent>,
        action: PendingAction,
    ) -> Option<u64> {
        let Some(revision) = self.context.as_ref().map(|context| context.revision) else {
            return None;
        };
        self.apply_changes(operations, allowed_intents, revision, action)
    }

    pub(super) fn apply_operations_at(
        &mut self,
        operations: Vec<Operation>,
        allowed_intents: BTreeSet<Intent>,
        expected_revision: u64,
    ) {
        let _ = self.apply_changes(operations, allowed_intents, expected_revision, PendingAction::Apply);
    }

    pub(super) fn apply_changes(
        &mut self,
        operations: Vec<Operation>,
        allowed_intents: BTreeSet<Intent>,
        expected_revision: u64,
        action: PendingAction,
    ) -> Option<u64> {
        if let Some(run) = self.run_context.as_mut() {
            run.mutation_attempted = true;
        }
        let (Some(active), Some(context)) = (&self.active, &self.context) else {
            if let Some(run) = self.run_context.as_mut().filter(|run| run.await_rpc_reply) {
                run.enqueue_error = Some(DltError::new(ErrorCode::InvalidArgument, "작업물을 아직 읽는 중입니다"));
            }
            return None;
        };
        let (actor, run_intents) = match &self.run_context {
            Some(run) => (run.actor.clone(), run.allowed_intents.clone()),
            None => ("desktop".to_owned(), allowed_intents.clone()),
        };
        let apply_context = ApplyContext {
            request_id: new_id(),
            workspace_id: context.workspace_id.clone(),
            artifact_id: active.id.clone(),
            base_revision: expected_revision,
            operations: operations.clone(),
            allowed_intents: run_intents.clone(),
            actor: actor.clone(),
            // A PDF evidence commit is not a direct edit: its refusal is held by `figure_tools`, not by the node holds.
            user_edit: self.run_context.is_none() && !matches!(&action, PendingAction::FigureEvidence),
            retryable: self.run_context.is_none(),
            held_edit_id: None,
            after_action: action.clone(),
        };
        let video_applying = self.video_insert.as_ref()
            .filter(|pending| pending.stage == VideoInsertStage::Applying && pending.intent.artifact_id == apply_context.artifact_id)
            .map(|pending| pending.intent.token.clone());
        if let Some(token) = video_applying {
            let own_video = matches!(&action, PendingAction::VideoInsertApply { token: action_token } if action_token == &token);
            if !own_video {
                let mut error = DltError::new(ErrorCode::TargetBusy, "a video insertion is being applied to this artifact");
                error.retryable = true;
                let mut nodes = operation_target_ids(&apply_context.operations);
                if nodes.is_empty() { nodes.clone_from(&self.selected_nodes); }
                error.affected_ids = nodes.clone();
                if apply_context.user_edit {
                    let request_id = apply_context.request_id.clone();
                    self.hold_direct_edit(apply_context, nodes, &error);
                    if let Some(held) = self.held_edits.iter_mut().find(|held| held.apply.request_id == request_id) {
                        held.can_retry = false;
                        held.message = "동영상 노드를 적용 중입니다. 입력은 보존했으며 적용이 끝난 뒤 최신 상태를 확인하세요".into();
                    }
                    self.refresh_held_edit_notice();
                    self.set_error("동영상 노드를 적용하는 동안 다른 변경을 보내지 않았습니다. 입력은 보류 항목에 보존했습니다".into());
                } else if let Some(run) = self.run_context.as_mut().filter(|run| run.await_rpc_reply) {
                    run.enqueue_error = Some(error);
                }
                return None;
            }
        }
        let replacement_applying = self.pending_image_replacement.as_ref()
            .filter(|pending| pending.stage == ImageReplacementStage::Applying && pending.intent.artifact_id == apply_context.artifact_id)
            .map(|pending| pending.intent.token.clone());
        if let Some(token) = replacement_applying {
            let own_replacement = matches!(&action, PendingAction::ImageReplacementApply { token: action_token } if action_token == &token);
            if !own_replacement {
                let mut error = DltError::new(ErrorCode::TargetBusy, "an image replacement is being applied to this artifact");
                error.retryable = true;
                let mut nodes = operation_target_ids(&apply_context.operations);
                if nodes.is_empty() { nodes.clone_from(&self.selected_nodes); }
                error.affected_ids = nodes.clone();
                if apply_context.user_edit {
                    let request_id = apply_context.request_id.clone();
                    self.hold_direct_edit(apply_context, nodes, &error);
                    if let Some(held) = self.held_edits.iter_mut().find(|held| held.apply.request_id == request_id) {
                        held.can_retry = false;
                        held.message = "이미지 교체를 적용 중입니다. 변경을 보존했습니다. 교체가 끝난 뒤 최신 작업물에서 다시 입력하세요".into();
                    }
                    self.refresh_held_edit_notice();
                    self.set_error("이미지 교체 적용 중에는 같은 작업물의 변경을 보내지 않습니다. 입력은 보류 항목에 보존했습니다".into());
                } else {
                    if let Some(run) = self.run_context.as_mut().filter(|run| run.await_rpc_reply) {
                        run.enqueue_error = Some(error);
                    }
                    self.set_error("이미지 교체 적용 중에는 같은 작업물의 자동 변경을 보류합니다".into());
                }
                return None;
            }
        }
        let crop_applying = self.image_crop_editor.as_ref()
            .filter(|editor| editor.stage == ImageCropStage::Applying && editor.intent.artifact_id == apply_context.artifact_id)
            .map(|editor| editor.intent.token.clone());
        if let Some(token) = crop_applying {
            let own_crop = matches!(&action, PendingAction::ImageCropApply { token: action_token } if action_token == &token);
            if !own_crop {
                let mut error = DltError::new(ErrorCode::TargetBusy, "an image crop is being applied to this artifact");
                error.retryable = true;
                let mut nodes = operation_target_ids(&apply_context.operations);
                if nodes.is_empty() { nodes.clone_from(&self.selected_nodes); }
                error.affected_ids = nodes.clone();
                if apply_context.user_edit {
                    let request_id = apply_context.request_id.clone();
                    self.hold_direct_edit(apply_context, nodes, &error);
                    if let Some(held) = self.held_edits.iter_mut().find(|held| held.apply.request_id == request_id) {
                        held.can_retry = false;
                        held.message = "이미지 자르기를 적용 중입니다. 변경을 보존했습니다. 끝난 뒤 최신 작업물에서 다시 입력하세요".into();
                    }
                    self.refresh_held_edit_notice();
                    self.set_error("이미지 자르기 적용 중에는 같은 작업물의 변경을 보내지 않습니다. 입력은 보류 항목에 보존했습니다".into());
                } else if let Some(run) = self.run_context.as_mut().filter(|run| run.await_rpc_reply) {
                    run.enqueue_error = Some(error);
                }
                return None;
            }
        }
        self.send_apply_context(apply_context, action)
    }

    pub(super) fn send_apply_context(&mut self, apply: ApplyContext, action: PendingAction) -> Option<u64> {
        let change = ChangeSet {
            request_id: apply.request_id.clone(),
            actor: apply.actor.clone(),
            workspace_id: apply.workspace_id.clone(),
            artifact_id: apply.artifact_id.clone(),
            base_revision: apply.base_revision,
            allowed_intents: apply.allowed_intents.clone(),
            job_ref: None,
            read_dependencies: Vec::new(),
            operations: apply.operations.clone(),
        };
        match serde_json::to_value(change) {
            Ok(params) => {
                let ticket = self.call("changes.apply", params, action);
                if let Some(ticket) = ticket {
                    if let Some(pending) = self.pending.get_mut(&ticket) {
                        pending.apply = Some(apply.clone());
                    }
                }
                ticket
            }
            Err(_) => {
                self.set_error(String::from("변경을 적용하지 못했습니다"));
                if let Some(run) = self.run_context.as_mut().filter(|run| run.await_rpc_reply) {
                    run.enqueue_error = Some(DltError::new(ErrorCode::IntegrityError, "변경을 적용하지 못했습니다"));
                }
                None
            }
        }
    }

    pub(super) fn feature_write_supported(&self, feature: Feature) -> bool {
        self.context
            .as_ref()
            .and_then(|context| context.capabilities.get(&feature))
            .is_some_and(|support| support.write == Support::Supported)
    }

    pub(super) fn feature_edit_supported(&self, feature: Feature) -> bool {
        self.context
            .as_ref()
            .and_then(|context| context.capabilities.get(&feature))
            .is_some_and(|support| support.edit == Support::Supported)
    }

    /// Sends one edited cell. The value is kept in its draft until the host answers (drafts.rs): a refusal or a failed send gives
    /// it back to the cell editor, and a table that changed keeps it as a copy-only draft with a notice.
    pub(super) fn apply_dataset_cell(&mut self, edit: DatasetCellEdit) {
        let (Some(active), Some(context), Some(artifact)) =
            (&self.active, &self.context, &self.artifact)
        else {
            self.keep_cell_as_copy(&edit);
            self.set_error("작업물을 읽는 중이라 자료 값을 적용하지 못했습니다. 값은 초안 목록에 남겼습니다".into());
            return;
        };
        let current_version = match &artifact.body {
            ArtifactBody::Figure(figure) => figure
                .datasets
                .iter()
                .find(|dataset| dataset.id == edit.dataset_id)
                .map(|dataset| dataset.version_hash.as_str()),
            _ => None,
        };
        if current_version != Some(edit.version_hash.as_str()) {
            self.dataset_edit = None;
            self.keep_cell_as_copy(&edit);
            self.set_error("자료가 다른 편집으로 바뀌었습니다. 최신 버전을 불러왔습니다. 편집하던 값은 초안 목록에 남겼습니다".into());
            self.refresh_active();
            return;
        }

        let params = dataset_edit_request(&context.workspace_id, &active.id, context.revision, &edit);
        let live = drafts::Live {
            artifact_id: Some(active.id.clone()),
            revision: Some(active.revision),
            body: drafts::cell_body(&edit),
        };
        let request_id = params["request_id"].as_str().unwrap_or_default().to_owned();
        let ticket = self.next_ticket;
        // The editor keeps the value while it is open (the figure's table); a cell editor already closed by the commit does not.
        let keep_owner = self.dataset_edit.is_some();
        if let Err(message) = self.drafts.begin_send(ticket, "cell", live, &request_id, keep_owner, Instant::now()) {
            if !keep_owner {
                self.keep_cell_as_copy(&edit);
            }
            self.set_error(message);
            return;
        }
        match self.call("figure.dataset.edit", params, PendingAction::DatasetEdit) {
            Some(sent) => self.drafts.rebind(ticket, sent),
            None => self.answer_sent_outcome(ticket, drafts::Outcome::Refused),
        }
    }
}

/// The `figure.dataset.edit` request for one edited cell: the common change-set fields plus the
/// dataset version the edit was made on. The host derives the new version and rebinds the plots;
/// the window never parses the number or computes a hash.
pub(super) fn dataset_edit_request(
    workspace_id: &str,
    artifact_id: &str,
    base_revision: u64,
    edit: &DatasetCellEdit,
) -> Value {
    json!({
        "request_id": new_id(),
        "actor": "desktop",
        "workspace_id": workspace_id,
        "artifact_id": artifact_id,
        "base_revision": base_revision,
        "allowed_intents": ["data"],
        "job_ref": null,
        "dataset_id": edit.dataset_id,
        "version_hash": edit.version_hash,
        "edits": [{
            "type": "edit_cell",
            "row_id": edit.row_id,
            "column_id": edit.column_id,
            "previous": edit.original,
            "replacement": edit.draft,
        }],
    })
}
