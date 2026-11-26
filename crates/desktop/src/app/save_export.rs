use super::{messages::engine_warning_message, print_range::{parse_page_range, range_supported, validate_page_range, UNKNOWN_PAGE_COUNT}, state::SaveRpcOutcome, *};

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

    /// Uses the project's managed copy, or asks for a file path in a standalone workspace.
    pub(super) fn save_active(&mut self) {
        let Some(active) = self.active.as_ref() else {
            self.set_error(String::from("작업물이 열려 있지 않습니다"));
            return;
        };
        if !self.feature_write_supported(Feature::NativeRoundtrip) {
            self.set_error(String::from("이 작업물의 원본 형식 저장은 지원되지 않습니다"));
            return;
        }
        let managed = self.home.project_folder.is_some();
        if !managed && self.save_path.trim().is_empty() {
            let Some(path) = crate::dialogs::save_document(self.parent_hwnd, active.title.clone(), active.kind) else {
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
            target_path: if managed { String::new() } else { self.save_path.trim().into() },
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

    /// The DPI the export popover shows and sends: its own pick while it is open (`export_dpi_draft`), else the view's.
    pub(crate) fn export_dpi(&self) -> u32 {
        self.export_dpi_draft.unwrap_or_else(|| self.view_ref().figure_export_dpi)
    }

    #[cfg(test)]
    pub(super) fn export_active(&mut self) {
        if let Err(error) = self.export_active_with(None) {
            self.set_error(error);
        }
    }

    /// Sends the export that the popover's settings (`options` None) or the arguments of the save dialog describe: a dry
    /// run for its losses first, then the write.
    pub(super) fn export_active_with(&mut self, options: Option<&Value>) -> Result<(), String> {
        if self.export_path.trim().is_empty() {
            return Err("내보낼 위치를 고르세요".into());
        }
        let pages = options.and_then(|options| options.get("pages")).and_then(Value::as_str).unwrap_or("");
        let Some(mut request) = self.export_request(self.export_path.trim().to_owned(), options) else {
            return Err(EXPORT_NOT_BUILT.into());
        };
        // The losses the popover showed for these settings are the ones the user accepted with its button: the key names the
        // document and the selection too, so a warning read for another document never permits this one's losses.
        if let Some((key, losses)) = &self.export_preview {
            if *key == self.export_key(pages) {
                request.options.allowed_losses = losses.iter().map(|loss| loss.code.clone()).collect::<BTreeSet<_>>().into_iter().collect();
            }
        }
        let mut dry_run = request.clone();
        dry_run.request_id = new_id();
        dry_run.dry_run = Some(true);
        match serde_json::to_value(dry_run) {
            Ok(params) => {
                let artifact_id = request.options.artifact_id.clone();
                match self.call_for(&artifact_id, "artifact.export", params, PendingAction::ExportDryRun) {
                    Some(ticket) => {
                        self.pending_exports.insert(ticket, request);
                        Ok(())
                    }
                    None => Err("내보내기 요청을 보내지 못했습니다".into()),
                }
            }
            Err(_) => Err("내보내기 요청을 만들지 못했습니다".into()),
        }
    }

    /// The checks a page range passes before any request: the document and the format must apply page selection, and the text
    /// must name pages that the document has now. The window's own export and the `file.export` tool both run it.
    pub(super) fn check_page_range(&self, format: &str, pages: &str) -> Result<(), String> {
        let active = self.active.as_ref().ok_or_else(|| String::from("작업물이 열려 있지 않습니다"))?;
        if !range_supported(active.kind, &format.trim().to_ascii_lowercase()) {
            return Err("이 작업물과 형식은 쪽 범위 내보내기를 지원하지 않습니다".into());
        }
        validate_page_range(pages, self.range_check_count()).map(|_| ()).map_err(|error| error.to_string())
    }

    /// The title row's `이 페이지 내보내기` / `이 슬라이드 내보내기`: the export menu on this page. One slide is not a PPTX
    /// (that writes the deck), so a deck's menu opens on PDF for it; the deck's PPTX stays in the menu's formats.
    pub(super) fn open_page_export(&mut self) {
        let kind = self.active.as_ref().map_or(ArtifactKind::Canvas, |item| item.kind);
        // A deck's slide is always PDF here, whatever was chosen before (a Canvas's HTML, the deck's PPTX): the menu would
        // otherwise put the deck's first format in its place and widen the page to every slide.
        if kind == ArtifactKind::Slides || !self.export_format_supported(&self.export_format) {
            self.export_format = String::from("pdf");
        }
        self.export_scope = String::from("page");
        self.smoke_open = Some(String::from("export"));
    }

    /// Whether the shown kind's export writes the selected nodes only (`EngineAdapter::exports_nodes`): the Canvas engine's
    /// PNG and SVG. A Figure panel or a slide object exports its whole Figure or slide, so it offers no selection export.
    pub(super) fn selection_export_supported(&self) -> bool {
        self.active.as_ref().is_some_and(|active| active.kind == ArtifactKind::Canvas)
    }

    /// Whether the shown document has more than one page, as far as the window knows it: the count read for the shown
    /// revision (`shown_page_count`), never the board's fallback page; while that count is unknown, every kind that can
    /// have several pages is taken to have them (its engine checks).
    pub(super) fn shown_document_multipage(&self) -> bool {
        let kind = self.active.as_ref().map_or(ArtifactKind::Canvas, |item| item.kind);
        match self.shown_page_count() {
            Some(count) => count > 1,
            None => !matches!(kind, ArtifactKind::WebCode | ArtifactKind::Figure),
        }
    }

    /// Ctrl+E: the export menu, on the settings it had (the editor's top bar opens it).
    pub(super) fn open_export_menu(&mut self) {
        self.smoke_open = Some(String::from("export"));
    }

    /// The properties panel's `내보내기 ›` on a selected Canvas object: the menu on the selection, in PNG unless SVG is
    /// chosen (only those write some nodes of a page).
    pub(super) fn open_selection_export(&mut self) {
        if !matches!(self.export_format.as_str(), "png" | "svg") {
            self.export_format = String::from("png");
        }
        self.export_scope = String::from("selection");
        self.smoke_open = Some(String::from("export"));
    }

    /// A deck's PPTX writes every slide: a request for one slide, the selection or a page range is refused, not widened.
    pub(super) fn deck_scope_refusal(&self, format: &str, scope: Option<&str>) -> Option<&'static str> {
        let kind = self.active.as_ref()?.kind;
        (whole_deck(kind, format.trim()) && scope.is_some_and(|scope| scope != "all")).then_some(DECK_PPTX_SCOPE)
    }

    /// The page count the window holds for the shown document at its shown revision (`page_count_at`); None until a render of
    /// that revision reported it.
    pub(super) fn shown_page_count(&self) -> Option<usize> {
        let active = self.active.as_ref()?;
        self.page_count_at(&active.id, active.revision)
    }

    /// The count a page range is checked against before it is sent: the count of the shown revision, or, while it is not known,
    /// none (the engine checks the range at the revision the request names).
    pub(super) fn range_check_count(&self) -> usize {
        self.shown_page_count().unwrap_or(UNKNOWN_PAGE_COUNT)
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
        if args.get("dpi").is_some_and(|dpi| !dpi.as_u64().is_some_and(|dpi| (72..=1200).contains(&dpi))) {
            return Err("dpi는 72부터 1200까지의 정수여야 합니다".into());
        }
        if args.get("background").is_some_and(|background| !matches!(background.as_str(), Some("page" | "transparent"))) {
            return Err("background는 page 또는 transparent여야 합니다".into());
        }
        if args.get("scope").is_some_and(|scope| !matches!(scope.as_str(), Some("page" | "selection" | "all" | "range"))) {
            return Err("scope는 page, selection, all 또는 range이어야 합니다".into());
        }
        if let Some(error) = self.deck_scope_refusal(format, args.get("scope").and_then(Value::as_str)) {
            return Err(error.into());
        }
        if args.get("scope").and_then(Value::as_str) == Some("selection") && self.selected_nodes.is_empty() {
            return Err("선택된 항목이 없어 선택 범위를 내보낼 수 없습니다".into());
        }
        // Every page of a document of several pages in one PNG or SVG is refused, as the engines refuse it; the window does not
        // write the page shown instead.
        let one_page_format = matches!(format.trim().to_ascii_lowercase().as_str(), "png" | "svg");
        if one_page_format && args.get("scope").and_then(Value::as_str) == Some("all") && self.shown_page_count().is_some_and(|count| count > 1) {
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
        let artifact_id = request.options.artifact_id.clone();
        let params = serde_json::to_value(request).map_err(|_| String::from("내보내기 요청을 만들지 못했습니다"))?;
        self.call_for(&artifact_id, "artifact.export", params, PendingAction::UiExport);
        Ok(())
    }

    /// What the export popover's settings are now: the document and its revision, format, scope, scale, background, the page
    /// range (`pages`, when the scope is `range`), the selected nodes (when the scope is `selection`) and the DPI. A loss
    /// warning read under one key is shown and accepted only under the same key.
    pub(crate) fn export_key(&self, pages: &str) -> String {
        let artifact = self.active.as_ref().map_or("", |item| item.id.as_str());
        let revision = self.active.as_ref().map_or(0, |item| item.revision);
        let pages = if self.export_scope == "range" { pages } else { "" };
        let selection = if self.export_scope == "selection" { self.selected_nodes.join(",") } else { String::new() };
        let dpi = if self.export_format == "png" && self.active.as_ref().is_some_and(|item| item.kind == ArtifactKind::Figure) { self.export_dpi() } else { 0 };
        format!(
            "{artifact}|{}|{}|{}|{}|{}|{revision}|{pages}|{selection}|{dpi}",
            self.export_format, self.export_scope, self.export_scale, self.export_transparent, self.view_ref().page_index,
        )
    }

    /// The arguments of the popover's settings as its button sends them to `file.export` (without the target).
    pub(crate) fn popover_export_args(&self, pages: &str) -> Value {
        let mut args = json!({
            "format": self.export_format,
            "scale": self.export_scale,
            "scope": self.export_scope,
            "background": if self.export_transparent { "transparent" } else { "page" },
        });
        if self.export_format == "png" && self.active.as_ref().is_some_and(|item| item.kind == ArtifactKind::Figure) {
            args["dpi"] = json!(self.export_dpi());
        }
        if self.export_scope == "range" {
            args["pages"] = json!(pages);
        }
        args
    }

    /// Asks the host, without writing a file, what the chosen export would lose; the popover lists it.
    pub(crate) fn preview_export_losses(&mut self, pages: &str) {
        let key = self.export_key(pages);
        // A failure is kept for its own settings only: other settings are asked at once.
        if self.export_preview_failed.as_ref().is_some_and(|(failed, _)| *failed != key) {
            self.export_preview_failed = None;
        }
        let held_back = self.export_preview_failed.as_ref().is_some_and(|(_, retry)| retry.is_none_or(|at| Instant::now() < at));
        if self.export_preview.as_ref().is_some_and(|(seen, _)| *seen == key)
            || self.export_preview_pending.as_deref() == Some(key.as_str())
            || held_back
        {
            return;
        }
        let options = self.popover_export_args(pages);
        let Some(mut request) = self.export_request(String::new(), Some(&options)) else {
            return;
        };
        request.dry_run = Some(true);
        if let Ok(params) = serde_json::to_value(request) {
            // Nothing is on its way when no ticket comes back (the host is gone): the preview is asked again later.
            if self.call("artifact.export", params, PendingAction::ExportPreview { key: key.clone() }).is_some() {
                self.export_preview_pending = Some(key);
            }
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
            .unwrap_or_else(|| if whole_deck(active.kind, &format) { "all".into() } else if explicit { "page".into() } else { self.export_scope.clone() });
        // A deck's PPTX is every slide: one slide, the selection or a range makes no request (`deck_scope_refusal` says why).
        if whole_deck(active.kind, &format) && scope != "all" {
            return None;
        }
        // A format or document that does not apply page selection makes no request: its engine would write the whole
        // document, and the chosen pages would be dropped without a word.
        if scope == "range" && !range_supported(active.kind, &format) {
            return None;
        }
        // PNG and SVG write one page; the other formats write the page, the named pages (`range`) or the whole document.
        // A WebCode project has no pages, and its engine refuses any page_indices (canvas web/mod.rs export), so none is sent.
        let selection = scope == "selection" && !self.selected_nodes.is_empty();
        let page_indices = if active.kind == ArtifactKind::WebCode || whole_deck(active.kind, &format) {
            None
        } else if matches!(format.as_str(), "svg" | "png") || scope == "page" || selection {
            Some(vec![self.view_ref().page_index])
        } else if scope == "range" {
            // Checked against the pages the document has now; a range that does not fit makes no request.
            let pages = options.and_then(|options| options.get("pages").and_then(Value::as_str))?;
            Some(parse_page_range(pages, self.range_check_count()).ok()?)
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
            let dpi = options.and_then(|options| options.get("dpi")).and_then(Value::as_u64).map(|dpi| dpi as u32)
                .unwrap_or_else(|| if active.kind == ArtifactKind::Figure && !options.is_some_and(|options| options.get("scale").is_some()) { self.view_ref().figure_export_dpi } else { 96 * scale });
            RenderProfile { dpi, transparent }
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

    /// The page count of `artifact_id` at `revision`, when the window holds that revision: the pages of a canvas or slides
    /// body read at it, or for another kind the count a render of that revision reported. None when the window shows another
    /// document or another revision, or no render of that revision reported a count.
    pub(super) fn page_count_at(&self, artifact_id: &str, revision: designllm_contracts::Revision) -> Option<usize> {
        if self.active.as_ref().is_none_or(|active| active.id != artifact_id) {
            return None;
        }
        if let Some(artifact) = self.artifact.as_ref().filter(|artifact| artifact.id == artifact_id) {
            let pages = match &artifact.body {
                designllm_contracts::ArtifactBody::Canvas(document) => Some(document.pages.len()),
                designllm_contracts::ArtifactBody::Slides(document) => Some(document.pages.len()),
                _ => None,
            };
            if let Some(pages) = pages {
                return (artifact.revision == revision).then_some(pages);
            }
        }
        // The count goes with the render that reported it: a later revision's count is not the one of this request.
        self.page_count_read
            .as_ref()
            .filter(|(id, read_revision, _)| id == artifact_id && *read_revision == revision)
            .map(|(_, _, total)| *total as usize)
    }

    /// Asks about an export's losses: at once when no other export is being asked about, else after the ones before it.
    pub(super) fn ask_export_losses(&mut self, confirmation: ExportConfirmation) {
        if self.export_confirmation.is_none() {
            self.export_confirmation = Some(confirmation);
        } else {
            self.export_confirmations_waiting.push_back(confirmation);
        }
    }

    /// The loss question on screen is answered: the next waiting export is asked about.
    fn next_export_confirmation(&mut self) {
        self.export_confirmation = self.export_confirmations_waiting.pop_front();
    }

    /// Loss questions the user has not answered yet: the one on screen and the ones waiting behind it.
    pub(super) fn export_confirmations_open(&self) -> bool {
        self.export_confirmation.is_some() || !self.export_confirmations_waiting.is_empty()
    }

    /// How many exports wait for their loss answer (the one on screen and those behind it).
    pub(super) fn export_confirmation_count(&self) -> usize {
        usize::from(self.export_confirmation.is_some()) + self.export_confirmations_waiting.len()
    }

    /// The close question's button that drops them, with their number when there are several.
    pub(super) fn cancel_exports_label(&self) -> String {
        match self.export_confirmation_count() {
            0 | 1 => "내보내기 취소하고 닫기".into(),
            count => format!("내보내기 {count}개 취소하고 닫기"),
        }
    }

    /// `내보내기 취소하고 닫기` of the close question: every unanswered export is dropped (nothing was written for them).
    pub(super) fn cancel_export_confirmations(&mut self) {
        self.export_confirmation = None;
        self.export_confirmations_waiting.clear();
    }

    /// `취소` of the loss question on screen: that export is not written, and the next waiting one is asked about.
    pub(super) fn cancel_export_confirmation(&mut self) {
        self.export_confirmation = None;
        self.next_export_confirmation();
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
        // The losses approved now join those approved before for this export: a retry that asks about other losses keeps them.
        let mut allowed: BTreeSet<String> = confirmation.request.options.allowed_losses.iter().cloned().collect();
        allowed.extend(confirmation.allowed);
        confirmation.request.options.allowed_losses = allowed.into_iter().collect();
        self.send_export(confirmation.request);
        self.next_export_confirmation();
    }

    /// Sends the write for a request the user accepted: after a dry run with no losses, or after the loss choice.
    pub(super) fn send_export(&mut self, request: ExportFileRequest) {
        // The pages were checked when the request was built; the document may have lost pages since (a dry run, a loss choice).
        // The words go by the request, not by the scope the menu shows now: only a page range names more than one page. The
        // window knows the page count of the document it shows; another document's export is left to its engine to check.
        // The page count is known only when the document is loaded at the request's revision; otherwise (another document,
        // or this one not read yet at that revision) its engine checks the pages at the revision the request names.
        let count = self.page_count_at(&request.options.artifact_id, request.options.expected_revision);
        if let Some(pages) = request.options.page_indices.as_ref().filter(|pages| count.is_some_and(|count| pages.iter().any(|&page| page as usize >= count))) {
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
                // Recorded under the export's own document, which may no longer be the shown one.
                let artifact_id = request.options.artifact_id.clone();
                if let Some(ticket) = self.call_for(&artifact_id, "artifact.export", params, PendingAction::Export) {
                    self.pending_exports.insert(ticket, request);
                }
            }
            Err(_) => self.set_error("내보내기 요청을 만들지 못했습니다".into()),
        }
    }

    pub(super) fn draw_export_loss_dialog(&mut self, ctx: &Context) {
        let Some(confirmation) = self.export_confirmation.as_mut() else {
            return;
        };
        use crate::widgets::{DialogButton, DialogChoice};
        let palette = crate::theme::palette(ctx);
        let shown = crate::widgets::dialog_with_choices(ctx, &palette, egui::Id::new("내보내기 확인"), "내보내기 확인", crate::widgets::DialogPlace::Screen, |ui| {
                ui.label(crate::i18n::tr("일부 편집 정보가 출력물에 포함되지 않습니다."));
                // Which export this is, when several wait for their answer: its whole path (two folders may hold the same name).
                let path = &confirmation.request.target_path;
                ui.label(format!("{}: {}", crate::i18n::tr("저장 위치"), export_destination_label(path)))
                    .on_hover_text(path.as_str());
                // A place a UI-test run's save answer named (68r): the person accepts the losses of a write the run placed.
                if crate::dialogs::is_test_save_answer(&confirmation.request.target_path) {
                    ui.label(crate::i18n::tr(&format!("시험 실행이 정한 위치: {}", confirmation.request.target_path)));
                }
                if confirmation.diagnostics.is_empty() {
                    ui.label(crate::i18n::tr("허용할 손실 항목을 확인할 수 없습니다."));
                }
                for diagnostic in &confirmation.diagnostics {
                    let mut checked = confirmation.allowed.contains(&diagnostic.code);
                    if crate::widgets::checkbox(ui, &palette, &mut checked, &diagnostic.message).changed() {
                        if checked {
                            confirmation.allowed.insert(diagnostic.code.clone());
                        } else {
                            confirmation.allowed.remove(&diagnostic.code);
                        }
                    }
                }
                ((), vec![
                    DialogChoice::new("취소", DialogButton::Plain),
                    DialogChoice::new("이 손실을 허용하고 내보내기", DialogButton::Warn).enabled(!confirmation.allowed.is_empty()).human_only(),
                ])
            });
        // The main button accepts a loss, so Enter does not press it. Esc is `취소`.
        let (cancel, confirm) = (shown.inner.1[0], shown.inner.1[1]);
        if cancel || shown.dismissed {
            self.cancel_export_confirmation();
        } else if confirm {
            self.apply_export_losses();
        }
    }
}

/// The longest path the loss question shows whole; a longer one keeps its start and its end (the folder and the file).
const DESTINATION_CHARS: usize = 72;

/// The place an export writes, as its loss question names it: the path verbatim, its middle left out when it is long.
pub(super) fn export_destination_label(target_path: &str) -> String {
    let chars: Vec<char> = target_path.chars().collect();
    if chars.len() <= DESTINATION_CHARS {
        return target_path.to_owned();
    }
    let tail = DESTINATION_CHARS * 2 / 3;
    let head = DESTINATION_CHARS - tail - 1;
    let start: String = chars[..head].iter().collect();
    let end: String = chars[chars.len() - tail..].iter().collect();
    format!("{start}…{end}")
}

/// A request the window could not build from the chosen settings: nothing is sent, and the caller is told.
pub(super) const EXPORT_NOT_BUILT: &str = "이 설정으로는 내보낼 수 없습니다. 형식과 범위를 확인해 주세요";

/// Why a deck's PPTX refuses one slide, the selection or a page range.
pub(super) const DECK_PPTX_SCOPE: &str = "PPTX는 모든 슬라이드를 내보냅니다. scope \"all\"을 쓰거나, 슬라이드 하나는 PDF 또는 PNG로 내보내세요";

/// How long a preview that failed in passing (the host busy, a lost reply) waits before it is asked again.
pub(super) const PREVIEW_RETRY: std::time::Duration = std::time::Duration::from_secs(2);

/// A refusal that the same settings would meet again: the request or the engine says no, not the link or the moment.
pub(super) fn preview_refusal_is_permanent(code: &designllm_contracts::ErrorCode) -> bool {
    use designllm_contracts::ErrorCode::*;
    matches!(code, InvalidArgument | NotFound | UnsupportedOperation | SchemaUnsupported | DataAmbiguous | UnsafeRoundtrip | IntegrityError | ExportLossRequired)
}

/// A deck's PPTX is the whole deck: its engine writes no single slide and refuses page indices for PPTX.
pub(super) fn whole_deck(kind: ArtifactKind, format: &str) -> bool {
    kind == ArtifactKind::Slides && format.eq_ignore_ascii_case("pptx")
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
    if matches!(extension.as_str(), "pptx" | "docx" | "hwpx" | "hwp" | "pdf") {
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
