use super::{messages::engine_warning_message, print_range::{parse_page_range, range_supported}, state::SaveRpcOutcome, *};

/// PNG and SVG write one page (the slide engine and the core refuse several: slides lib.rs `export`, core files.rs `export`).
pub(super) const ONE_PAGE_FORMAT: &str = "PNG와 SVG는 한 번에 한 쪽만 내보냅니다. scope를 page로 하고 쪽을 고르세요";

impl DesktopApp {
    pub(super) fn save_succeeded(&mut self, request_id: &str, artifact_id: &str) {
        let mut event_received = false;
        let mut diagnostics = None;
        if let Some(pending) = self.pending_save_notices.get_mut(request_id) {
            if pending.artifact_id.as_str() != artifact_id {
                return;
            }
            pending.outcome = SaveRpcOutcome::Succeeded;
            event_received = pending.diagnostics.is_some();
            diagnostics = pending.diagnostics.clone();
        }
        if event_received {
            self.pending_save_notices.remove(request_id);
        }
        self.show_saved_notice(diagnostics.as_deref(), true);
    }

    pub(super) fn show_saved_notice(&mut self, diagnostics: Option<&[Diagnostic]>, confirm_success: bool) {
        if let Some(message) = diagnostics.and_then(engine_warning_message) {
            self.notices.warning(format!("저장했습니다. {message}"), Instant::now());
        } else if confirm_success {
            self.notices.info("저장했습니다", Instant::now());
        }
    }

    pub(super) fn save_event_confirms(&mut self, request_id: &str, artifact_id: &str) -> bool {
        let diagnostics = self.pending_save_notices.get(request_id).and_then(|pending| {
            (pending.artifact_id.as_str() == artifact_id).then(|| pending.diagnostics.clone()).flatten()
        });
        let Some(diagnostics) = diagnostics else { return false };
        self.pending_save_notices.remove(request_id);
        self.show_saved_notice(Some(&diagnostics), true);
        true
    }

    pub(super) fn record_save_notice(&mut self, event: &Value) {
        if event.get("event").and_then(Value::as_str) != Some("artifact.saved") {
            return;
        }
        let Some(detail) = event.get("detail") else { return };
        let Some(request_id) = detail.get("request_id").and_then(Value::as_str) else { return };
        let Some(artifact_id) = event.get("artifact_id").and_then(Value::as_str) else { return };
        let diagnostics = detail
            .get("diagnostics")
            .cloned()
            .and_then(|value| serde_json::from_value::<Vec<Diagnostic>>(value).ok())
            .unwrap_or_default();

        let mut outcome = None;
        if let Some(pending) = self.pending_save_notices.get_mut(request_id) {
            if pending.artifact_id.as_str() != artifact_id {
                return;
            }
            if pending.outcome == SaveRpcOutcome::AwaitingReply {
                pending.diagnostics = Some(diagnostics);
                return;
            }
            outcome = Some(pending.outcome);
        }
        if let Some(outcome) = outcome {
            self.pending_save_notices.remove(request_id);
            self.show_saved_notice(Some(&diagnostics), outcome == SaveRpcOutcome::Unknown);
        }
    }

    pub(super) fn prune_pending_save_notices(&mut self, now: Instant) {
        self.pending_save_notices.retain(|_, pending| now.duration_since(pending.started) < Duration::from_secs(120));
    }

    pub(super) fn open_file(&mut self) {
        self.open_file_as(None);
    }

    /// Opens `open_path` as `kind`; without one the core reads the kind from an Office extension, and a file of this tool is
    /// opened as the kind its `format` names (the core cannot tell it by the extension).
    pub(super) fn open_file_as(&mut self, kind: Option<ArtifactKind>) {
        let path = self.open_path.trim();
        if path.is_empty() {
            return;
        }
        let request = designllm_contracts::OpenRequest {
            request_id: new_id(),
            actor: "desktop".into(),
            path: path.into(),
            kind: kind.or_else(|| native_kind(std::path::Path::new(path))),
        };
        match serde_json::to_value(request) {
            Ok(params) => {
                self.call("artifact.open", params, PendingAction::Open);
            }
            Err(_) => self.set_error(String::from("파일을 열지 못했습니다. 경로와 파일 형식을 확인해 주세요")),
        }
    }

    /// Saves to the save location. Without one, the dialog of "다른 이름으로 저장" asks for it; a dialog that is cancelled or
    /// cannot be shown saves nothing, and the window says so.
    pub(super) fn save_active(&mut self) {
        let Some(active) = self.active.as_ref() else {
            self.set_error(String::from("작업물이 열려 있지 않습니다"));
            return;
        };
        if !self.feature_write_supported(Feature::NativeRoundtrip) {
            self.set_error(String::from("이 작업물의 원본 형식 저장은 지원되지 않습니다"));
            return;
        }
        if self.save_path.trim().is_empty() {
            let Some(path) = crate::dialogs::save_document(self.parent_hwnd, active.title.clone()) else {
                self.notices.warning(
                    String::from("저장 위치가 정해지지 않아 저장하지 않았습니다. '다른 이름으로 저장'으로 위치를 정해 주세요"),
                    Instant::now(),
                );
                return;
            };
            self.save_path = path.to_string_lossy().into_owned();
        }
        let request = SaveRequest {
            request_id: new_id(),
            artifact_id: active.id.clone(),
            expected_revision: active.revision,
            target_path: self.save_path.trim().into(),
            overwrite: false,
            expected_target_hash: None,
        };
        if let Err(message) = self.enqueue_save_request(request) {
            self.set_error(message);
        }
    }

    pub(super) fn save_active_to(&mut self, args: &Value) -> Result<(), String> {
        let target_path = args
            .get("target_path")
            .and_then(Value::as_str)
            .filter(|path| !path.trim().is_empty())
            .ok_or_else(|| String::from("저장할 target_path를 지정해 주세요"))?;
        if args.get("overwrite").is_some_and(|overwrite| !overwrite.is_boolean()) {
            return Err("overwrite는 true 또는 false여야 합니다".into());
        }
        if args.get("expected_target_hash").is_some_and(|hash| !hash.is_string()) {
            return Err("expected_target_hash는 문자열이어야 합니다".into());
        }
        if !self.feature_write_supported(Feature::NativeRoundtrip) {
            return Err("이 작업물의 원본 형식 저장은 지원되지 않습니다".into());
        }
        let active = self.active.as_ref().ok_or_else(|| String::from("작업물이 열려 있지 않습니다"))?;
        self.enqueue_save_request(SaveRequest {
            request_id: new_id(),
            artifact_id: active.id.clone(),
            expected_revision: active.revision,
            target_path: target_path.to_owned(),
            overwrite: args.get("overwrite").and_then(Value::as_bool).unwrap_or(false),
            expected_target_hash: args.get("expected_target_hash").and_then(Value::as_str).map(str::to_owned),
        })
    }

    pub(super) fn enqueue_save_request(&mut self, request: SaveRequest) -> Result<(), String> {
        let request_id = request.request_id.clone();
        let artifact_id = request.artifact_id.clone();
        let params = serde_json::to_value(request).map_err(|_| String::from("저장 요청을 만들지 못했습니다"))?;
        let now = Instant::now();
        self.prune_pending_save_notices(now);
        self.pending_save_notices.insert(request_id.clone(), PendingSaveNotice {
            artifact_id: artifact_id.clone(),
            outcome: SaveRpcOutcome::AwaitingReply,
            diagnostics: None,
            started: now,
        });
        if self.call("artifact.save", params, PendingAction::Save { request_id: request_id.clone(), artifact_id }).is_none() {
            self.pending_save_notices.remove(&request_id);
        }
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn export_active(&mut self) {
        self.export_active_with(None);
    }

    /// Sends the export that the popover's settings (`options` None) or the arguments of the save dialog describe: a dry
    /// run for its losses first, then the write.
    pub(super) fn export_active_with(&mut self, options: Option<&Value>) {
        if self.export_path.trim().is_empty() {
            return;
        }
        let pages = options.and_then(|options| options.get("pages")).and_then(Value::as_str).unwrap_or("");
        let Some(mut request) = self.export_request(self.export_path.trim().to_owned(), options) else {
            return;
        };
        // The losses the popover showed for these settings are the ones the user accepted with its button.
        if let Some((key, losses)) = &self.export_preview {
            if *key == self.export_key(pages) {
                request.options.allowed_losses = losses.iter().map(|loss| loss.code.clone()).collect::<BTreeSet<_>>().into_iter().collect();
            }
        }
        let mut dry_run = request.clone();
        dry_run.request_id = new_id();
        dry_run.dry_run = Some(true);
        self.pending_export = Some(request);
        match serde_json::to_value(dry_run) {
            Ok(params) => {
                self.call("artifact.export", params, PendingAction::ExportDryRun);
            }
            Err(_) => self.set_error(String::from("내보내기 요청을 만들지 못했습니다")),
        }
    }

    /// The checks a page range passes before any request: the document and the format must apply page selection, and the text
    /// must name pages that the document has now. The window's own export and the `file.export` tool both run it.
    pub(super) fn check_page_range(&self, format: &str, pages: &str) -> Result<(), String> {
        let active = self.active.as_ref().ok_or_else(|| String::from("작업물이 열려 있지 않습니다"))?;
        if !range_supported(active.kind, &format.trim().to_ascii_lowercase()) {
            return Err("이 작업물과 형식은 쪽 범위 내보내기를 지원하지 않습니다".into());
        }
        parse_page_range(pages, self.page_sizes().len()).map(|_| ()).map_err(|error| error.to_string())
    }

    pub(super) fn export_active_to(&mut self, args: &Value) -> Result<(), String> {
        let target = args
            .get("target_path")
            .and_then(Value::as_str)
            .filter(|path| !path.trim().is_empty())
            .ok_or_else(|| String::from("내보낼 target_path를 지정해 주세요"))?;
        let format = args.get("format").and_then(Value::as_str).ok_or_else(|| String::from("내보낼 format을 지정해 주세요"))?;
        if !self.export_format_supported(format) {
            return Err("현재 작업물에서 지원하지 않는 내보내기 형식입니다".into());
        }
        if let Some(losses) = args.get("allowed_losses") {
            if !losses.as_array().is_some_and(|losses| losses.iter().all(Value::is_string)) {
                return Err("allowed_losses는 진단 코드 문자열의 배열이어야 합니다".into());
            }
        }
        if args.get("overwrite").is_some_and(|overwrite| !overwrite.is_boolean()) {
            return Err("overwrite는 true 또는 false여야 합니다".into());
        }
        if args.get("expected_target_hash").is_some_and(|hash| !hash.is_string()) {
            return Err("expected_target_hash는 문자열이어야 합니다".into());
        }
        if args.get("scale").is_some_and(|scale| !matches!(scale.as_u64(), Some(1 | 2 | 3))) {
            return Err("scale은 1, 2 또는 3이어야 합니다".into());
        }
        if args.get("background").is_some_and(|background| !matches!(background.as_str(), Some("page" | "transparent"))) {
            return Err("background는 page 또는 transparent여야 합니다".into());
        }
        if args.get("scope").is_some_and(|scope| !matches!(scope.as_str(), Some("page" | "selection" | "all" | "range"))) {
            return Err("scope는 page, selection, all 또는 range이어야 합니다".into());
        }
        if args.get("scope").and_then(Value::as_str) == Some("selection") && self.selected_nodes.is_empty() {
            return Err("선택된 항목이 없어 선택 범위를 내보낼 수 없습니다".into());
        }
        // Every page of a document of several pages in one PNG or SVG is refused, as the engines refuse it; the window does not
        // write the page shown instead.
        let one_page_format = matches!(format.trim().to_ascii_lowercase().as_str(), "png" | "svg");
        if one_page_format && args.get("scope").and_then(Value::as_str) == Some("all") && self.page_sizes().len() > 1 {
            return Err(ONE_PAGE_FORMAT.into());
        }
        if args.get("pages").is_some_and(|pages| !pages.is_string()) {
            return Err("pages는 문자열이어야 합니다".into());
        }
        if args.get("scope").and_then(Value::as_str) == Some("range") {
            let pages = args.get("pages").and_then(Value::as_str).ok_or_else(|| String::from("scope가 range이면 pages(예: 1,3-5)를 지정해 주세요"))?;
            self.check_page_range(format, pages)?;
        } else if args.get("pages").is_some() {
            return Err("pages는 scope가 range일 때만 쓸 수 있습니다".into());
        }
        let request = self.export_request(target.to_owned(), Some(args)).ok_or_else(|| String::from("현재 작업물에서 지원하지 않는 내보내기 형식입니다"))?;
        let params = serde_json::to_value(request).map_err(|_| String::from("내보내기 요청을 만들지 못했습니다"))?;
        self.call("artifact.export", params, PendingAction::UiExport);
        Ok(())
    }

    /// What the export popover's settings are now: format, scope, scale, background, the page range (`pages`, when the
    /// scope is `range`) and revision.
    pub(crate) fn export_key(&self, pages: &str) -> String {
        let revision = self.active.as_ref().map_or(0, |item| item.revision);
        let pages = if self.export_scope == "range" { pages } else { "" };
        format!("{}|{}|{}|{}|{}|{}|{}", self.export_format, self.export_scope, self.export_scale, self.export_transparent, self.page_index, revision, pages)
    }

    /// The arguments of the popover's settings as its button sends them to `file.export` (without the target).
    pub(crate) fn popover_export_args(&self, pages: &str) -> Value {
        let mut args = json!({
            "format": self.export_format,
            "scale": self.export_scale,
            "scope": self.export_scope,
            "background": if self.export_transparent { "transparent" } else { "page" },
        });
        if self.export_scope == "range" {
            args["pages"] = json!(pages);
        }
        args
    }

    /// Asks the host, without writing a file, what the chosen export would lose; the popover lists it.
    pub(crate) fn preview_export_losses(&mut self, pages: &str) {
        let key = self.export_key(pages);
        if self.export_preview.as_ref().is_some_and(|(seen, _)| *seen == key) || self.export_preview_pending.as_deref() == Some(key.as_str()) {
            return;
        }
        let options = self.popover_export_args(pages);
        let Some(mut request) = self.export_request(String::new(), Some(&options)) else {
            return;
        };
        request.dry_run = Some(true);
        if let Ok(params) = serde_json::to_value(request) {
            self.export_preview_pending = Some(key.clone());
            self.call("artifact.export", params, PendingAction::ExportPreview { key });
        }
    }

    /// Builds either the popover request or an explicit one-call file.export request.
    pub(super) fn export_request(&self, target: String, options: Option<&Value>) -> Option<ExportFileRequest> {
        let active = self.active.as_ref()?;
        let explicit = options.is_some();
        let format = options
            .and_then(|options| options.get("format").and_then(Value::as_str))
            .unwrap_or(&self.export_format)
            .trim()
            .to_ascii_lowercase();
        if !self.export_format_supported(&format) { return None; }
        let required_editability = match format.as_str() {
            "svg" | "pdf" => Editability::Vector,
            // A Figure's raster image panels make its PPTX Vector at best (`FigureEngine` lowers it), so the floor is Vector.
            "pptx" if active.kind == ArtifactKind::Figure => Editability::Vector,
            "pptx" | "html" => Editability::Objects,
            "png" => Editability::None,
            // The ZIP holds the project's own files, so its floor is the native data itself.
            "zip" => Editability::NativeData,
            _ => return None,
        };
        let scope = options
            .and_then(|options| options.get("scope").and_then(Value::as_str))
            .map(str::to_owned)
            .unwrap_or_else(|| if explicit { "page".into() } else { self.export_scope.clone() });
        // A format or document that does not apply page selection makes no request: its engine would write the whole
        // document, and the chosen pages would be dropped without a word.
        if scope == "range" && !range_supported(active.kind, &format) {
            return None;
        }
        // PNG and SVG write one page; the other formats write the page, the named pages (`range`) or the whole document.
        // A WebCode project has no pages, and its engine refuses any page_indices (canvas web/mod.rs export), so none is sent.
        let selection = scope == "selection" && !self.selected_nodes.is_empty();
        let page_indices = if active.kind == ArtifactKind::WebCode {
            None
        } else if matches!(format.as_str(), "svg" | "png") || scope == "page" || selection {
            Some(vec![self.page_index])
        } else if scope == "range" {
            // Checked against the pages the document has now; a range that does not fit makes no request.
            let pages = options.and_then(|options| options.get("pages").and_then(Value::as_str))?;
            Some(parse_page_range(pages, self.page_sizes().len()).ok()?)
        } else {
            None
        };
        // `선택`: only the selected nodes, cropped to their union (`ExportRequest.node_ids`, PNG and SVG).
        let node_ids = selection.then(|| self.selected_nodes.clone());
        // A PNG at 1× has one pixel per CSS px (96 dpi).
        let profile = if format == "png" {
            let scale = options
                .and_then(|options| options.get("scale").and_then(Value::as_u64))
                .map_or(if explicit { 1 } else { self.export_scale }, |scale| scale as u32)
                .clamp(1, 3);
            let transparent = options
                .and_then(|options| options.get("background").and_then(Value::as_str))
                .map_or(if explicit { false } else { self.export_transparent }, |background| background == "transparent");
            RenderProfile { dpi: 96 * scale, transparent }
        } else {
            RenderProfile::default()
        };
        let allowed_losses = options
            .and_then(|options| options.get("allowed_losses").and_then(Value::as_array))
            .map(|losses| losses.iter().filter_map(Value::as_str).map(str::to_owned).collect())
            .unwrap_or_default();
        Some(ExportFileRequest {
            request_id: new_id(),
            dry_run: None,
            options: ExportRequest {
                allowed_losses,
                profile,
                page_indices,
                node_ids,
                ..ExportRequest::new(active.id.clone(), active.revision, format, required_editability)
            },
            target_path: target,
            overwrite: options.and_then(|options| options.get("overwrite").and_then(Value::as_bool)).unwrap_or(false),
            expected_target_hash: options.and_then(|options| options.get("expected_target_hash").and_then(Value::as_str)).map(str::to_owned),
        })
    }

    pub(super) fn apply_export_losses(&mut self) {
        let Some(mut confirmation) = self.export_confirmation.take() else {
            return;
        };
        if confirmation.allowed.is_empty() {
            self.export_confirmation = Some(confirmation);
            return;
        }
        confirmation.request.request_id = new_id();
        confirmation.request.options.allowed_losses =
            confirmation.allowed.into_iter().collect();
        self.send_export(confirmation.request);
    }

    /// Sends the write for a request the user accepted: after a dry run with no losses, or after the loss choice.
    pub(super) fn send_export(&mut self, request: ExportFileRequest) {
        // The pages were checked when the request was built; the document may have lost pages since (a dry run, a loss choice).
        // The words go by the request, not by the scope the menu shows now: only a page range names more than one page.
        if let Some(pages) = request.options.page_indices.as_ref().filter(|pages| pages.iter().any(|&page| page as usize >= self.page_sizes().len())) {
            let message = if pages.len() > 1 {
                "문서 쪽 수가 바뀌어 쪽 범위를 쓸 수 없습니다. 쪽 번호를 다시 적어 주세요"
            } else {
                "내보낼 쪽이 없어졌습니다. 다시 내보내 주세요"
            };
            self.set_error(message.into());
            return;
        }
        match serde_json::to_value(&request) {
            Ok(params) => {
                self.pending_export = Some(request);
                self.call("artifact.export", params, PendingAction::Export);
            }
            Err(_) => self.set_error("내보내기 요청을 만들지 못했습니다".into()),
        }
    }

    pub(super) fn draw_export_loss_dialog(&mut self, ctx: &Context) {
        let Some(confirmation) = self.export_confirmation.as_mut() else {
            return;
        };
        let mut confirm = false;
        let mut cancel = false;
        egui::Window::new("내보내기 확인")
            .collapsible(false)
            .resizable(false)
            .show(ctx, |ui| {
                ui.label("일부 편집 정보가 출력물에 포함되지 않습니다.");
                if confirmation.diagnostics.is_empty() {
                    ui.label("허용할 손실 항목을 확인할 수 없습니다.");
                }
                for diagnostic in &confirmation.diagnostics {
                    let mut checked = confirmation.allowed.contains(&diagnostic.code);
                    if ui.checkbox(&mut checked, &diagnostic.message).changed() {
                        if checked {
                            confirmation.allowed.insert(diagnostic.code.clone());
                        } else {
                            confirmation.allowed.remove(&diagnostic.code);
                        }
                    }
                }
                ui.horizontal(|ui| {
                    if ui.button("취소").clicked() {
                        cancel = true;
                    }
                    let accept = ui.add_enabled(!confirmation.allowed.is_empty(), egui::Button::new("이 손실을 허용하고 내보내기"));
                    crate::ui_record::note_human_only(ui, "이 손실을 허용하고 내보내기", &accept);
                    if accept.clicked() {
                        confirm = true;
                    }
                });
            });
        if cancel {
            self.export_confirmation = None;
        } else if confirm {
            self.apply_export_losses();
        }
    }
}

/// The kind a file of this tool names in its `format` (`designllm.canvas`, `designllm.web`, `designllm.figure-project`: the
/// native writers of canvas and figures). Office files, links and anything that is not a JSON document with a known format are
/// left to the core (None).
fn native_kind(path: &std::path::Path) -> Option<ArtifactKind> {
    #[derive(serde::Deserialize)]
    struct Head {
        format: Option<String>,
    }
    let extension = path.extension().map(|extension| extension.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
    if matches!(extension.as_str(), "pptx" | "docx" | "hwpx" | "hwp") {
        return None;
    }
    if !std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_file()) {
        return None;
    }
    let file = std::fs::File::open(path).ok()?;
    let head: Head = serde_json::from_reader(std::io::BufReader::new(file)).ok()?;
    match head.format.as_deref()? {
        "designllm.canvas" => Some(ArtifactKind::Canvas),
        "designllm.web" => Some(ArtifactKind::WebCode),
        "designllm.figure-project" => Some(ArtifactKind::Figure),
        _ => None,
    }
}
