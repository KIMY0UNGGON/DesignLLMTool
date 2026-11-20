use super::*;
use super::{
    doc_tree::{bmp_dimensions, canvas_image_node_with_state, find_node},
    messages::change_result_unknown,
    state::{image_crop_fields, pixel_rect_within, ImageReplacementIntent},
};

impl DesktopApp {
    pub(super) fn image_replacement_snapshot(&self) -> Result<ImageReplacementIntent, String> {
        if self.run_context.as_ref().is_some_and(|run| run.command_id != "canvas.image.replace") {
            return Err("현재 자동 작업이 끝난 뒤 이미지를 교체하세요".into());
        }
        let (Some(active), Some(artifact), Some(context)) = (&self.active, &self.artifact, &self.context) else {
            return Err("작업물을 아직 읽는 중입니다".into());
        };
        if !self.screen_is_editor() || active.kind != ArtifactKind::Canvas || artifact.kind != ArtifactKind::Canvas
            || active.id != artifact.id || artifact.id != context.artifact_id
            || self.view_id != context.view_id
            || active.revision != artifact.revision || artifact.revision != context.revision
        {
            return Err("현재 캔버스 revision을 확인할 수 없습니다".into());
        }
        if !self.feature_edit_supported(Feature::Images) {
            return Err("이 캔버스에서는 이미지를 교체할 수 없습니다".into());
        }
        if self.artifact_revision_change_pending(&active.id) {
            return Err("같은 작업물의 다른 변경이 끝난 뒤 이미지를 교체하세요".into());
        }
        if self.selected_nodes.len() != 1 || self.selected_node.as_deref() != self.selected_nodes.first().map(String::as_str) {
            return Err("이미지 하나만 선택하세요".into());
        }
        let node_id = self.selected_nodes[0].clone();
        let ArtifactBody::Canvas(document) = &artifact.body else { return Err("캔버스에서만 이미지를 교체할 수 있습니다".into()) };
        let mut found = None;
        for (page_index, page) in document.pages.iter().enumerate() {
            if let Some((node, inside_instance, locked)) = canvas_image_node_with_state(&page.nodes, &node_id, false, false) {
                found = Some((page_index as u32, page, node, inside_instance, locked));
                break;
            }
        }
        let Some((page_index, page, node, inside_instance, locked)) = found else {
            return Err("선택한 캔버스 이미지를 찾지 못했습니다".into());
        };
        if node.kind != NodeKind::Image || inside_instance || page.role == Some(designllm_contracts::PageRole::Component) {
            return Err("컴포넌트 원본이나 인스턴스 안의 이미지는 이 경로로 교체할 수 없습니다".into());
        }
        if locked {
            return Err("잠긴 이미지는 교체할 수 없습니다".into());
        }
        if artifact.figure_links.iter().any(|link| link.target_node_id == node_id) {
            return Err("그림 연결 이미지의 원본은 이 경로로 교체할 수 없습니다".into());
        }
        if self.image_property_mutation_pending(&node_id) {
            return Err("이미지 맞춤이나 대체 텍스트 변경이 끝난 뒤 교체하세요".into());
        }
        let asset = node.asset.as_ref().ok_or("이미지 원본 자산을 확인할 수 없습니다")?;
        if asset.sha256.len() != 64 || !asset.sha256.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err("현재 이미지 자산의 SHA-256을 확인할 수 없습니다".into());
        }
        if self.pending.values().any(|pending| matches!(&pending.action, PendingAction::HitTest { .. })) {
            return Err("선택 중에는 이미지를 교체할 수 없습니다".into());
        }
        let selection_set_pending = self.pending.values().any(|pending| matches!(&pending.action, PendingAction::Selection));
        if !selection_set_pending && (context.selection.node_ids.as_slice() != [node_id.as_str()]
            || self.local_selection.as_ref().is_none_or(|selection| selection.node_ids.as_slice() != [node_id.as_str()]))
        {
            return Err("선택 상태가 확정될 때까지 기다려 주세요".into());
        }
        Ok(ImageReplacementIntent {
            token: String::new(),
            artifact_id: artifact.id.clone(),
            expected_revision: artifact.revision,
            page_index,
            page_id: page.id.clone(),
            node_id,
            expected_asset_sha256: asset.sha256.clone(),
            selection_generation: self.selection_generation,
            view_id: self.view_id.clone(),
        })
    }

    pub(super) fn image_replacement_intent_is_current(&self, expected: &ImageReplacementIntent) -> bool {
        self.image_replacement_snapshot().is_ok_and(|current| {
            current.artifact_id == expected.artifact_id
                && current.expected_revision == expected.expected_revision
                && current.page_index == expected.page_index
                && current.page_id == expected.page_id
                && current.node_id == expected.node_id
                && current.expected_asset_sha256 == expected.expected_asset_sha256
                && current.selection_generation == expected.selection_generation
                && current.view_id == expected.view_id
        })
    }

    /// Whether the image button of `node_id` is on: a replacement of that image can start now (`image_replacement_check`).
    pub(crate) fn canvas_image_replacement_available(&self, node_id: &str) -> bool {
        self.image_replacement_check().is_ok_and(|intent| intent.node_id == node_id)
    }

    pub(crate) fn image_crop_edit_open(&self, node_id: &str) -> bool {
        self.image_crop_editor.as_ref().is_some_and(|editor| {
            editor.intent.node_id == node_id
                && self.active.as_ref().is_some_and(|active| active.id == editor.intent.artifact_id)
        })
    }

    pub(crate) fn image_property_change_pending(&self, node_id: &str) -> bool {
        self.pending_image_replacement.as_ref().is_some_and(|pending| pending.intent.node_id == node_id)
            || self.image_property_mutation_pending(node_id)
    }

    pub(super) fn image_property_mutation_pending(&self, node_id: &str) -> bool {
        self.held_edits.iter().any(|held| {
            self.active.as_ref().is_some_and(|active| held.apply.artifact_id == active.id)
                && held.nodes.iter().any(|target| target == node_id)
        }) || self.pending.values().any(|pending| {
            pending.apply.as_ref().is_some_and(|apply| {
                self.active.as_ref().is_some_and(|active| active.id == apply.artifact_id)
                    && apply.operations.iter().any(|operation| match operation {
                        Operation::SetNodeAppearance { node_ids, .. } | Operation::SetNodeInfo { node_ids, .. } => {
                            node_ids.iter().any(|target| target == node_id)
                        }
                        _ => false,
                    })
            })
        })
    }

    /// Whether a request that moves the document's revision is on its way. A proposal's apply commits its change set like
    /// `changes.apply` does, so it is one too (as in the save check, `artifact_write_has_pending_edits`).
    pub(super) fn artifact_revision_change_pending(&self, artifact_id: &str) -> bool {
        self.pending.values().any(|pending| {
            pending.artifact_id.as_deref() == Some(artifact_id)
                && matches!(pending.method.as_str(),
                    "changes.apply" | "proposal.apply" | "history.undo" | "history.redo" | "history.undo_change"
                        | "design_system.publish" | "design_system.set" | "job.start" | "job.resume" | "figure.dataset.edit")
        })
    }

    /// The conditions of a canvas image replacement, shared by the image button and `canvas.image.replace`: no replacement
    /// under way, no crop editor open on the selected image, no property change of it pending, and the image selected
    /// (`image_replacement_snapshot`). Returns the selected image's intent, without a token.
    pub(super) fn image_replacement_check(&self) -> Result<ImageReplacementIntent, String> {
        if self.pending_image_replacement.is_some() {
            return Err("이미지 교체가 이미 진행 중입니다".into());
        }
        let intent = self.image_replacement_snapshot()?;
        let crop_open = self.image_crop_editor.as_ref().is_some_and(|editor| {
            editor.intent.node_id == intent.node_id
                && self.active.as_ref().is_some_and(|active| active.id == editor.intent.artifact_id)
        });
        if crop_open {
            return Err("이 이미지의 자르기 편집기가 열려 있습니다. 편집기를 닫은 뒤 교체하세요".into());
        }
        if self.image_property_change_pending(&intent.node_id) {
            return Err("이 이미지의 맞춤이나 대체 텍스트 변경이 끝난 뒤 교체하세요".into());
        }
        Ok(intent)
    }

    /// The intent a replacement starts from: `image_replacement_check` with a new token. The image button (after its
    /// dialog) and `canvas.image.replace` both start here.
    pub(super) fn image_replacement_start(&self) -> Result<ImageReplacementIntent, String> {
        let mut intent = self.image_replacement_check()?;
        intent.token = new_id();
        Ok(intent)
    }

    pub(crate) fn begin_canvas_image_replacement(&mut self) -> Result<(), String> {
        let intent = self.image_replacement_start()?;
        let Some(path) = crate::dialogs::open_image_replacement(self.parent_hwnd) else { return Ok(()) };
        self.start_canvas_image_replacement_import(intent, path)
    }

    pub(super) fn start_canvas_image_replacement_import(&mut self, intent: ImageReplacementIntent, path: PathBuf) -> Result<(), String> {
        let supported_extension = path.extension().and_then(|extension| extension.to_str())
            .is_some_and(|extension| matches!(extension.to_ascii_lowercase().as_str(), "png" | "jpg" | "jpeg" | "bmp"));
        if !supported_extension {
            return Err("PNG, JPEG 또는 BMP 이미지만 교체할 수 있습니다".into());
        }
        if path.extension().and_then(|extension| extension.to_str()).is_some_and(|extension| extension.eq_ignore_ascii_case("bmp"))
            && bmp_dimensions(&path).is_none()
        {
            return Err("BMP 헤더에서 이미지 크기를 확인하지 못했습니다".into());
        }
        if !self.image_replacement_intent_is_current(&intent) {
            return Err("파일을 고르는 동안 선택이나 작업물이 바뀌어 교체하지 않았습니다".into());
        }
        self.pending_image_replacement = Some(PendingImageReplacement { intent: intent.clone(), stage: ImageReplacementStage::Importing });
        if self.call(
            "asset.import",
            json!({ "path": path.to_string_lossy() }),
            PendingAction::ImageReplacementImport { token: intent.token.clone() },
        ).is_none() {
            self.pending_image_replacement = None;
            return Err("이미지를 작업공간에 가져오지 못했습니다".into());
        }
        Ok(())
    }

    pub(crate) fn cancel_canvas_image_replacement(&mut self) {
        if self.pending_image_replacement.as_ref().is_some_and(|pending| pending.stage == ImageReplacementStage::Importing) {
            self.pending_image_replacement = None;
        }
    }

    pub(super) fn invalidate_image_replacement_if_target_moved(&mut self) {
        let stale = self.pending_image_replacement.as_ref().is_some_and(|pending| {
            pending.stage == ImageReplacementStage::Importing && !self.image_replacement_intent_is_current(&pending.intent)
        });
        if stale {
            self.pending_image_replacement = None;
        }
    }

    pub(super) fn complete_image_replacement_import(&mut self, token: &str, value: &Value) {
        let Some(intent) = self.pending_image_replacement.as_ref()
            .filter(|pending| pending.stage == ImageReplacementStage::Importing && pending.intent.token == token)
            .map(|pending| pending.intent.clone()) else { return };
        if !self.image_replacement_intent_is_current(&intent) {
            self.pending_image_replacement = None;
            self.set_error("선택 또는 작업물이 바뀌어 이미지를 교체하지 않았습니다. 가져온 자산은 사용되지 않은 상태로 남을 수 있습니다".into());
            return;
        }
        let Ok(asset) = serde_json::from_value::<AssetRef>(value.clone()) else {
            self.pending_image_replacement = None;
            self.set_error("가져온 이미지 정보를 읽지 못했습니다. 원본 이미지는 유지했으며 자산은 사용되지 않은 채 남을 수 있습니다".into());
            return;
        };
        let dimensions = value.get("image");
        let reported_dimensions = dimensions.and_then(|image| image.get("width_px")).and_then(Value::as_u64).is_some_and(|width| width > 0)
            && dimensions.and_then(|image| image.get("height_px")).and_then(Value::as_u64).is_some_and(|height| height > 0);
        let valid_dimensions = reported_dimensions || asset.mime == "image/bmp";
        if !matches!(asset.mime.as_str(), "image/png" | "image/jpeg" | "image/bmp") || !valid_dimensions || asset.bytes == 0
            || asset.sha256.len() != 64 || !asset.sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            self.pending_image_replacement = None;
            self.set_error("PNG, JPEG 또는 BMP 이미지를 확인할 수 없습니다. 원본 이미지는 유지했으며 자산은 사용되지 않은 채 남을 수 있습니다".into());
            return;
        }
        if asset.sha256 == intent.expected_asset_sha256 {
            self.pending_image_replacement = None;
            return;
        }
        if let Some(pending) = self.pending_image_replacement.as_mut().filter(|pending| pending.intent.token == token) {
            pending.stage = ImageReplacementStage::Applying;
        } else {
            return;
        }
        // The change goes through the replacement command (operation_commands.rs), which completes it as this import's own change.
        if self.invoke("canvas.image.replace", json!({ "node_id": intent.node_id, "asset_sha256": asset.sha256 })).is_err() {
            self.pending_image_replacement = None;
            self.set_error("이미지를 교체하지 못했습니다. 원본 이미지는 유지했으며 가져온 자산은 사용되지 않은 채 남을 수 있습니다".into());
        }
    }

    pub(super) fn fail_image_replacement_import(&mut self, token: &str, error: &DltError) {
        if self.pending_image_replacement.as_ref().is_none_or(|pending| {
            pending.intent.token != token || pending.stage != ImageReplacementStage::Importing
        }) {
            return;
        }
        self.pending_image_replacement = None;
        self.set_error(format!("이미지를 가져오지 못했습니다. 원본 이미지는 유지했습니다: {}", user_error(error)));
    }

    pub(crate) fn canvas_image_crop_available(&self, node_id: &str) -> bool {
        self.image_crop_editor.is_none()
            && self.pending_image_replacement.is_none()
            && self.image_replacement_snapshot().is_ok_and(|intent| {
                intent.node_id == node_id && self.image_crop_target_is_current(&intent)
            })
    }

    pub(crate) fn begin_canvas_image_crop(&mut self, node_id: &str) -> Result<(), String> {
        if self.image_crop_editor.is_some() {
            return Err("진행 중인 이미지 자르기 초안을 먼저 적용하거나 복사·버리세요".into());
        }
        if self.pending_image_replacement.is_some() {
            return Err("이미지 교체가 끝난 뒤 자르기를 시작하세요".into());
        }
        let mut intent = self.image_replacement_snapshot()?;
        if intent.node_id != node_id {
            return Err("선택한 이미지가 바뀌었습니다. 다시 선택하세요".into());
        }
        if self.reservations_for.as_deref() != Some(intent.artifact_id.as_str()) {
            return Err("이미지 예약 상태를 확인한 뒤 자르기를 시작하세요".into());
        }
        if self.has_reservation_for_nodes(std::slice::from_ref(&intent.node_id)) {
            return Err("다른 작업이 이 이미지를 사용하는 중입니다. 끝난 뒤 다시 선택하세요".into());
        }
        let original = self.artifact.as_ref()
            .and_then(|artifact| find_node(artifact, node_id))
            .and_then(|node| node.ext.image_crop_pixel_rect);
        intent.token = new_id();
        let token = intent.token.clone();
        self.image_crop_editor = Some(ImageCropEditor {
            intent: intent.clone(),
            stage: ImageCropStage::Loading,
            original,
            dimensions: None,
            draft: None,
            requested_crop: None,
            outcome_uncertain: false,
            refresh_requested: false,
            refresh_failed: false,
            message: None,
        });
        if self.call(
            "asset.lookup",
            json!({ "sha256": intent.expected_asset_sha256 }),
            PendingAction::ImageCropLookup { token: token.clone() },
        ).is_none() {
            self.image_crop_editor = None;
            return Err("원본 이미지 크기를 확인하지 못했습니다".into());
        }
        Ok(())
    }

    pub(super) fn image_crop_target_is_current(&self, intent: &ImageReplacementIntent) -> bool {
        self.image_replacement_intent_is_current(intent)
            && self.reservations_for.as_deref() == Some(intent.artifact_id.as_str())
            && !self.has_reservation_for_nodes(std::slice::from_ref(&intent.node_id))
    }

    pub(super) fn retry_canvas_image_crop_lookup(&mut self) {
        let Some(editor) = self.image_crop_editor.as_ref().filter(|editor| {
            editor.stage == ImageCropStage::Editing && editor.dimensions.is_none()
        }) else { return };
        let intent = editor.intent.clone();
        if !self.image_crop_target_is_current(&intent) {
            if let Some(editor) = self.image_crop_editor.as_mut() {
                editor.message = Some("작업물 또는 이미지가 바뀌었습니다. 초안을 복사하거나 버리세요".into());
            }
            return;
        }
        if let Some(editor) = self.image_crop_editor.as_mut() {
            editor.stage = ImageCropStage::Loading;
            editor.message = None;
        }
        if self.call(
            "asset.lookup",
            json!({ "sha256": intent.expected_asset_sha256 }),
            PendingAction::ImageCropLookup { token: intent.token.clone() },
        ).is_none() {
            if let Some(editor) = self.image_crop_editor.as_mut() {
                editor.stage = ImageCropStage::Editing;
                editor.message = Some("원본 이미지 크기를 확인하지 못했습니다. 다시 시도하거나 초안을 버리세요".into());
            }
        }
    }

    pub(super) fn complete_image_crop_lookup(&mut self, token: &str, value: &Value) {
        let Some(intent) = self.image_crop_editor.as_ref()
            .filter(|editor| editor.stage == ImageCropStage::Loading && editor.intent.token == token)
            .map(|editor| editor.intent.clone()) else { return };
        let asset = serde_json::from_value::<AssetRef>(value.clone());
        let dimensions = value.get("image").and_then(|image| {
            let width = image.get("width_px")?.as_u64().and_then(|value| u32::try_from(value).ok())?;
            let height = image.get("height_px")?.as_u64().and_then(|value| u32::try_from(value).ok())?;
            (width > 0 && height > 0).then_some((width, height))
        });
        let valid = asset.as_ref().is_ok_and(|asset| {
            asset.sha256 == intent.expected_asset_sha256 && asset.mime.starts_with("image/") && asset.bytes > 0
        });
        let current = self.image_crop_target_is_current(&intent);
        let Some(editor) = self.image_crop_editor.as_mut().filter(|editor| editor.intent.token == token) else { return };
        editor.stage = ImageCropStage::Editing;
        if !valid || dimensions.is_none() {
            editor.message = Some("원본 이미지 픽셀 크기를 확인하지 못했습니다. 원본은 바뀌지 않았습니다".into());
            return;
        }
        let (width, height) = dimensions.expect("validated image dimensions");
        editor.dimensions = Some((width, height));
        let initial = editor.original.unwrap_or(designllm_contracts::PixelRect { x: 0, y: 0, width, height });
        if editor.draft.is_none() {
            editor.draft = Some(image_crop_fields(initial));
        }
        editor.message = (!current).then(|| "작업물 또는 이미지가 바뀌었습니다. 최신 상태를 확인한 뒤 명시적으로 다시 적용하거나 초안을 복사하세요".into());
    }

    pub(super) fn fail_image_crop_lookup(&mut self, token: &str, error: &DltError) {
        let Some(editor) = self.image_crop_editor.as_mut().filter(|editor| {
            editor.stage == ImageCropStage::Loading && editor.intent.token == token
        }) else { return };
        editor.stage = ImageCropStage::Editing;
        editor.message = Some(format!("원본 이미지 크기를 읽지 못했습니다. 초안은 보존했습니다: {}", user_error(error)));
    }

    pub(super) fn fail_image_crop_refresh(&mut self, artifact_id: Option<&str>) {
        if let Some(editor) = self.image_crop_editor.as_mut().filter(|editor| {
            editor.refresh_requested && artifact_id == Some(editor.intent.artifact_id.as_str())
        }) {
            editor.refresh_failed = true;
            editor.message = Some("최신 작업물 또는 예약 상태를 읽지 못했습니다. 자르기 초안을 보존했습니다".into());
        }
    }

    pub(crate) fn update_canvas_image_crop_draft(&mut self, fields: [String; 4]) {
        if let Some(editor) = self.image_crop_editor.as_mut().filter(|editor| editor.stage == ImageCropStage::Editing) {
            if editor.outcome_uncertain { return; }
            editor.draft = Some(fields);
            editor.requested_crop = None;
            editor.message = None;
        }
    }

    pub(crate) fn apply_canvas_image_crop(&mut self, crop: Option<designllm_contracts::PixelRect>) -> Result<(), String> {
        let Some(editor) = self.image_crop_editor.as_ref().filter(|editor| editor.stage == ImageCropStage::Editing) else {
            return Err("이미지 자르기 편집기가 준비되지 않았습니다".into());
        };
        let intent = editor.intent.clone();
        let dimensions = editor.dimensions.ok_or("원본 이미지 픽셀 크기를 확인하지 못했습니다")?;
        if editor.outcome_uncertain {
            return Err("이전 요청 결과를 먼저 최신 상태에서 확인하세요".into());
        }
        if crop.is_some_and(|rect| !pixel_rect_within(rect, dimensions)) {
            return Err("자르기 사각형은 원본 이미지 안의 0보다 큰 픽셀 범위여야 합니다".into());
        }
        if !self.image_crop_target_is_current(&intent) {
            if let Some(editor) = self.image_crop_editor.as_mut() {
                editor.message = Some("revision, 선택, 잠금 또는 이미지 원본이 바뀌었습니다. 초안을 보존했습니다. 최신 상태를 확인하세요".into());
            }
            return Err("작업물이나 이미지가 바뀌어 자르기를 적용하지 않았습니다. 초안은 보존했습니다".into());
        }
        if crop == editor.original {
            self.image_crop_editor = None;
            return Ok(());
        }
        if self.artifact_revision_change_pending(&intent.artifact_id) {
            if let Some(editor) = self.image_crop_editor.as_mut() {
                editor.message = Some("같은 작업물의 다른 변경이 끝나지 않았습니다. 초안은 보존했습니다".into());
            }
            return Err("같은 작업물의 변경이 끝난 뒤 자르기를 적용하세요".into());
        }
        if let Some(editor) = self.image_crop_editor.as_mut() {
            editor.requested_crop = Some(crop);
            editor.stage = ImageCropStage::Applying;
            editor.message = None;
        }
        // The change goes through the crop command (operation_commands.rs), which completes it as this editor's own change.
        let mut args = json!({ "node_id": intent.node_id, "expected_asset_sha256": intent.expected_asset_sha256 });
        if let Some(rect) = crop {
            args["x"] = json!(rect.x);
            args["y"] = json!(rect.y);
            args["width"] = json!(rect.width);
            args["height"] = json!(rect.height);
        }
        if self.invoke("canvas.image.crop", args).is_err() {
            if let Some(editor) = self.image_crop_editor.as_mut().filter(|editor| editor.intent.token == intent.token) {
                editor.stage = ImageCropStage::Editing;
                editor.message = Some("자르기를 보내지 못했습니다. 초안은 보존했습니다".into());
            }
            return Err("이미지 자르기를 적용하지 못했습니다. 초안은 보존했습니다".into());
        }
        Ok(())
    }

    pub(crate) fn rebase_canvas_image_crop(&mut self) -> Result<(), String> {
        let Some(editor) = self.image_crop_editor.as_ref().filter(|editor| editor.stage == ImageCropStage::Editing) else {
            return Err("이미지 자르기 초안을 확인할 수 없습니다".into());
        };
        let uncertain = editor.outcome_uncertain;
        if !editor.refresh_requested || editor.refresh_failed {
            if let Some(editor) = self.image_crop_editor.as_mut() {
                editor.refresh_requested = true;
                editor.refresh_failed = false;
                editor.message = Some("최신 작업물과 이미지 상태를 확인하고 있습니다. 확인이 끝난 뒤 다시 눌러 초안을 재검증하세요".into());
            }
            self.reservations.clear();
            self.user_reservations.clear();
            self.reservations_for = None;
            self.refresh_active();
            let old_artifact_id = self.image_crop_editor.as_ref().expect("editor checked").intent.artifact_id.clone();
            self.call("targets.list", json!({ "artifact_id": old_artifact_id }), PendingAction::Reservations);
            let (inspect_pending, context_pending, reservations_pending) = self.image_crop_refresh_reads_pending(
                &self.image_crop_editor.as_ref().expect("editor checked").intent.artifact_id,
            );
            if !inspect_pending || !context_pending || !reservations_pending {
                if let Some(editor) = self.image_crop_editor.as_mut() {
                    editor.refresh_failed = true;
                    editor.message = Some("최신 작업물 상태를 요청하지 못했습니다. 초안을 보존했습니다".into());
                }
                return Err("최신 작업물 상태를 요청하지 못했습니다".into());
            }
            return Ok(());
        }
        let old = self.image_crop_editor.as_ref().expect("editor checked").intent.clone();
        if self.pending.values().any(|pending| {
            pending.artifact_id.as_deref() == Some(old.artifact_id.as_str())
                && matches!(pending.method.as_str(), "artifact.inspect" | "context.read" | "targets.list")
        }) {
            if let Some(editor) = self.image_crop_editor.as_mut() {
                editor.message = Some("최신 작업물 상태를 읽고 있습니다. 완료될 때까지 기다리세요".into());
            }
            return Err("최신 작업물 상태 확인이 끝나지 않았습니다".into());
        }
        if self.image_crop_editor.as_ref().is_some_and(|editor| editor.refresh_failed) {
            return Err("최신 작업물 상태를 읽지 못했습니다. 초안을 보존했습니다".into());
        }
        let mut current = match self.image_replacement_snapshot() {
            Ok(current) => current,
            Err(_) => {
                if let Some(editor) = self.image_crop_editor.as_mut() {
                    editor.message = Some("선택한 이미지와 최신 작업물 상태를 확인하지 못했습니다. 초안은 보존했습니다".into());
                }
                return Err("선택한 이미지의 최신 상태를 확인하지 못했습니다. 초안을 보존했습니다".into());
            }
        };
        if current.node_id != old.node_id || current.expected_asset_sha256 != old.expected_asset_sha256
            || current.artifact_id != old.artifact_id
        {
            let editor = self.image_crop_editor.as_mut().expect("editor checked");
            editor.message = Some("선택 대상이나 이미지 원본이 달라졌습니다. 초안을 복사한 뒤 현재 이미지용 자르기를 새로 시작하세요".into());
            return Err("선택 대상이나 이미지 원본이 바뀌었습니다. 초안을 보존했습니다".into());
        }
        current.token = old.token;
        let current_crop = self.artifact.as_ref().and_then(|artifact| find_node(artifact, &current.node_id))
            .and_then(|node| node.ext.image_crop_pixel_rect);
        if uncertain && self.image_crop_editor.as_ref().is_some_and(|editor| editor.requested_crop == Some(current_crop)) {
            self.image_crop_editor = None;
            self.notices.info("이미지 자르기 요청이 작업물에 반영된 것을 확인했습니다", Instant::now());
            return Ok(());
        }
        let Some(editor) = self.image_crop_editor.as_mut() else { return Err("이미지 자르기 초안이 사라졌습니다".into()) };
        editor.intent = current;
        editor.original = current_crop;
        editor.outcome_uncertain = false;
        editor.refresh_requested = false;
        editor.refresh_failed = false;
        editor.message = Some("최신 이미지와 revision을 확인했습니다. 초안은 원본 픽셀 좌표 그대로 보존했습니다".into());
        Ok(())
    }

    pub(crate) fn discard_canvas_image_crop(&mut self) {
        if let Some(token) = self.image_crop_editor.as_ref().filter(|editor| editor.stage != ImageCropStage::Applying).map(|editor| editor.intent.token.clone()) {
            self.clear_crop_draft(&token);
            self.image_crop_editor = None;
        }
    }

    pub(super) fn image_crop_apply_pending(&self) -> bool {
        self.image_crop_editor.as_ref().is_some_and(|editor| editor.stage == ImageCropStage::Applying)
            || self.pending.values().any(|pending| matches!(&pending.action, PendingAction::ImageCropApply { .. }))
    }

    pub(super) fn image_crop_refresh_reads_pending(&self, artifact_id: &str) -> (bool, bool, bool) {
        let mut inspect = false;
        let mut context = false;
        let mut reservations = false;
        for pending in self.pending.values().filter(|pending| pending.artifact_id.as_deref() == Some(artifact_id)) {
            inspect |= pending.method == "artifact.inspect";
            context |= pending.method == "context.read";
            reservations |= matches!(&pending.action, PendingAction::Reservations);
        }
        (inspect, context, reservations)
    }

    pub(super) fn complete_image_crop_apply(&mut self, token: &str, value: &Value) {
        if self.image_crop_editor.as_ref().is_some_and(|editor| {
            editor.intent.token == token && editor.stage == ImageCropStage::Applying
        }) {
            self.clear_crop_draft(token);
            self.image_crop_editor = None;
            self.committed(value);
        }
    }

    pub(super) fn fail_image_crop_apply(&mut self, token: &str, error: &DltError, refresh_active: bool) {
        let refresh_required = change_result_unknown(error)
            || matches!(error.code, ErrorCode::RevisionConflict | ErrorCode::TargetBusy | ErrorCode::ContentLocked | ErrorCode::NotFound);
        let message = {
            let Some(editor) = self.image_crop_editor.as_mut().filter(|editor| {
                editor.intent.token == token && editor.stage == ImageCropStage::Applying
            }) else { return };
            editor.stage = ImageCropStage::Editing;
            editor.outcome_uncertain = change_result_unknown(error);
            editor.refresh_requested = refresh_required && refresh_active;
            editor.refresh_failed = false;
            editor.message = Some(if editor.outcome_uncertain {
                "요청 결과를 확인하지 못했습니다. 중복 적용을 막기 위해 다시 보내지 않습니다. 최신 상태 확인 전까지 초안을 보존합니다".into()
            } else {
                format!("자르기를 적용하지 못했습니다. 원본은 유지했고 초안을 보존했습니다. 최신 상태를 확인하세요: {}", user_error(error))
            });
            editor.message.clone().unwrap_or_default()
        };
        self.set_error(message);
        if refresh_required && refresh_active {
            self.reservations.clear();
            self.user_reservations.clear();
            self.reservations_for = None;
            self.refresh_active();
            if let Some(artifact_id) = self.active.as_ref().map(|active| active.id.clone()) {
                self.call("targets.list", json!({ "artifact_id": artifact_id }), PendingAction::Reservations);
            }
            let artifact_id = self.image_crop_editor.as_ref().map(|editor| editor.intent.artifact_id.clone());
            if artifact_id.as_deref().is_some_and(|artifact_id| {
                let (inspect, context, reservations) = self.image_crop_refresh_reads_pending(artifact_id);
                !inspect || !context || !reservations
            }) {
                if let Some(editor) = self.image_crop_editor.as_mut().filter(|editor| editor.intent.token == token) {
                    editor.refresh_failed = true;
                    editor.message = Some("요청 결과를 확인할 최신 상태를 읽지 못했습니다. 초안을 보존했습니다".into());
                }
            }
        }
    }
}
