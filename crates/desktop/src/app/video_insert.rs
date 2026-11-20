//! Canvas video insertion: inspect and decode before either asset is imported, then add both
//! references with one content operation.

use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    io::Write,
    path::PathBuf,
    thread,
};

use designllm_contracts::{
    new_id, sha256, ArtifactBody, ArtifactKind, AssetRef, DltError, ErrorCode, Feature, Intent,
    Node, NodeKind, NodeExt, Operation, Rect, Style,
};
use serde_json::{json, Value};

use super::{
    state::VideoInsertIntent, DesktopApp, PendingAction, PendingVideoInsert, VideoInsertStage,
    VideoPosterWorkerResult,
};

// The cap Canvas native.rs applies to the embedded assets of a saved file. Canvas exposes no public asset_refs/size helper.
const MAX_CANVAS_EMBEDDED_ASSET_BYTES: u64 = designllm_contracts::native_envelope::NATIVE_MAX_EMBEDDED_BYTES;

impl DesktopApp {
    pub(super) fn video_insert_for_active(&self) -> Option<&PendingVideoInsert> {
        let active_id = self.active.as_ref()?.id.as_str();
        self.video_insert
            .as_ref()
            .filter(|pending| pending.intent.artifact_id == active_id)
    }

    pub(super) fn video_insert_status_for_active(&self) -> String {
        let Some(pending) = self.video_insert_for_active() else {
            return String::new();
        };
        pending
            .message
            .clone()
            .unwrap_or_else(|| pending.status_text().to_owned())
    }

    pub(super) fn video_insert_can_cancel(&self) -> bool {
        self.video_insert_for_active().is_some_and(|pending| {
            !pending.cancel_requested
                && !matches!(
                    pending.stage,
                    VideoInsertStage::Applying | VideoInsertStage::OutcomeUncertain
                )
        })
    }

    pub(super) fn begin_canvas_video_insert(&mut self) -> Result<(), String> {
        let intent = self.video_insert_intent()?;
        let Some(path) = crate::dialogs::open_video(self.parent_hwnd) else {
            return Ok(());
        };
        if !self.video_insert_intent_is_current(&intent) {
            return Err("파일을 고르는 동안 선택이나 작업물·페이지가 바뀌어 동영상을 추가하지 않았습니다".into());
        }
        let extension = path
            .extension()
            .and_then(|extension| extension.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        if !matches!(extension.as_str(), "mp4" | "webm") {
            return Err("MP4 또는 WebM 동영상만 추가할 수 있습니다".into());
        }
        self.video_insert = Some(PendingVideoInsert {
            intent: intent.clone(),
            source_path: path,
            stage: VideoInsertStage::InspectingSource,
            source_info: None,
            source_asset: None,
            poster: None,
            poster_asset: None,
            poster_temp_path: None,
            worker_cancel: None,
            cancel_requested: false,
            node_id: new_id(),
            apply: None,
            resolution_retry_sent: false,
            message: None,
        });
        self.spawn_video_source_inspection(&intent.token)
    }

    pub(super) fn cancel_canvas_video_insert(&mut self) -> Result<(), String> {
        let Some(pending) = self.video_insert.as_mut() else {
            return Err("진행 중인 동영상 추가가 없습니다".into());
        };
        if matches!(pending.stage, VideoInsertStage::Applying | VideoInsertStage::OutcomeUncertain) {
            pending.message = Some("동영상 노드 추가 결과를 확인하기 전에는 요청을 취소할 수 없습니다".into());
            return Err("동영상 노드 추가 결과를 먼저 확인하세요".into());
        }
        pending.cancel_requested = true;
        if let Some(cancel) = &pending.worker_cancel {
            cancel.cancel();
        }
        pending.message = Some(match pending.stage {
            VideoInsertStage::InspectingSource | VideoInsertStage::ExtractingPoster => {
                "동영상 검사 또는 첫 장면 추출을 중단하고 있습니다".into()
            }
            VideoInsertStage::ImportingSource => {
                "가져오기를 마친 뒤 동영상 노드는 추가하지 않습니다. 원본 자산은 미참조 상태로 남을 수 있습니다".into()
            }
            VideoInsertStage::ImportingPoster => {
                "가져오기를 마친 뒤 동영상 노드는 추가하지 않습니다. 자산은 미참조 상태로 남을 수 있습니다".into()
            }
            VideoInsertStage::Applying | VideoInsertStage::OutcomeUncertain => unreachable!(),
        });
        Ok(())
    }

    pub(super) fn has_video_poster_worker(&self) -> bool {
        self.video_insert.as_ref().is_some_and(|pending| {
            matches!(
                pending.stage,
                VideoInsertStage::InspectingSource | VideoInsertStage::ExtractingPoster
            )
        })
    }

    pub(super) fn cancel_stale_video_worker(&mut self) {
        let Some(intent) = self.video_insert.as_ref()
            .filter(|pending| matches!(pending.stage, VideoInsertStage::InspectingSource | VideoInsertStage::ExtractingPoster))
            .map(|pending| pending.intent.clone())
        else { return };
        if self.video_insert_intent_is_current(&intent) { return; }
        if let Some(pending) = self.video_insert.as_mut().filter(|pending| pending.intent.token == intent.token) {
            pending.cancel_requested = true;
            if let Some(cancel) = &pending.worker_cancel { cancel.cancel(); }
            pending.message = Some("선택, 페이지 또는 작업물 revision이 바뀌어 검사 또는 추출을 취소하고 정리 중입니다".into());
        }
    }

    pub(super) fn poll_video_poster_workers(&mut self) {
        for reply in self.video_worker_rx.try_iter().collect::<Vec<_>>() {
            match reply {
                VideoPosterWorkerResult::Inspected { token, result } => {
                    self.complete_video_source_inspection(&token, result)
                }
                VideoPosterWorkerResult::Extracted { token, result } => {
                    self.complete_video_poster_extraction(&token, result)
                }
            }
        }
    }

    pub(super) fn cancel_video_insert_before_switch(&mut self) {
        let Some(pending) = self.video_insert.as_mut() else {
            return;
        };
        match pending.stage {
            VideoInsertStage::InspectingSource | VideoInsertStage::ExtractingPoster => {
                if let Some(cancel) = &pending.worker_cancel {
                    cancel.cancel();
                }
                pending.cancel_requested = true;
                pending.message = Some("검사 또는 첫 장면 추출이 끝나 정리된 뒤 작업물을 전환할 수 있습니다".into());
            }
            VideoInsertStage::ImportingSource | VideoInsertStage::ImportingPoster => {
                pending.cancel_requested = true;
                if let Some(cancel) = &pending.worker_cancel {
                    cancel.cancel();
                }
                pending.message = Some("다른 작업물로 이동했습니다. 진행 중인 가져오기가 끝나면 동영상 노드를 추가하지 않습니다".into());
            }
            VideoInsertStage::Applying | VideoInsertStage::OutcomeUncertain => {}
        }
    }

    pub(super) fn discard_video_insert_for_close(&mut self) {
        let Some(mut pending) = self.video_insert.take() else {
            return;
        };
        if let Some(cancel) = &pending.worker_cancel {
            cancel.cancel();
        }
        if let Some(path) = pending.poster_temp_path.take() {
            let _ = fs::remove_file(path);
        }
        if pending.source_asset.is_some() || pending.poster_asset.is_some() {
            self.notices.warning(
                "동영상 노드를 추가하지 않았습니다. 가져온 자산은 다른 참조가 있을 수 있어 삭제하지 않았으며, 문서에서 사용되지 않은 상태로 남을 수 있습니다",
                super::Instant::now(),
            );
        }
    }

    pub(super) fn handle_video_offscreen_reply(
        &mut self,
        action: &PendingAction,
        result: &Result<crate::rpc::RpcPayload, DltError>,
    ) {
        match action {
            PendingAction::VideoSourceImport { token } => {
                self.finish_video_offscreen_import(token, result, None)
            }
            PendingAction::VideoPosterImport { token, temp_path } => {
                self.finish_video_offscreen_import(token, result, Some(temp_path))
            }
            PendingAction::VideoInsertApply { token } => {
                self.finish_video_insert_offscreen(token, result)
            }
            _ => {}
        }
    }

    pub(super) fn finish_video_source_import_error(&mut self, token: &str, error: &DltError) {
        if self.video_insert.as_ref().is_none_or(|pending| {
            pending.intent.token != token || pending.stage != VideoInsertStage::ImportingSource
        }) {
            return;
        }
        self.cleanup_video_poster_temp(token);
        self.video_insert = None;
        self.set_error(format!(
            "동영상 원본을 가져오지 못해 노드를 추가하지 않았습니다. 저장소에 자산이 남았을 수 있습니다: {}",
            super::user_error(error)
        ));
    }

    pub(super) fn finish_video_poster_import_error(
        &mut self,
        token: &str,
        temp_path: &std::path::Path,
        error: &DltError,
    ) {
        let _ = fs::remove_file(temp_path);
        if self.video_insert.as_ref().is_none_or(|pending| {
            pending.intent.token != token || pending.stage != VideoInsertStage::ImportingPoster
        }) {
            return;
        }
        self.cleanup_video_poster_temp(token);
        self.video_insert = None;
        self.set_error(format!(
            "동영상 poster를 가져오지 못해 노드를 추가하지 않았습니다. 가져온 원본·poster 자산은 미참조 상태로 남을 수 있습니다: {}",
            super::user_error(error)
        ));
    }

    pub(super) fn finish_video_insert_apply_error(&mut self, token: &str, error: &DltError) {
        if self.video_insert.as_ref().is_none_or(|pending| {
            pending.intent.token != token || pending.stage != VideoInsertStage::Applying
        }) {
            return;
        }
        if super::messages::change_result_unknown(error) {
            if let Some(pending) = self.video_insert.as_mut() {
                pending.stage = VideoInsertStage::OutcomeUncertain;
                pending.message = Some(
                    "요청 결과가 불명확합니다. 중복 삽입을 막기 위해 다시 전송하지 않고 최신 상태를 확인합니다".into(),
                );
            }
            let active_matches = self.video_insert.as_ref().is_some_and(|pending| {
                self.active.as_ref().is_some_and(|active| active.id == pending.intent.artifact_id)
            });
            if active_matches {
                self.refresh_active();
            }
        } else {
            self.cleanup_video_poster_temp(token);
            self.video_insert = None;
            self.set_error(format!(
                "동영상 노드를 추가하지 못했습니다. 가져온 자산은 미참조 상태로 남을 수 있습니다: {}",
                super::user_error(error)
            ));
            self.refresh_active();
        }
    }

    pub(super) fn reconcile_video_insert_after_refresh(&mut self) {
        let Some(pending) = self.video_insert.as_ref().filter(|pending| {
            pending.stage == VideoInsertStage::OutcomeUncertain
        }) else {
            return;
        };
        let intent = pending.intent.clone();
        let node_id = pending.node_id.clone();
        let Some(source_asset) = pending.source_asset.clone() else { return };
        let Some(poster_asset) = pending.poster_asset.clone() else { return };
        let Some(apply) = pending.apply.clone() else { return };
        let retried = pending.resolution_retry_sent;
        let Some((active, artifact, context)) = self
            .active
            .as_ref()
            .zip(self.artifact.as_ref())
            .zip(self.context.as_ref())
            .map(|((active, artifact), context)| (active, artifact, context))
        else {
            return;
        };
        if active.id != intent.artifact_id
            || artifact.id != intent.artifact_id
            || context.artifact_id != intent.artifact_id
            || active.revision != artifact.revision
            || artifact.revision != context.revision
        {
            return;
        }
        let ArtifactBody::Canvas(document) = &artifact.body else {
            self.set_video_uncertain_message("최신 작업물이 Canvas가 아니므로 동영상 추가 결과를 확정하지 못했습니다".into());
            return;
        };
        let found = document
            .pages
            .iter()
            .find(|page| page.id == intent.page_id)
            .and_then(|page| find_node_in_page(&page.nodes, &node_id));
        if let Some(node) = found {
            if node.kind == NodeKind::Video
                && node.asset.as_ref() == Some(&source_asset)
                && node.ext.video_poster_asset.as_ref() == Some(&poster_asset)
            {
                self.video_insert = None;
                self.notices.info("동영상 노드가 이미 추가된 것을 확인했습니다", super::Instant::now());
            } else {
                self.set_video_uncertain_message("같은 노드 ID에 다른 내용이 있어 자동 재전송하지 않습니다. 최신 작업물을 확인하세요".into());
            }
            return;
        }
        if document.pages.iter().any(|page| find_node_in_page(&page.nodes, &node_id).is_some()) {
            self.set_video_uncertain_message("동영상 노드 ID가 다른 페이지에 있어 자동 재전송하지 않습니다. 최신 작업물을 확인하세요".into());
            return;
        }
        if artifact.revision != intent.expected_revision {
            self.set_video_uncertain_message("작업물 revision이 바뀌어 자동 재전송하지 않습니다. 최신 작업물을 확인하세요".into());
            return;
        }
        if !self.video_insert_intent_is_current(&intent) {
            self.set_video_uncertain_message("선택 또는 페이지가 바뀌어 자동 재전송하지 않습니다. 추가 결과를 직접 확인하세요".into());
            return;
        }
        if retried {
            self.set_video_uncertain_message("같은 요청으로 한 번 더 결과를 확인했지만 확정하지 못했습니다. 중복 방지를 위해 재전송하지 않습니다".into());
            return;
        }
        if let Some(pending) = self.video_insert.as_mut() {
            pending.stage = VideoInsertStage::Applying;
            pending.resolution_retry_sent = true;
            pending.message = Some("같은 request ID로 결과를 한 번 확인하고 있습니다".into());
        }
        if let Some(ticket) = self.send_apply_context(apply.clone(), PendingAction::VideoInsertApply { token: intent.token.clone() }) {
            if let Some(pending) = self.pending.get_mut(&ticket) {
                pending.apply = Some(apply.clone());
            }
            if let Some(video) = self.video_insert.as_mut() {
                video.apply = Some(apply);
            }
        } else {
            self.set_video_uncertain_message("같은 요청을 다시 보내지 못했습니다. 자동 중복 방지는 유지하며 최신 상태를 확인하세요".into());
        }
    }

    fn set_video_uncertain_message(&mut self, message: String) {
        if let Some(pending) = self.video_insert.as_mut() {
            pending.stage = VideoInsertStage::OutcomeUncertain;
            pending.message = Some(message);
        }
    }

    fn video_insert_intent(&self) -> Result<VideoInsertIntent, String> {
        if self.video_insert.is_some() {
            return Err("진행 중인 동영상 추가가 끝날 때까지 기다리세요".into());
        }
        if self.run_context.is_some() {
            return Err("자동 작업이 끝난 뒤 동영상을 추가하세요".into());
        }
        let (Some(active), Some(artifact), Some(context)) = (&self.active, &self.artifact, &self.context) else {
            return Err("작업물을 아직 읽는 중입니다".into());
        };
        if !self.screen_is_editor()
            || active.kind != ArtifactKind::Canvas
            || artifact.kind != ArtifactKind::Canvas
            || active.id != artifact.id
            || artifact.id != context.artifact_id
            || self.view_id != context.view_id
            || active.revision != artifact.revision
            || artifact.revision != context.revision
        {
            return Err("현재 Canvas revision을 확인할 수 없습니다".into());
        }
        if !self.feature_write_supported(Feature::Images) {
            return Err("이 Canvas에서는 동영상을 추가할 수 없습니다".into());
        }
        if self.artifact_revision_change_pending(&active.id) {
            return Err("같은 작업물의 다른 변경이 끝난 뒤 동영상을 추가하세요".into());
        }
        if self.pending_image_replacement.is_some() || self.image_crop_editor.is_some() {
            return Err("현재 이미지 작업이 끝난 뒤 동영상을 추가하세요".into());
        }
        if self.pending.values().any(|pending| pending.method == "selection.set")
            || self.local_selection.as_ref().is_some_and(|selection| !super::events::selection_matches(selection, &context.selection))
        {
            return Err("선택 변경이 끝난 뒤 동영상을 추가하세요".into());
        }
        let ArtifactBody::Canvas(document) = &artifact.body else {
            return Err("Canvas에서만 동영상을 추가할 수 있습니다".into());
        };
        let page_index = self.view_ref().page_index;
        let page = document.pages.get(page_index as usize).ok_or("현재 페이지를 찾지 못했습니다")?;
        Ok(VideoInsertIntent {
            token: new_id(),
            artifact_id: artifact.id.clone(),
            expected_revision: artifact.revision,
            page_index,
            page_id: page.id.clone(),
            view_id: self.view_id.clone(),
            expected_selection: context.selection.clone(),
            selection_generation: self.selection_generation,
        })
    }

    fn video_insert_intent_is_current(&self, expected: &VideoInsertIntent) -> bool {
        let (Some(active), Some(artifact), Some(context)) = (&self.active, &self.artifact, &self.context) else {
            return false;
        };
        if !self.screen_is_editor()
            || active.kind != ArtifactKind::Canvas
            || artifact.kind != ArtifactKind::Canvas
            || active.id != expected.artifact_id
            || artifact.id != expected.artifact_id
            || context.artifact_id != expected.artifact_id
            || self.view_id != expected.view_id
            || context.view_id != expected.view_id
            || active.revision != expected.expected_revision
            || artifact.revision != expected.expected_revision
            || context.revision != expected.expected_revision
            || self.view_ref().page_index != expected.page_index
            || self.selection_generation != expected.selection_generation
            || !super::events::selection_matches(&context.selection, &expected.expected_selection)
            || self.local_selection.as_ref().is_some_and(|selection| {
                !super::events::selection_matches(selection, &expected.expected_selection)
            })
            || self.pending.values().any(|pending| pending.method == "selection.set")
            || self.artifact_revision_change_pending(&expected.artifact_id)
        {
            return false;
        }
        let ArtifactBody::Canvas(document) = &artifact.body else {
            return false;
        };
        document
            .pages
            .get(expected.page_index as usize)
            .is_some_and(|page| page.id == expected.page_id)
    }

    fn spawn_video_source_inspection(&mut self, token: &str) -> Result<(), String> {
        if let Some(reason) = self.ui_test_path_refusal(self.video_insert.as_ref().map(|pending| pending.source_path.as_path())) {
            self.video_insert = None;
            return Err(reason);
        }
        let Some(pending) = self.video_insert.as_mut().filter(|pending| {
            pending.intent.token == token && pending.stage == VideoInsertStage::InspectingSource
        }) else {
            return Err("동영상 추가 요청이 사라졌습니다".into());
        };
        let path = pending.source_path.clone();
        let token = token.to_owned();
        let cancel = designllm_web_preview::VideoPosterCancellation::default();
        pending.worker_cancel = Some(cancel.clone());
        let sender = self.video_worker_tx.clone();
        thread::Builder::new()
            .name("designllm-video-inspect".into())
            .spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    designllm_web_preview::inspect_video_source_cancellable(
                        &path,
                        designllm_web_preview::DEFAULT_VIDEO_POSTER_TIMEOUT,
                        &cancel,
                    )
                }))
                .unwrap_or_else(|_| {
                    Err(DltError::new(
                        ErrorCode::Internal,
                        "동영상 파일 검사 작업이 예기치 않게 종료되었습니다",
                    ))
                });
                let _ = sender.send(VideoPosterWorkerResult::Inspected { token, result });
            })
            .map_err(|error| {
                self.video_insert = None;
                format!("동영상 파일 검사 작업을 시작하지 못했습니다: {error}")
            })?;
        Ok(())
    }

    fn spawn_video_poster_extraction(&mut self, token: &str) -> Result<(), String> {
        if let Some(reason) = self.ui_test_path_refusal(self.video_insert.as_ref().map(|pending| pending.source_path.as_path())) {
            self.video_insert = None;
            return Err(reason);
        }
        let Some(pending) = self.video_insert.as_mut().filter(|pending| {
            pending.intent.token == token && pending.stage == VideoInsertStage::ExtractingPoster
        }) else {
            return Err("동영상 poster 요청이 사라졌습니다".into());
        };
        let expected_sha256 = pending
            .source_info
            .as_ref()
            .map(|source| source.sha256.clone())
            .ok_or("동영상 파일 hash를 확인하지 못했습니다")?;
        let path = pending.source_path.clone();
        let token_for_error = token.to_owned();
        let token = token.to_owned();
        let cancel = designllm_web_preview::VideoPosterCancellation::default();
        pending.worker_cancel = Some(cancel.clone());
        let sender = self.video_worker_tx.clone();
        thread::Builder::new()
            .name("designllm-video-poster".into())
            .spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    designllm_web_preview::extract_video_poster_cancellable(
                        &path,
                        &expected_sha256,
                        designllm_web_preview::DEFAULT_VIDEO_POSTER_TIMEOUT,
                        &cancel,
                    )
                }))
                .unwrap_or_else(|_| {
                    Err(DltError::new(
                        ErrorCode::Internal,
                        "동영상 poster 추출 작업이 예기치 않게 종료되었습니다",
                    ))
                });
                let _ = sender.send(VideoPosterWorkerResult::Extracted { token, result });
            })
            .map_err(|error| {
                if let Some(pending) = self.video_insert.as_mut().filter(|pending| pending.intent.token == token_for_error) {
                    pending.worker_cancel = None;
                }
                format!("동영상 poster 추출 작업을 시작하지 못했습니다: {error}")
            })?;
        Ok(())
    }

    fn complete_video_source_inspection(
        &mut self,
        token: &str,
        result: designllm_contracts::DltResult<designllm_web_preview::VideoSourceInfo>,
    ) {
        let Some((intent, canceled, source_path)) = self
            .video_insert
            .as_ref()
            .filter(|pending| {
                pending.intent.token == token && pending.stage == VideoInsertStage::InspectingSource
            })
            .map(|pending| (pending.intent.clone(), pending.cancel_requested, pending.source_path.clone()))
        else {
            return;
        };
        if let Some(pending) = self.video_insert.as_mut() {
            pending.worker_cancel = None;
        }
        if canceled {
            self.video_insert = None;
            self.notices.info("동영상 추가를 취소했습니다. 자산은 가져오지 않았습니다", super::Instant::now());
            return;
        }
        if !self.video_insert_intent_is_current(&intent) {
            self.video_insert = None;
            self.set_error("동영상 파일을 확인하는 동안 선택 또는 작업물이 바뀌어 추가를 취소했습니다. 자산은 가져오지 않았습니다".into());
            return;
        }
        let source = match result {
            Ok(source) => source,
            Err(error) => {
                self.video_insert = None;
                self.set_error(format!("동영상 파일을 확인하지 못했습니다: {}", super::user_error(&error)));
                return;
            }
        };
        let extension = source_path
            .extension()
            .and_then(|extension| extension.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        let mime_matches_extension = matches!((extension.as_str(), source.mime.as_str()),
            ("mp4", "video/mp4") | ("webm", "video/webm"));
        if source.bytes == 0
            || source.bytes > designllm_web_preview::MAX_VIDEO_INPUT_BYTES
            || !mime_matches_extension
            || !valid_sha256(&source.sha256)
            || source.sha256.bytes().any(|byte| byte.is_ascii_uppercase())
        {
            self.video_insert = None;
            self.set_error("동영상은 1 byte 이상 128 MiB 이하의 일치하는 MP4 또는 WebM 파일이어야 합니다".into());
            return;
        }
        if let Some(pending) = self.video_insert.as_mut().filter(|pending| pending.intent.token == token) {
            pending.source_info = Some(source);
            pending.stage = VideoInsertStage::ExtractingPoster;
            pending.message = None;
        }
        if let Err(message) = self.spawn_video_poster_extraction(token) {
            self.video_insert = None;
            self.set_error(message);
        }
    }

    fn complete_video_poster_extraction(
        &mut self,
        token: &str,
        result: designllm_contracts::DltResult<designllm_web_preview::VideoPoster>,
    ) {
        let Some((intent, canceled)) = self
            .video_insert
            .as_ref()
            .filter(|pending| {
                pending.intent.token == token && pending.stage == VideoInsertStage::ExtractingPoster
            })
            .map(|pending| (pending.intent.clone(), pending.cancel_requested))
        else {
            return;
        };
        if let Some(pending) = self.video_insert.as_mut() {
            pending.worker_cancel = None;
        }
        if canceled {
            self.video_insert = None;
            self.notices.info("동영상 추가를 취소했습니다. 원본과 poster 자산은 가져오지 않았습니다", super::Instant::now());
            return;
        }
        if !self.video_insert_intent_is_current(&intent) {
            self.video_insert = None;
            self.set_error("첫 장면을 만드는 동안 선택 또는 작업물이 바뀌어 동영상을 추가하지 않았습니다. 자산은 가져오지 않았습니다".into());
            return;
        }
        let poster = match result {
            Ok(poster) => poster,
            Err(error) => {
                self.video_insert = None;
                self.set_error(format!("첫 장면 이미지를 만들지 못해 동영상을 추가하지 않았습니다: {}", super::user_error(&error)));
                return;
            }
        };
        if poster.png.is_empty()
            || poster.png.len() > designllm_web_preview::MAX_VIDEO_POSTER_PNG_BYTES
            || !poster.png.starts_with(&designllm_contracts::media_header::PNG_SIGNATURE)
            || poster.width == 0
            || poster.height == 0
            || u64::from(poster.width) * u64::from(poster.height) > designllm_web_preview::MAX_VIDEO_POSTER_PIXELS
        {
            self.video_insert = None;
            self.set_error("동영상 첫 장면 이미지가 허용된 PNG 크기 범위를 벗어나 노드를 추가하지 않았습니다".into());
            return;
        }
        let (decoded_width, decoded_height, _) = match designllm_render::decode_image(&poster.png) {
            Ok(decoded) => decoded,
            Err(error) => {
                self.video_insert = None;
                self.set_error(format!("첫 장면 PNG가 손상되어 동영상을 추가하지 않았습니다: {}", super::user_error(&error)));
                return;
            }
        };
        if (decoded_width, decoded_height) != (poster.width, poster.height) {
            self.video_insert = None;
            self.set_error("첫 장면 PNG의 실제 크기가 추출 결과와 달라 동영상을 추가하지 않았습니다".into());
            return;
        }
        let Some(source_info) = self.video_insert.as_ref().and_then(|pending| pending.source_info.clone()) else {
            self.video_insert = None;
            self.set_error("검증한 동영상 원본 정보를 찾지 못해 자산을 가져오지 않았습니다".into());
            return;
        };
        let poster_ref = AssetRef {
            sha256: sha256(&poster.png),
            mime: "image/png".into(),
            bytes: poster.png.len() as u64,
        };
        let source_ref = AssetRef {
            sha256: source_info.sha256.clone(),
            mime: source_info.mime.clone(),
            bytes: source_info.bytes,
        };
        let asset_budget = match self.artifact.as_ref().map(|artifact| &artifact.body) {
            Some(ArtifactBody::Canvas(document)) => projected_canvas_asset_bytes(document, [&source_ref, &poster_ref]),
            _ => Err("Canvas 자산 용량을 확인할 수 없어 동영상을 가져오지 않았습니다".into()),
        };
        if let Err(error) = asset_budget {
            self.video_insert = None;
            self.set_error(error);
            return;
        }
        let poster_path = match write_video_poster_temp(&poster.png) {
            Ok(path) => path,
            Err(error) => {
                self.video_insert = None;
                self.set_error(format!("첫 장면 이미지를 임시 저장하지 못해 동영상을 추가하지 않았습니다: {error}"));
                return;
            }
        };
        if let Some(pending) = self.video_insert.as_mut().filter(|pending| pending.intent.token == token) {
            pending.poster = Some(poster);
            pending.poster_temp_path = Some(poster_path.clone());
            pending.stage = VideoInsertStage::ImportingSource;
            pending.message = None;
        }
        let Some(source_path) = self
            .video_insert
            .as_ref()
            .filter(|pending| pending.intent.token == token)
            .map(|pending| pending.source_path.clone()) else {
                self.cleanup_video_poster_temp(token);
                self.video_insert = None;
                self.set_error("동영상 추가 상태가 바뀌어 원본을 가져오지 않았습니다".into());
                return;
            };
        if self.call(
            "asset.import",
            json!({ "path": source_path.to_string_lossy() }),
            PendingAction::VideoSourceImport { token: token.to_owned() },
        ).is_none() {
            self.cleanup_video_poster_temp(token);
            self.video_insert = None;
            self.set_error("동영상 원본을 가져오지 못해 노드를 추가하지 않았습니다".into());
        }
    }

    pub(super) fn complete_video_source_import(&mut self, token: &str, value: &Value) {
        let Some((intent, expected, canceled, temp_path)) = self.video_insert.as_ref()
            .filter(|pending| pending.intent.token == token && pending.stage == VideoInsertStage::ImportingSource)
            .and_then(|pending| Some((pending.intent.clone(), pending.source_info.clone()?, pending.cancel_requested, pending.poster_temp_path.clone()?)))
        else { return };
        let Ok(asset) = serde_json::from_value::<AssetRef>(value.clone()) else {
            self.cleanup_video_poster_temp(token);
            self.video_insert = None;
            self.set_error("가져온 동영상 자산 정보를 읽지 못해 노드를 추가하지 않았습니다. 원본 자산은 미참조 상태로 남을 수 있습니다".into());
            return;
        };
        if !source_asset_matches(&asset, &expected) {
            self.cleanup_video_poster_temp(token);
            self.video_insert = None;
            self.set_error("검사·poster 추출 뒤 원본 파일이 바뀌었습니다. 다른 내용의 자산을 연결하지 않았으며 가져온 자산은 미참조 상태로 남을 수 있습니다".into());
            return;
        }
        if canceled || !self.video_insert_intent_is_current(&intent) {
            self.cleanup_video_poster_temp(token);
            self.video_insert = None;
            self.notices.warning("동영상 노드를 추가하지 않았습니다. 가져온 원본 자산은 미참조 상태로 남을 수 있습니다", super::Instant::now());
            return;
        }
        if let Some(pending) = self.video_insert.as_mut().filter(|pending| pending.intent.token == token) {
            pending.source_asset = Some(asset);
            pending.stage = VideoInsertStage::ImportingPoster;
            pending.message = None;
        }
        if self.call(
            "asset.import",
            json!({ "path": temp_path.to_string_lossy() }),
            PendingAction::VideoPosterImport { token: token.to_owned(), temp_path },
        ).is_none() {
            self.cleanup_video_poster_temp(token);
            self.video_insert = None;
            self.set_error("동영상 poster를 가져오지 못해 노드를 추가하지 않았습니다. 원본 자산은 미참조 상태로 남을 수 있습니다".into());
        }
    }

    pub(super) fn complete_video_poster_import(&mut self, token: &str, temp_path: &std::path::Path, value: &Value) {
        let _ = fs::remove_file(temp_path);
        let Some((intent, canceled, poster)) = self.video_insert.as_ref()
            .filter(|pending| pending.intent.token == token && pending.stage == VideoInsertStage::ImportingPoster)
            .and_then(|pending| Some((pending.intent.clone(), pending.cancel_requested, pending.poster.clone()?)))
        else { return };
        let Ok(asset) = serde_json::from_value::<AssetRef>(value.clone()) else {
            self.cleanup_video_poster_temp(token);
            self.video_insert = None;
            self.set_error("가져온 poster 자산 정보를 읽지 못해 동영상 노드를 추가하지 않았습니다. 자산은 미참조 상태로 남을 수 있습니다".into());
            return;
        };
        if !poster_asset_matches(&asset, &poster) {
            self.cleanup_video_poster_temp(token);
            self.video_insert = None;
            self.set_error("가져온 poster 자산이 추출한 PNG와 달라 동영상 노드를 추가하지 않았습니다. 자산은 미참조 상태로 남을 수 있습니다".into());
            return;
        }
        if canceled || !self.video_insert_intent_is_current(&intent) {
            self.cleanup_video_poster_temp(token);
            self.video_insert = None;
            self.notices.warning("동영상 노드를 추가하지 않았습니다. 가져온 원본·poster 자산은 미참조 상태로 남을 수 있습니다", super::Instant::now());
            return;
        }
        let Some(source_asset) = self.video_insert.as_ref().and_then(|pending| pending.source_asset.clone()) else {
            self.cleanup_video_poster_temp(token);
            self.video_insert = None;
            self.set_error("동영상 원본 자산을 확인할 수 없어 노드를 추가하지 않았습니다".into());
            return;
        };
        if let Some(pending) = self.video_insert.as_mut().filter(|pending| pending.intent.token == token) {
            pending.poster_asset = Some(asset.clone());
            pending.stage = VideoInsertStage::Applying;
            pending.message = None;
        }
        let Some((node_id, source_path)) = self.video_insert.as_ref()
            .filter(|pending| pending.intent.token == token)
            .map(|pending| (pending.node_id.clone(), pending.source_path.clone()))
        else {
            self.cleanup_video_poster_temp(token);
            self.video_insert = None;
            self.set_error("동영상 추가 상태가 바뀌어 노드를 만들지 않았습니다".into());
            return;
        };
        let node = video_node(node_id, &source_path, source_asset, asset);
        let ticket = self.apply_changes(
            vec![Operation::AddNode { page_id: intent.page_id.clone(), parent_id: None, node }],
            [Intent::Content].into_iter().collect(),
            intent.expected_revision,
            PendingAction::VideoInsertApply { token: token.to_owned() },
        );
        let Some(ticket) = ticket else {
            self.cleanup_video_poster_temp(token);
            self.video_insert = None;
            self.set_error("동영상 노드를 추가하지 못했습니다. 가져온 자산은 미참조 상태로 남을 수 있습니다".into());
            return;
        };
        let apply = self.pending.get(&ticket).and_then(|pending| pending.apply.clone());
        if let Some(pending) = self.video_insert.as_mut().filter(|pending| pending.intent.token == token) {
            pending.apply = apply;
        }
    }

    fn finish_video_offscreen_import(
        &mut self,
        token: &str,
        result: &Result<crate::rpc::RpcPayload, DltError>,
        temp_path: Option<&std::path::Path>,
    ) {
        if let Some(path) = temp_path {
            let _ = fs::remove_file(path);
        }
        self.cleanup_video_poster_temp(token);
        let Some(pending) = self.video_insert.as_ref().filter(|pending| pending.intent.token == token) else {
            return;
        };
        let has_source = pending.source_asset.is_some()
            || matches!(result, Ok(crate::rpc::RpcPayload::Json(_)) if pending.stage == VideoInsertStage::ImportingSource);
        self.video_insert = None;
        let detail = match result {
            Ok(_) => "가져온 자산은 원래 작업물에서 사용되지 않은 상태로 남을 수 있습니다".to_owned(),
            Err(error) => format!("가져오기에 실패했습니다: {}", super::user_error(error)),
        };
        let prefix = if has_source { "다른 작업물로 이동해 동영상 노드를 추가하지 않았습니다." } else { "다른 작업물로 이동해 동영상 추가를 취소했습니다." };
        self.notices.warning(format!("{prefix} {detail}"), super::Instant::now());
    }

    fn finish_video_insert_offscreen(
        &mut self,
        token: &str,
        result: &Result<crate::rpc::RpcPayload, DltError>,
    ) {
        match result {
            Ok(_) => {
                self.video_insert = None;
                self.notices.info("동영상 노드를 원래 작업물에 추가했습니다", super::Instant::now());
            }
            Err(error) if super::messages::change_result_unknown(error) => {
                self.set_video_uncertain_message("원래 작업물에서 요청 결과가 불명확합니다. 해당 작업물을 다시 열어 중복 없이 확인하세요".into());
            }
            Err(error) => {
                self.cleanup_video_poster_temp(token);
                self.video_insert = None;
                self.notices.warning(format!(
                    "원래 작업물에 동영상 노드를 추가하지 못했습니다. 가져온 자산은 미참조 상태로 남을 수 있습니다: {}",
                    super::user_error(error)
                ), super::Instant::now());
            }
        }
    }

    pub(super) fn cleanup_video_poster_temp(&mut self, token: &str) {
        if let Some(path) = self.video_insert.as_mut()
            .filter(|pending| pending.intent.token == token)
            .and_then(|pending| pending.poster_temp_path.take())
        {
            let _ = fs::remove_file(path);
        }
    }
}

fn video_node(node_id: String, source_path: &std::path::Path, source: AssetRef, poster: AssetRef) -> Node {
    Node {
        id: node_id,
        kind: NodeKind::Video,
        bounds: Rect {
            x: 40 * designllm_contracts::EMU_PER_CSS_PX,
            y: 40 * designllm_contracts::EMU_PER_CSS_PX,
            width: 320 * designllm_contracts::EMU_PER_CSS_PX,
            height: 180 * designllm_contracts::EMU_PER_CSS_PX,
        },
        text: String::new(),
        style: Style::default(),
        asset: Some(source),
        children: Vec::new(),
        layout: None,
        interactions: Vec::new(),
        variants: BTreeMap::new(),
        ext: NodeExt {
            alt: Some(video_alt_from_path(source_path)),
            video_poster_asset: Some(poster),
            ..NodeExt::default()
        },
    }
}

pub(super) fn video_alt_from_path(path: &std::path::Path) -> String {
    let stem = path.file_stem().and_then(|value| value.to_str()).unwrap_or_default();
    let alt: String = stem.chars().filter(|character| !character.is_control()).take(256).collect();
    let alt = alt.trim();
    if alt.is_empty() { "동영상".into() } else { alt.to_owned() }
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn source_asset_matches(asset: &AssetRef, source: &designllm_web_preview::VideoSourceInfo) -> bool {
    asset.sha256 == source.sha256
        && asset.mime == source.mime
        && asset.bytes == source.bytes
        && valid_sha256(&asset.sha256)
}

fn poster_asset_matches(asset: &AssetRef, poster: &designllm_web_preview::VideoPoster) -> bool {
    asset.sha256 == sha256(&poster.png)
        && asset.mime == "image/png"
        && asset.bytes == poster.png.len() as u64
        && valid_sha256(&asset.sha256)
}

const VIDEO_POSTER_TEMP_PREFIX: &str = "designllm-video-poster-";

/// At start: the poster PNGs and poster work folders that a forced exit during a video insert left behind (D19.5). Those of
/// another running window stay.
pub(crate) fn remove_orphaned_video_poster_temp() {
    remove_orphaned_video_posters();
    designllm_web_preview::remove_orphaned_video_poster_work();
}

pub(super) fn remove_orphaned_video_posters() -> usize {
    designllm_web_preview::remove_orphaned_temp_files(VIDEO_POSTER_TEMP_PREFIX, ".png")
}

/// The name carries this process's tag, so that a later start can tell an abandoned poster from another window's.
pub(super) fn write_video_poster_temp(png: &[u8]) -> std::io::Result<PathBuf> {
    let directory = std::env::temp_dir();
    for _ in 0..8 {
        let path = directory.join(format!("{VIDEO_POSTER_TEMP_PREFIX}{}{}.png", designllm_web_preview::process_tag(), new_id()));
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut file) => {
                let result = file.write_all(png).and_then(|()| file.sync_all());
                if let Err(error) = result {
                    let _ = fs::remove_file(&path);
                    return Err(error);
                }
                return Ok(path);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "could not allocate a unique temporary poster path",
    ))
}

pub(super) fn projected_canvas_asset_bytes(
    document: &designllm_contracts::CanvasDocument,
    additional: [&AssetRef; 2],
) -> Result<u64, String> {
    let mut assets = BTreeMap::new();
    for page in &document.pages {
        for node in &page.nodes {
            collect_node_assets(node, &mut assets)?;
        }
    }
    for asset in additional {
        insert_canonical_asset(&mut assets, asset)?;
    }
    let total = assets.values().try_fold(0_u64, |total, asset| total.checked_add(asset.bytes))
        .ok_or_else(|| "Canvas 자산 용량 합계가 범위를 벗어났습니다".to_owned())?;
    if total > MAX_CANVAS_EMBEDDED_ASSET_BYTES {
        return Err("동영상 원본과 첫 장면을 포함한 Canvas 고유 자산이 128 MiB 제한을 넘습니다. 자산을 가져오지 않았습니다".into());
    }
    Ok(total)
}

fn collect_node_assets(node: &Node, assets: &mut BTreeMap<String, AssetRef>) -> Result<(), String> {
    if let Some(asset) = &node.asset {
        insert_canonical_asset(assets, asset)?;
    }
    if let Some(asset) = &node.ext.video_poster_asset {
        insert_canonical_asset(assets, asset)?;
    }
    for child in &node.children {
        collect_node_assets(child, assets)?;
    }
    Ok(())
}

fn insert_canonical_asset(assets: &mut BTreeMap<String, AssetRef>, asset: &AssetRef) -> Result<(), String> {
    if let Some(existing) = assets.get(&asset.sha256) {
        if existing != asset {
            return Err("같은 SHA-256의 자산 메타데이터가 서로 달라 용량을 안전하게 계산하지 못했습니다. 자산을 가져오지 않았습니다".into());
        }
    } else {
        assets.insert(asset.sha256.clone(), asset.clone());
    }
    Ok(())
}

fn find_node_in_page<'a>(nodes: &'a [Node], node_id: &str) -> Option<&'a Node> {
    for node in nodes {
        if node.id == node_id {
            return Some(node);
        }
        if let Some(found) = find_node_in_page(&node.children, node_id) {
            return Some(found);
        }
    }
    None
}
