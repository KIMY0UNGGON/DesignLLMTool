use super::*;
use super::{
    doc_tree::canvas_table_anchor_for_covered_cell,
    events::{parse_artifact, selection_matches, stale_selection},
    messages::export_warning,
    state::{parse_reservations, parse_user_reservations, HeldEditState, WorkspaceListing},
};

impl DesktopApp {
    pub(super) fn handle_payload(&mut self, ctx: &Context, ticket: u64, action: PendingAction, payload: RpcPayload) {
        match (action, payload) {
            (PendingAction::Hello, RpcPayload::Json(value)) => self.hello_answered(&value),
            (PendingAction::WorkspaceList, RpcPayload::Json(value)) => self.workspace_listed(value),
            (PendingAction::Create | PendingAction::Open, RpcPayload::Json(value)) => self.artifact_opened(value),
            (PendingAction::HomeCreate { prompt }, RpcPayload::Json(value)) => self.home_created(&value, prompt),
            (PendingAction::StopForEdit, RpcPayload::Json(_)) => {}
            (PendingAction::Reservations, RpcPayload::Json(value)) => {
                self.adopt_reservations(&value);
                self.resume_held_edits();
            }
            (PendingAction::HeldEditReservations { held_id }, RpcPayload::Json(value)) => {
                self.held_edit_reservations_read(&held_id, &value)
            }
            (PendingAction::FlowStyles { artifact_id, revision }, RpcPayload::Json(value)) => {
                let styles = value.get("styles").cloned().and_then(|styles| serde_json::from_value(styles).ok());
                self.flow_styles = Some((artifact_id, revision, styles));
            }
            (PendingAction::UiPublish, RpcPayload::Json(value)) => self.accept_ui_publish(value),
            (PendingAction::UiComplete, RpcPayload::Json(_)) => {}
            (PendingAction::HostProbe, _) => {}
            (PendingAction::UiAsset { run_id, width, height, path }, RpcPayload::Json(value)) => {
                self.finish_ui_asset(run_id, width, height, path, value)
            }
            (PendingAction::VideoSourceImport { token }, RpcPayload::Json(value)) => {
                self.complete_video_source_import(&token, &value)
            }
            (PendingAction::VideoPosterImport { token, temp_path }, RpcPayload::Json(value)) => {
                self.complete_video_poster_import(&token, &temp_path, &value)
            }
            (PendingAction::VideoInsertApply { token }, RpcPayload::Json(value)) => {
                if self.video_insert.as_ref().is_some_and(|pending| {
                    pending.intent.token == token && pending.stage == VideoInsertStage::Applying
                }) {
                    self.cleanup_video_poster_temp(&token);
                    self.video_insert = None;
                    self.committed(&value);
                }
            }
            (PendingAction::ImageReplacementImport { token }, RpcPayload::Json(value)) => self.complete_image_replacement_import(&token, &value),
            (PendingAction::ImageReplacementApply { token }, RpcPayload::Json(value)) => {
                if self.pending_image_replacement.as_ref().is_some_and(|pending| {
                    pending.intent.token == token && pending.stage == ImageReplacementStage::Applying
                }) {
                    self.pending_image_replacement = None;
                    self.committed(&value);
                }
            }
            (PendingAction::ImageCropLookup { token }, RpcPayload::Json(value)) => self.complete_image_crop_lookup(&token, &value),
            (PendingAction::ImageCropApply { token }, RpcPayload::Json(value)) => self.complete_image_crop_apply(&token, &value),
            (PendingAction::BoardMemosChange { token }, RpcPayload::Json(value)) => {
                self.finish_board_memo_change(&token, &value);
            }
            (PendingAction::PageSectionsChange { token }, RpcPayload::Json(value)) => {
                self.finish_page_sections_change(&token, &value);
            }
            (PendingAction::ImageAsset { page_id }, RpcPayload::Json(value)) => self.insert_image(&page_id, value),
            (PendingAction::FigureImageAsset { origin, source_ref }, RpcPayload::Json(value)) => self.add_figure_image_panel(&origin, source_ref, &value),
            (PendingAction::LintWarnings { artifact_id }, RpcPayload::Json(value)) => self.take_lint(&artifact_id, &value),
            (PendingAction::Context, RpcPayload::Json(value)) => self.context_read(value),
            (PendingAction::Inspect, RpcPayload::Json(value)) => self.artifact_inspected(&value),
            (PendingAction::DesignSystemList, RpcPayload::Json(value)) => self.design_systems_listed(value),
            (
                PendingAction::DesignSystemSource { consumer_id, source_id, source_revision, generation },
                RpcPayload::Json(value),
            ) => self.design_system_source_read(&consumer_id, &source_id, source_revision, generation, value),
            (PendingAction::DesignSystemChange { refresh_systems }, RpcPayload::Json(value)) => {
                self.committed(&value);
                if refresh_systems {
                    self.fetch_design_systems();
                }
            }
            (PendingAction::FlowFindPage { request }, RpcPayload::Json(value)) => {
                self.flow_find_page_received(&request, value);
            }
            (PendingAction::Render { page_index }, RpcPayload::Json(value)) => self.page_rendered(page_index, value),
            (PendingAction::PreviewAsset { snapshot, page_index }, RpcPayload::Asset { mime, width, height, rgba }) => {
                if !self.shows_revision(&snapshot.artifact_id, snapshot.revision) {
                    return;
                }
                if !board::is_rgba_png(&mime, width, height, &rgba) {
                    self.set_error("미리보기 이미지가 올바르지 않습니다".into());
                    return;
                }
                self.show_page_preview(ctx, page_index, snapshot, width, height, &rgba);
            }
            (PendingAction::Transform { .. }, RpcPayload::Json(_)) => {}
            (PendingAction::HitTest { snapshot_id, page_index, transform_id, additive, edit_text }, RpcPayload::Json(value)) => {
                if self.hit_test_scene_current(&snapshot_id, page_index, &transform_id) {
                    self.hit_test_answered(ctx, page_index, additive, edit_text, value);
                }
            }
            (PendingAction::Selection, RpcPayload::Json(_)) => {
                self.fetch_context();
            }
            (PendingAction::InlineTextChange { session_id }, RpcPayload::Json(value)) => {
                self.complete_inline_text_change(session_id, &value);
            }
            (PendingAction::InlineTextReserve { session_id }, RpcPayload::Json(value)) => {
                self.complete_inline_text_reservation(session_id, &value);
            }
            (PendingAction::InlineTextRelease { session_id, reservation_id }, RpcPayload::Json(_)) => {
                if self.close_after_inline_cleanup && self.close_inline_session_id == Some(session_id) {
                    self.finalize_close_after_inline_cleanup(ctx);
                } else {
                    self.complete_inline_text_release(session_id, &reservation_id, false);
                }
            }
            (PendingAction::InlineTextCloseRelease { session_id }, RpcPayload::Json(_)) => {
                if self.close_inline_session_id == Some(session_id) { self.finalize_close_after_inline_cleanup(ctx); }
            }
            (PendingAction::InlineTextCloseReservationCheck { session_id, view_id, actor, node_id, artifact_id }, RpcPayload::Json(value)) => {
                self.complete_close_reservation_check(ctx, session_id, &view_id, &actor, &node_id, &artifact_id, &value);
            }
            (
                PendingAction::Apply
                | PendingAction::PathAdd { .. }
                | PendingAction::DatasetEdit
                | PendingAction::History
                | PendingAction::LinkApply,
                RpcPayload::Json(value),
            ) => self.committed(&value),
            (PendingAction::PageReorder { token }, RpcPayload::Json(value)) => {
                if let Some(revision) = value.get("revision").and_then(Value::as_u64) {
                    self.commit_pending_page_reorder_selection(&token, revision);
                } else {
                    self.cancel_pending_page_reorder_selection(&token);
                }
                self.committed(&value);
            }
            (PendingAction::PageReorder { token }, _) => {
                self.cancel_pending_page_reorder_selection(&token);
                self.set_error("페이지 순서를 반영하지 못했습니다".into());
            }
            (PendingAction::TokenChange { baseline }, RpcPayload::Json(value)) => {
                if let (Some(baseline), Some(revision)) = (baseline, value.get("revision").and_then(Value::as_u64)) {
                    self.token_panel.commit_local_change(revision, baseline);
                }
                self.committed(&value);
            }
            (PendingAction::ApplyWithSelectionIfUnchanged { node_ids, expected_selection, expected_page, expected_selection_generation }, RpcPayload::Json(value)) => {
                self.committed_selecting_if_unchanged(ticket, node_ids, expected_selection, expected_page, expected_selection_generation, &value)
            }
            (PendingAction::FigureInfo { request }, RpcPayload::Json(value)) => {
                match parse_artifact(&value).and_then(|figure| links::figure_facts(&figure)) {
                    Some(figure) => self.render_figure_for_link(request, figure),
                    None => self.set_error("이 그림을 문서에 넣을 수 없습니다".into()),
                }
            }
            (PendingAction::FigureRender { request, figure }, RpcPayload::Json(value)) => {
                match serde_json::from_value::<links::RenderedLink>(value) {
                    Ok(rendered) => self.apply_figure_link(&request, &figure, &rendered),
                    Err(_) => self.set_error("그림을 이미지로 만들지 못했습니다".into()),
                }
            }
            (PendingAction::FigureDataset, RpcPayload::Json(value)) => {
                self.figure_form = None;
                self.csv_draft = None;
                self.csv_path.clear();
                self.committed(&value);
            }
            (PendingAction::FigureEvidence, RpcPayload::Json(value)) => {
                self.pdf_evidence_committed(ticket, &value);
                self.figure_form = None;
                self.pdf_path.clear();
                self.committed_keeping_cell_draft(&value);
            }
            (PendingAction::Save { request_id, artifact_id }, RpcPayload::Json(_)) => {
                self.save_succeeded(&request_id, &artifact_id);
                self.refresh_active();
            }
            (PendingAction::ExportDryRun, RpcPayload::Json(value)) => self.export_dry_run_read(value),
            (PendingAction::PlayDryRun { request }, RpcPayload::Json(value)) => self.play_dry_run_read(request, value),
            (PendingAction::PlayExport { path }, RpcPayload::Json(value)) => {
                if let Some(message) = export_warning(&value) {
                    self.notices.warning(message, Instant::now());
                }
                if let Err(error) = self.launch_web_preview(&path, None) {
                    self.set_error(error.message);
                }
            }
            (PendingAction::RenderRegion { page_index }, RpcPayload::Json(value)) => self.region_rendered(page_index, value),
            (PendingAction::RegionAsset { snapshot, page_index }, RpcPayload::Asset { mime, width, height, rgba }) => {
                let region = snapshot.preview_transform.as_ref().and_then(|transform| transform.crop);
                if board::is_rgba_png(&mime, width, height, &rgba) {
                    if let Some(region) = region {
                        self.store_region_texture(ctx, page_index, &snapshot, region, width, height, &rgba);
                    }
                }
            }
            (PendingAction::ExportPreview { key }, RpcPayload::Json(value)) => {
                if self.export_preview_pending.as_deref() == Some(key.as_str()) {
                    self.export_preview_pending = None;
                }
                if let Ok(dry) = serde_json::from_value::<ExportDryRun>(value) {
                    self.export_preview = Some((key, dry.losses));
                }
            }
            (PendingAction::Export, RpcPayload::Json(value)) => {
                self.pending_export = None;
                self.notify_exported(&value);
            }
            (PendingAction::UiExport, RpcPayload::Json(value)) => self.notify_exported(&value),
            (PendingAction::FigureAiImport { token }, RpcPayload::Json(value)) => self.figure_ai_import_done(&token, value),
            (PendingAction::FigureAiJob { token }, RpcPayload::Json(value)) => self.figure_ai_job_started(&token, value),
            (PendingAction::GetParkedJob { .. }, RpcPayload::Json(value)) => {
                if let Ok(job) = serde_json::from_value::<Job>(value) {
                    self.update_job(job);
                }
            }
            (PendingAction::StartJob, RpcPayload::Json(value)) => self.job_started(value),
            (PendingAction::GetJob { .. }, RpcPayload::Json(value)) | (PendingAction::CancelJob, RpcPayload::Json(value)) => {
                match serde_json::from_value::<Job>(value) {
                    Ok(job) => {
                        self.update_job(job);
                        self.last_job_poll = Instant::now();
                    }
                    Err(_) => self.set_error(String::from("AI 작업 상태를 읽지 못했습니다")),
                }
            }
            (PendingAction::ReserveDrag { token }, RpcPayload::Json(value)) => self.drag_reserved(token, &value),
            (PendingAction::CsvAssetImport, RpcPayload::Json(value)) => self.csv_asset_imported(value),
            (PendingAction::CsvDatasetImport, RpcPayload::Json(value)) => self.csv_dataset_read(value),
            (PendingAction::CsvDeclaredImport, RpcPayload::Json(value)) => self.csv_declaration_read(value),
            (PendingAction::PdfAssetImport { page_index }, RpcPayload::Json(value)) => self.pdf_asset_imported(ticket, page_index, value),
            (PendingAction::PdfEvidenceImport, RpcPayload::Json(value)) => self.pdf_evidence_read(ticket, &value),
            (PendingAction::PollEvents, RpcPayload::Json(value)) => self.handle_events(value),
            (PendingAction::ReleaseDrag, RpcPayload::Json(_)) => {}
            (_, RpcPayload::Asset { .. }) => self.set_error("예기치 않은 이미지 응답입니다".into()),
            (_, RpcPayload::Json(_)) => self.set_error("예기치 않은 작업 응답입니다".into()),
        }
    }

    // Workspace and documents.

    fn hello_answered(&mut self, value: &Value) {
        if let Some(id) = value.get("workspace_id").and_then(Value::as_str) {
            if let Some(listing_id) = self.context.as_ref().map(|item| item.workspace_id.as_str()) {
                if listing_id != id {
                    self.context = None;
                }
            }
        }
    }

    fn workspace_listed(&mut self, value: Value) {
        match serde_json::from_value::<WorkspaceListing>(value) {
            Ok(listing) => {
                self.update_workspace_notices(&listing.workspace_id, listing.workspace_notices);
                self.artifacts = listing.artifacts;
                self.workspace_loaded = true;
                // The app always starts on the home screen; only the smoke capture opens a document,
                // so that the capture shows an editor.
                let smoke_editor = std::env::var("DESIGNLLM_SMOKE_SCREEN").map_or(true, |value| value != "home");
                if self.active.is_none() && self.smoke_output.is_some() && smoke_editor {
                    // `DESIGNLLM_SMOKE_TITLE` picks the document to show by its title.
                    let wanted = std::env::var("DESIGNLLM_SMOKE_TITLE").ok();
                    let chosen = match wanted {
                        Some(title) => self.artifacts.iter().find(|item| item.title == title).cloned(),
                        None => self.artifacts.first().cloned(),
                    };
                    if let Some(first) = chosen {
                        self.activate(first);
                    }
                } else if let Some(active) = &self.active {
                    self.active = self
                        .artifacts
                        .iter()
                        .find(|item| item.id == active.id)
                        .cloned();
                }
                self.forget_unlisted_jobs();
                self.forget_unlisted_pdf_evidence();
            }
            Err(_) => self.set_error(String::from("작업 목록을 읽지 못했습니다")),
        }
    }

    /// Shows a document that was just created or opened, and lists the workspace again.
    fn open_listed(&mut self, summary: ArtifactSummary) {
        self.call("workspace.list", json!({}), PendingAction::WorkspaceList);
        self.activate(summary);
    }

    fn artifact_opened(&mut self, value: Value) {
        match serde_json::from_value::<ArtifactSummary>(value) {
            Ok(summary) => self.open_listed(summary),
            Err(_) => self.set_error(String::from("작업물을 열지 못했습니다")),
        }
    }

    fn home_created(&mut self, value: &Value, prompt: String) {
        match ArtifactSummary::deserialize(&value["artifact"]) {
            Ok(summary) => {
                if let Ok(Some(job)) = Option::<Job>::deserialize(&value["job"]) {
                    // The job's label in the AI tab is the request it was started with (`job_prompts`).
                    self.job_prompts.insert(job.id.clone(), prompt);
                    self.record_started_job(job);
                }
                self.open_listed(summary);
            }
            Err(_) => self.set_error(String::from("작업물을 열지 못했습니다")),
        }
    }

    // Reservations.

    /// Takes the job and user reservations the host reported, as those of the open document.
    fn adopt_reservations(&mut self, value: &Value) {
        self.reservations = parse_reservations(value);
        self.user_reservations = parse_user_reservations(value);
        self.reservations_for = self.active.as_ref().map(|active| active.id.clone());
    }

    fn held_edit_reservations_read(&mut self, held_id: &str, value: &Value) {
        let Some(expected_artifact) = self.held_edits.iter().find(|held| held.id == held_id).map(|held| held.apply.artifact_id.clone()) else { return };
        if value.get("artifact_id").and_then(Value::as_str) != Some(expected_artifact.as_str()) {
            self.fail_held_edit_check(held_id, &DltError::new(ErrorCode::IntegrityError, "보류한 변경의 대상 확인 응답이 다른 작업물을 가리킵니다"));
            return;
        }
        self.adopt_reservations(value);
        if let Some(held) = self.held_edits.iter_mut().find(|held| held.id == held_id && held.state == HeldEditState::Checking) {
            held.reservations_checked = true;
        }
        self.resume_held_edits();
    }

    // Context and document structure.

    /// Brings the edit state in line with a context or a document structure just read: the image being replaced, the
    /// style fields, the text target, and the inline text session with the table cell it may wait on.
    fn resync_edit_state(&mut self) {
        self.invalidate_image_replacement_if_target_moved();
        self.sync_style_fields();
        self.sync_text_target();
        self.reconcile_inline_text_after_refresh();
        if let Some(page_index) = self.inline_text_edit.as_ref()
            .filter(|session| session.reservation_node_ids.len() > 1)
            .map(|session| session.page_index)
        {
            self.resume_inline_table_cell_after_render(page_index);
        }
    }

    fn context_read(&mut self, value: Value) {
        match serde_json::from_value::<ContextSnapshot>(value) {
            Ok(snapshot) => {
                if self.active.as_ref().is_none_or(|active| active.id != snapshot.artifact_id)
                    || self.view_id != snapshot.view_id
                {
                    self.reset_held_edit_checks("최신 작업물 상태가 달라졌습니다. 다시 확인하거나 변경을 취소하세요");
                    return;
                }
                if let Some(active) = self.active.as_mut() {
                    active.revision = snapshot.revision;
                }
                let selection_set_pending = self.pending.values().any(|pending| pending.method == "selection.set");
                if !selection_set_pending {
                    self.set_selected_ids(snapshot.selection.node_ids.clone());
                    self.local_selection = Some(snapshot.selection.clone());
                    self.reselect = stale_selection(&snapshot);
                }
                self.view_id = snapshot.view_id.clone();
                self.context = Some(snapshot);
                self.resync_edit_state();
                self.resume_held_edits();
                self.reconcile_video_insert_after_refresh();
            }
            Err(_) => self.set_error(String::from("편집 상태를 읽지 못했습니다")),
        }
    }

    fn artifact_inspected(&mut self, value: &Value) {
        match parse_artifact(value) {
            Some(artifact) => {
                if self.active.as_ref().is_some_and(|summary| summary.id == artifact.id) {
                    self.active = Some(ArtifactSummary {
                        id: artifact.id.clone(),
                        kind: artifact.kind,
                        title: artifact.title.clone(),
                        revision: artifact.revision,
                        engine_id: artifact.engine_id.clone(),
                        dirty: self.active.as_ref().is_some_and(|summary| summary.dirty),
                        modified_at_ms: self.active.as_ref().and_then(|summary| summary.modified_at_ms),
                        active_jobs: self.active.as_ref().map(|summary| summary.active_jobs.clone()).unwrap_or_default(),
                    });
                    if let Some(active) = self.active.as_mut() {
                        active.revision = artifact.revision;
                    }
                    self.artifact = Some(artifact);
                    self.resync_edit_state();
                    self.restore_selection();
                    self.fetch_linked_design_system_source();
                    // Every revision the window shows (its own edits, an agent's, undo) gets its warnings.
                    self.request_lint();
                    self.resume_held_edits();
                    self.reconcile_video_insert_after_refresh();
                } else {
                    self.reset_held_edit_checks("보류한 변경의 작업물을 다시 연 뒤 확인하세요");
                }
            }
            None => {
                self.reset_held_edit_checks("보류한 변경의 최신 작업 구조를 읽지 못했습니다");
                self.set_error(String::from("작업 구조를 읽지 못했습니다"));
            }
        }
    }

    // Design systems.

    fn design_systems_listed(&mut self, value: Value) {
        match serde_json::from_value::<DesignSystemListResult>(value) {
            Ok(result) => {
                self.token_panel.set_design_systems(result.systems);
                self.fetch_linked_design_system_source();
            }
            Err(_) => {
                self.token_panel.design_systems_error = true;
                self.token_panel.design_systems_loaded = false;
            }
        }
    }

    fn design_system_source_read(&mut self, consumer_id: &str, source_id: &str, source_revision: u64, generation: u64, value: Value) {
        let consumer_still_uses_source = self.artifact.as_ref().is_some_and(|consumer| {
            consumer.id == consumer_id
                && consumer.design_system.as_ref().is_some_and(|reference| {
                    reference.artifact_id == source_id && reference.revision == source_revision
                })
        });
        if !consumer_still_uses_source || self.token_panel.shared_source_generation != generation {
            return;
        }
        let source = serde_json::from_value::<Artifact>(value).ok();
        let Some(source) = source.filter(|source| {
            source.id == source_id
                && source.revision == source_revision
                && source.kind == ArtifactKind::Canvas
                && source.publishes_design_system
                && source.design_system.is_none()
        }) else {
            self.token_panel.fail_shared_source(generation);
            return;
        };
        match source.body {
            ArtifactBody::Canvas(document) => {
                self.token_panel.install_shared_source(generation, source_id, source_revision, document.tokens);
            }
            _ => self.token_panel.fail_shared_source(generation),
        }
    }

    // Rendered pages.

    fn page_rendered(&mut self, page_index: u32, value: Value) {
        match serde_json::from_value::<RenderSnapshot>(value) {
            Ok(snapshot) => {
                if !snapshot.scene.pages.iter().any(|page| page.page_index == page_index)
                    || !self.shows_revision(&snapshot.artifact_id, snapshot.revision)
                {
                    return;
                }
                for page in &snapshot.scene.pages {
                    self.remember_page_size(page);
                }
                self.total_pages = scene_total_pages(&snapshot, page_index);
                if let Some(asset) = snapshot.preview.clone() {
                    self.read_preview_asset(asset, snapshot, page_index);
                } else {
                    self.render = Some(snapshot);
                }
            }
            Err(_) => self.set_error(String::from("페이지를 렌더링하지 못했습니다")),
        }
    }

    /// Shows the decoded preview of a page of the shown revision.
    fn show_page_preview(&mut self, ctx: &Context, page_index: u32, snapshot: RenderSnapshot, width: u32, height: u32, rgba: &[u8]) {
        self.store_page_texture(ctx, page_index, &snapshot, width, height, rgba);
        self.resume_inline_table_cell_after_render(page_index);
        let first_page = snapshot
            .scene
            .pages
            .first()
            .map_or(self.page_index, |page| page.page_index);
        self.total_pages = scene_total_pages(&snapshot, first_page);
        self.render = Some(snapshot);
    }

    fn region_rendered(&mut self, page_index: u32, value: Value) {
        let Ok(snapshot) = serde_json::from_value::<RenderSnapshot>(value) else {
            return;
        };
        if !self.shows_revision(&snapshot.artifact_id, snapshot.revision) {
            return;
        }
        if let Some(asset) = snapshot.preview.clone() {
            let artifact_id = snapshot.artifact_id.clone();
            // A region that cannot be read is not shown: the page texture stays.
            let _ = self.send_asset_read(asset, artifact_id, PendingAction::RegionAsset { snapshot, page_index });
        }
    }

    // Hit tests.

    /// Whether a hit test answers the scene on screen: the document, its structure, its context and the drawn page are
    /// all of one revision, the page was drawn from snapshot `snapshot_id`, and its view transform is still `transform_id`.
    fn hit_test_scene_current(&self, snapshot_id: &str, page_index: u32, transform_id: &str) -> bool {
        let scene_current = self.active.as_ref().zip(self.artifact.as_ref()).zip(self.context.as_ref()).is_some_and(|((active, artifact), context)| {
            self.page_textures.get(&page_index).is_some_and(|page| {
                active.id == artifact.id && artifact.id == context.artifact_id
                    && active.revision == artifact.revision && artifact.revision == context.revision
                    && page.artifact_id == active.id && page.revision == active.revision
                    && page.page.page_index == page_index && page.snapshot_id == snapshot_id
            })
        });
        scene_current && self.page_transforms.get(&page_index).is_some_and(|transform| transform.id == transform_id)
    }

    fn hit_test_answered(&mut self, ctx: &Context, page_index: u32, additive: bool, edit_text: bool, mut value: Value) {
        // The core answers `{snapshot_id, transform_id, page_index, revision, point_emu, hit}` (API.md, render.hit_test):
        // `hit` is null on empty space. A reply without the field does not answer the click, so it is refused.
        let Some(hit) = value.get_mut("hit").map(Value::take) else {
            self.set_error(String::from("선택을 읽지 못했습니다"));
            return;
        };
        match serde_json::from_value::<Option<designllm_contracts::HitTestResult>>(hit) {
            Ok(Some(hit)) => {
                let covered_cell_anchor = self.artifact.as_ref()
                    .and_then(|artifact| canvas_table_anchor_for_covered_cell(artifact, page_index, &hit.node_id));
                let target_node_id = covered_cell_anchor.as_ref().map_or_else(|| hit.node_id.clone(), |(anchor_id, _)| anchor_id.clone());
                let position = if covered_cell_anchor.is_some() { None } else { hit.position.clone() };
                let ids = if additive {
                    multi::toggle_id(&self.selected_nodes, &target_node_id)
                } else {
                    vec![target_node_id.clone()]
                };
                self.set_selected_ids(ids.clone());
                self.sync_style_fields();
                let text_range = if additive {
                    None
                } else {
                    position.as_ref().map(|position| TextRange {
                        paragraph_id: position.paragraph_id.clone(),
                        start: position.grapheme_index,
                        end: position.grapheme_index,
                    })
                };
                self.set_selection(Selection { node_ids: ids, text_range });
                self.covered_table_text_selection = if !additive
                    && self.inline_text_edit.is_none()
                    && self.selected_nodes.len() == 1
                    && self.selected_nodes[0] == target_node_id
                {
                    covered_cell_anchor.as_ref().and_then(|(anchor_cell_id, _)| {
                        self.active.as_ref().map(|active| CoveredTableTextSelection {
                            artifact_id: active.id.clone(),
                            covered_cell_id: hit.node_id.clone(),
                            anchor_cell_id: anchor_cell_id.clone(),
                        })
                    })
                } else {
                    None
                };
                if edit_text && !additive {
                    match covered_cell_anchor.as_ref() {
                        Some((_, true)) => self.set_error("컴포넌트 인스턴스의 표 병합 구조는 원본 페이지에서 변경해야 합니다".into()),
                        Some((_, false)) => self.set_error("병합된 영역의 추가 텍스트는 바로 편집할 수 없습니다. 병합을 해제한 뒤 다시 선택하세요".into()),
                        None => { let _ = self.begin_inline_text_edit(ctx, &target_node_id, page_index, position); }
                    }
                }
            }
            Ok(None) if additive => {}
            Ok(None) => self.select_clear(),
            Err(_) => self.set_error(String::from("선택을 읽지 못했습니다")),
        }
    }

    // Commits.

    /// A commit that selects `node_ids` once it is shown, unless the selection or the page moved on while it was on its way.
    fn committed_selecting_if_unchanged(
        &mut self,
        ticket: u64,
        node_ids: Vec<String>,
        expected_selection: Selection,
        expected_page: u32,
        expected_selection_generation: u64,
        value: &Value,
    ) {
        let same_view = self.selection_generation == expected_selection_generation
            && self.view_ref().page_index == expected_page
            && self.local_selection.as_ref().is_none_or(|selection| selection_matches(selection, &expected_selection))
            && self.context.as_ref().is_some_and(|context| context.visible_page == expected_page && selection_matches(&context.selection, &expected_selection));
        if same_view {
            self.select_after_commit = Some(node_ids);
            self.select_after_commit_ticket = Some(ticket);
            self.select_after_commit_guard = value
                .get("revision")
                .and_then(Value::as_u64)
                .map(|committed_revision| SelectionRestoreGuard {
                    ticket,
                    expected_selection,
                    expected_page,
                    expected_generation: expected_selection_generation,
                    committed_revision,
                });
        } else {
            // The selection moved on while the commit was on its way: the answer brings nothing to select.
            self.ui.take_agent_commit(ticket);
        }
        self.committed(value);
    }

    // Export and play.

    fn export_dry_run_read(&mut self, value: Value) {
        match serde_json::from_value::<ExportDryRun>(value) {
            Ok(dry) => {
                if let Some(request) = self.pending_export.take() {
                    let accepted = dry.losses.iter().all(|loss| request.options.allowed_losses.contains(&loss.code));
                    if accepted {
                        self.send_export(request);
                    } else {
                        self.export_confirmation = Some(ExportConfirmation {
                            request,
                            diagnostics: dry.losses,
                            allowed: BTreeSet::new(),
                        });
                    }
                }
            }
            Err(_) => self.set_error(String::from("내보내기 미리보기를 읽지 못했습니다")),
        }
    }

    fn play_dry_run_read(&mut self, mut request: ExportFileRequest, value: Value) {
        let Ok(dry) = serde_json::from_value::<ExportDryRun>(value) else {
            self.set_error(String::from("재생할 화면을 만들지 못했습니다"));
            return;
        };
        request.request_id = new_id();
        request.dry_run = None;
        request.options.allowed_losses = dry.losses.iter().map(|loss| loss.code.clone()).collect::<BTreeSet<_>>().into_iter().collect();
        let path = PathBuf::from(&request.target_path);
        match serde_json::to_value(&request) {
            Ok(params) => {
                self.call("artifact.export", params, PendingAction::PlayExport { path });
            }
            Err(_) => self.set_error(String::from("재생할 화면을 만들지 못했습니다")),
        }
    }

    /// Tells that an export was written, with the losses the engine reported.
    fn notify_exported(&mut self, value: &Value) {
        if let Some(message) = export_warning(value) {
            self.notices.warning(format!("내보냈습니다. {message}"), Instant::now());
        } else {
            self.notices.info("내보냈습니다", Instant::now());
        }
    }

    // Drag.

    fn drag_reserved(&mut self, token: u64, value: &Value) {
        let reservation = value
            .get("reservation_id")
            .and_then(Value::as_str)
            .map(str::to_owned);
        if let Some(drag) = self.drag.as_mut().filter(|drag| drag.token == token) {
            drag.reservation_id = reservation;
            if drag.reservation_id.is_none() {
                self.set_error("편집 대상을 예약하지 못했습니다".into());
                self.drag = None;
            } else if drag.released {
                self.commit_drag_if_reserved();
            }
        }
    }

    // CSV/TSV import.

    fn csv_asset_imported(&mut self, value: Value) {
        match serde_json::from_value::<AssetRef>(value) {
            Ok(asset) => {
                self.call(
                    "source.import_csv",
                    json!({ "sha256": asset.sha256 }),
                    PendingAction::CsvDatasetImport,
                );
            }
            Err(_) => self.set_error("CSV/TSV 파일을 가져오지 못했습니다".into()),
        }
    }

    fn csv_dataset_read(&mut self, value: Value) {
        match serde_json::from_value::<Dataset>(value) {
            Ok(dataset) => {
                // An agent's `add.chart` with a path: the table goes into the figure with the types read, no dialog. The run
                // completes with that commit (the CSV reads prepare it, `prepares_figure_change`).
                if self.run_context.as_ref().is_some_and(|run| run.command_id == "add.chart") {
                    if let Err(error) = self.kind_change(vec![Operation::PutDataset { dataset }]) {
                        if let Some(run) = self.run_context.as_mut() {
                            run.enqueue_error = Some(DltError::invalid(error));
                        }
                    }
                    return;
                }
                // A table the figure already holds keeps the types it was declared with.
                let held = self
                    .figure_document()
                    .and_then(|figure| figure.datasets.into_iter().find(|held| held.id == dataset.id));
                self.csv_draft = Some(match held {
                    Some(held) => CsvImportDraft::new(held),
                    None => CsvImportDraft::to_declare(dataset),
                });
            }
            Err(_) => self.set_error("CSV/TSV 자료를 읽지 못했습니다".into()),
        }
    }

    fn csv_declaration_read(&mut self, value: Value) {
        match serde_json::from_value::<Dataset>(value) {
            Ok(declared) => {
                if let Some(draft) = self.csv_draft.as_mut() {
                    draft.adopt(declared);
                }
            }
            Err(_) => self.set_error("CSV/TSV 자료를 읽지 못했습니다".into()),
        }
    }
}

/// The page count a render reports, or the pages up to `page_index` when it reports none.
fn scene_total_pages(snapshot: &RenderSnapshot, page_index: u32) -> u32 {
    snapshot
        .scene
        .metadata
        .get("total_pages")
        .and_then(Value::as_u64)
        .unwrap_or(u64::from(page_index) + 1) as u32
}
