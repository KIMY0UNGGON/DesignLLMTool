use super::*;
use super::{doc_tree::paragraph_text, state::{ApplyContext, HeldEditState}};
use std::sync::atomic::{AtomicU8, Ordering};

/// `DESIGNLLM_SMOKE_PDF=<absolute path>`: the smoke run also imports page 1 of this PDF into a new figure through the window's
/// own "근거 추가" path (`import_pdf_evidence`), then writes `<output>.pdf.json` with the message the window shows, the `PDF_*`
/// codes of that message and of every earlier reader diagnostic the window kept (a later error may have replaced the message),
/// the figure's evidence count and the host's `system.hello`. A refused PDF is the expected result here, so it does not fail
/// the smoke run.
const PDF_IDLE: u8 = 0;
const PDF_CREATE_SENT: u8 = 1;
const PDF_IMPORT_SENT: u8 = 2;
const PDF_DONE: u8 = 3;
static PDF_STEP: AtomicU8 = AtomicU8::new(PDF_IDLE);
/// The most `PDF_*` codes a smoke report keeps.
const SMOKE_PDF_CODE_LIMIT: usize = 32;

/// The PDF of the smoke run, when `DESIGNLLM_SMOKE_PDF` names one.
fn smoke_pdf_path() -> Option<String> {
    std::env::var("DESIGNLLM_SMOKE_PDF").ok().filter(|path| !path.trim().is_empty())
}

fn is_valid_pdf_code(token: &str) -> bool {
    token.strip_prefix("PDF_").is_some_and(|suffix| {
        !suffix.is_empty() && suffix.bytes().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == b'_')
            && !suffix.starts_with('_') && !suffix.ends_with('_') && !suffix.contains("__")
    })
}

/// The complete PDF codes in a message, in order, each once.
#[cfg(test)]
fn pdf_codes(message: &str) -> Vec<String> {
    let mut codes: Vec<String> = Vec::new();
    for token in message.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_')) {
        if is_valid_pdf_code(token) && codes.len() < SMOKE_PDF_CODE_LIMIT && !codes.iter().any(|code| code == token)
        {
            codes.push(token.to_owned());
        }
    }
    codes
}

/// The smoke failure of a refused call: the method, the window's sentence, and the diagnostic codes of the error. The window's
/// sentence names no code for a request-level refusal, so the codes are kept here for the PDF report.
pub(super) fn smoke_failure_text(method: &str, friendly: &str, error: &DltError) -> String {
    if error.diagnostics.is_empty() {
        return format!("{method}: {friendly}");
    }
    let codes: Vec<&str> = error.diagnostics.iter().map(|diagnostic| diagnostic.code.as_str()).collect();
    format!("{method}: {friendly} [{}]", codes.join(", "))
}

/// Whether a smoke failure (`smoke_failure_text`) is a refused PDF read or import. In a PDF run that refusal is the expected
/// result: the report keeps it and the run does not fail on it.
fn pdf_refusal(failure: &str) -> bool {
    failure.starts_with("source.extract_pdf: ") || failure.starts_with("asset.import: ")
}

#[cfg(test)]
mod pdf_smoke_tests {
    use super::{pdf_codes, pdf_refusal, smoke_failure_text};
    use designllm_contracts::{DltError, ErrorCode};

    #[test]
    fn codes_are_read_from_the_window_message_once_each() {
        assert_eq!(pdf_codes("1쪽을 안전하게 읽지 못해 PDF 근거를 넣지 않았습니다. 진단: PDF_MEMORY_LIMIT: 메모리; PDF_MEMORY_LIMIT: 다시"), vec!["PDF_MEMORY_LIMIT"]);
        assert_eq!(pdf_codes("PDF_STACK_LIMIT: a. PDF_REQUEST_BUDGET: b"), vec!["PDF_STACK_LIMIT", "PDF_REQUEST_BUDGET"]);
        assert_eq!(pdf_codes("PDF_TYPE1_ENCODING_UNREADABLE: 글꼴 진단"), vec!["PDF_TYPE1_ENCODING_UNREADABLE"]);
        assert!(pdf_codes("PDF 근거를 넣지 않았습니다").is_empty());
        assert!(pdf_codes("PDF__ abcPDF_FAKE XPDF_FAKE PDF_FOO_bar PDF_FOO_ PDF_FOO__BAR").is_empty());
        assert_eq!(pdf_codes("(PDF_TYPE1_ENCODING_UNREADABLE)"), vec!["PDF_TYPE1_ENCODING_UNREADABLE"]);
    }

    #[test]
    fn structured_codes_are_valid_unique_and_bounded() {
        let mut harness = super::super::test_support::Harness::new();
        harness.app.smoke_output = Some(std::env::temp_dir().join("smoke-codes.png"));
        let mut codes = vec!["PDF_OK".into(), "".into(), "PDF_".into(), "PDF_bad".into(), "PDF_A__B".into(), "PDF_OK".into(), "NOT_PDF_CODE".into()];
        codes.extend((0..40).map(|n| format!("PDF_CODE_{n}")));
        harness.app.smoke_note_pdf_code_list(&codes);
        assert_eq!(harness.app.smoke_pdf_codes.len(), 32);
        assert_eq!(harness.app.smoke_pdf_codes[0], "PDF_OK");
        assert!(harness.app.smoke_pdf_codes.iter().all(|code| super::is_valid_pdf_code(code)));
    }

    #[test]
    fn the_refusal_text_keeps_the_reader_codes_for_the_report() {
        let mut error = DltError::new(ErrorCode::UnsupportedOperation, "the PDF could not be read; the reader's detail is withheld");
        error.diagnostics.push(
            serde_json::from_value(serde_json::json!({ "code": "PDF_REQUEST_BUDGET", "message": "m", "node_id": null, "blocking": true, "params": {} }))
                .expect("a diagnostic"),
        );
        let text = smoke_failure_text("source.extract_pdf", "이 문서에서는 이 작업을 지원하지 않습니다", &error);
        assert_eq!(pdf_codes(&text), vec!["PDF_REQUEST_BUDGET"]);
        assert!(pdf_refusal(&text));
        assert!(!pdf_refusal(&smoke_failure_text("artifact.create", "작업을 완료하지 못했습니다", &DltError::new(ErrorCode::Internal, "x"))));
    }
}

impl DesktopApp {
    /// Whether the PDF step of the smoke run has not finished yet (the capture waits for it).
    pub(super) fn smoke_pdf_waiting(&self) -> bool {
        smoke_pdf_path().is_some() && PDF_STEP.load(Ordering::SeqCst) != PDF_DONE
    }

    /// One step of the PDF smoke run per frame: create a figure, import the PDF through `import_pdf_evidence`, then write the
    /// report once the window has no request in flight.
    pub(super) fn smoke_pdf_step(&mut self, path: &str) {
        match PDF_STEP.load(Ordering::SeqCst) {
            PDF_IDLE => {
                if self.workspace_loaded && self.pending.is_empty() {
                    let params = json!({ "request_id": new_id(), "actor": "desktop", "kind": "figure", "title": "PDF 스모크", "size": null, "template": null });
                    self.call("artifact.create", params, PendingAction::Create);
                    PDF_STEP.store(PDF_CREATE_SENT, Ordering::SeqCst);
                }
            }
            PDF_CREATE_SENT => {
                let shown = self.artifact.as_ref().is_some_and(|artifact| artifact.kind == ArtifactKind::Figure);
                if shown && self.pending.is_empty() {
                    self.pdf_path = path.to_owned();
                    self.pdf_page = "1".into();
                    self.import_pdf_evidence();
                    PDF_STEP.store(PDF_IMPORT_SENT, Ordering::SeqCst);
                }
            }
            PDF_IMPORT_SENT => {
                // asset.import and source.extract_pdf are sent from one reply handler, so no frame sees the gap between them.
                if self.pending.is_empty() {
                    self.write_pdf_report(path);
                    PDF_STEP.store(PDF_DONE, Ordering::SeqCst);
                }
            }
            _ => {}
        }
    }

    /// Keeps the diagnostic codes of a refused PDF request for the report.
    pub(super) fn smoke_note_pdf_refusal(&mut self, action: &PendingAction, _method: &str, _friendly: &str, error: &DltError) {
        if matches!(action, PendingAction::PdfAssetImport { .. } | PendingAction::PdfEvidenceImport) {
            let codes: Vec<String> = error.diagnostics.iter().map(|diagnostic| diagnostic.code.clone()).collect();
            self.smoke_note_pdf_code_list(&codes);
        }
    }

    /// Keeps structured diagnostic codes in order, once each, for the smoke report.
    pub(super) fn smoke_note_pdf_code_list(&mut self, codes: &[String]) {
        if self.smoke_output.is_none() {
            return;
        }
        for code in codes {
            if self.smoke_pdf_codes.len() >= SMOKE_PDF_CODE_LIMIT {
                break;
            }
            if is_valid_pdf_code(code) && !self.smoke_pdf_codes.contains(code) {
                self.smoke_pdf_codes.push(code.clone());
            }
        }
    }

    /// Writes the PDF result, structured diagnostic codes, evidence count, and host connection result.
    pub(super) fn write_pdf_report(&mut self, path: &str) {
        let Some(output) = self.smoke_output.clone() else {
            return;
        };
        let message = self
            .notices
            .current(Instant::now())
            .filter(|notice| notice.severity == Severity::Error)
            .map(|notice| notice.text.clone());
        // The first refused call (the reader's codes) comes from `smoke_failure`; the window's sentence may be a later error's.
        let first_refusal = self.smoke_failure.clone();
        let codes = &self.smoke_pdf_codes;
        let evidence = match self.artifact.as_ref().map(|artifact| &artifact.body) {
            Some(ArtifactBody::Figure(figure)) => figure.evidence.len(),
            _ => 0,
        };
        let hello = self
            .settings_path
            .as_deref()
            .and_then(|settings| settings.parent().and_then(|gui| gui.parent()))
            .ok_or_else(|| String::from("the workspace is not known"))
            .and_then(|root| {
                designllm_bridge::Client::with_role(root, designllm_bridge::ClientRole::Cli).hello().map_err(|error| error.message.clone())
            });
        let host_hello = match hello {
            Ok(value) => json!({ "ok": true, "host": value.get("host").cloned().unwrap_or(Value::Null) }),
            Err(error) => json!({ "ok": false, "error": error }),
        };
        let report = json!({ "pdf": path, "message": message, "first_refusal": first_refusal, "codes": codes, "figure_evidence": evidence, "host_hello": host_hello });
        let target = output.with_extension("pdf.json");
        let written = serde_json::to_string_pretty(&report).map_err(|error| error.to_string()).and_then(|text| {
            std::fs::write(&target, text).map_err(|error| error.to_string())
        });
        if let Err(error) = written {
            self.smoke_failure = Some(format!("PDF 결과를 쓰지 못했습니다: {error}"));
        }
    }

    /// Smoke captures only. `DESIGNLLM_SMOKE_SELECT=<n>` selects the first `n` top-level nodes of the first page, and
    /// `DESIGNLLM_SMOKE_MARK=1` puts a job's mark on the first node with the refusal callout and its stop action.
    pub(super) fn apply_smoke_scene(&mut self) {
        let select = std::env::var("DESIGNLLM_SMOKE_SELECT").ok().and_then(|value| value.parse::<usize>().ok());
        let mark = std::env::var_os("DESIGNLLM_SMOKE_MARK").is_some();
        // `DESIGNLLM_SMOKE_PAGE=<n>` takes the nodes of page `n` (0-based) and shows that page as the current one.
        let page = std::env::var("DESIGNLLM_SMOKE_PAGE").ok().and_then(|value| value.parse::<usize>().ok()).unwrap_or(0);
        self.smoke_open = std::env::var("DESIGNLLM_SMOKE_OPEN").ok();
        // `DESIGNLLM_SMOKE_SELECT_IDS=a,b,c` selects those node ids (any depth, e.g. siblings of one stack).
        if let Ok(list) = std::env::var("DESIGNLLM_SMOKE_SELECT_IDS") {
            let ids: Vec<String> = list.split(',').map(str::trim).filter(|id| !id.is_empty()).map(str::to_owned).collect();
            if !ids.is_empty() {
                self.select_ids(ids);
            }
        }
        if select.is_none() && !mark {
            return;
        }
        let nodes: Vec<String> = match self.artifact.as_ref().map(|artifact| &artifact.body) {
            Some(ArtifactBody::Canvas(document)) => document
                .pages
                .get(page)
                .map(|page| page.nodes.iter().map(|node| node.id.clone()).collect())
                .unwrap_or_default(),
            Some(ArtifactBody::Slides(document)) => document
                .pages
                .get(page)
                .map(|page| page.nodes.iter().map(|node| node.id.clone()).collect())
                .unwrap_or_default(),
            Some(ArtifactBody::Figure(figure)) => figure.panels.iter().map(|panel| panel.id.clone()).collect(),
            Some(ArtifactBody::Flow(document)) => {
                // A text document selects its paragraph `n` (counting from 1) as a text range.
                let paragraphs: Vec<(String, String)> = document
                    .sections
                    .iter()
                    .flat_map(|section| &section.blocks)
                    .filter_map(|block| match block {
                        FlowBlock::Paragraph(paragraph) => Some((paragraph.id.clone(), paragraph_text(paragraph))),
                        _ => None,
                    })
                    .collect();
                if let Some((id, text)) = select.and_then(|n| paragraphs.get(n.saturating_sub(1)).cloned()) {
                    self.select_paragraph(id, &text);
                }
                return;
            }
            _ => Vec::new(),
        };
        if page > 0 {
            self.view().page_index = page as u32;
            self.page_index = page as u32;
        }
        if let Some(count) = select.filter(|count| *count > 0) {
            let ids: Vec<String> = nodes.iter().take(count).cloned().collect();
            if !ids.is_empty() {
                self.select_ids(ids);
            }
        }
        if mark {
            if let Some(first) = nodes.first() {
                self.reservations.push(JobReservation {
                    reservation_id: String::from("smoke"),
                    job_id: String::from("smoke-job"),
                    owner: String::from("job smoke-job (claude)"),
                    node_ids: vec![first.clone()],
                });
                let id = new_id();
                let message = self.busy_reason(&[first.clone()]);
                self.held_edits.push(HeldEdit {
                    id: id.clone(),
                    apply: ApplyContext {
                        request_id: new_id(),
                        workspace_id: String::new(),
                        artifact_id: String::new(),
                        base_revision: 0,
                        operations: Vec::new(),
                        allowed_intents: BTreeSet::new(),
                        actor: String::from("desktop"),
                        user_edit: false,
                        retryable: false,
                        held_edit_id: None,
                        after_action: PendingAction::Apply,
                    },
                    nodes: vec![first.clone()],
                    state: HeldEditState::WaitingForJobRelease,
                    message: message.clone(),
                    can_retry: false,
                    retry_same_request: false,
                    reservations_checked: false,
                    checked_revision: None,
                });
                self.refusal = Some(message);
                self.refusal_ids = vec![first.clone()];
                self.held_refusal_id = Some(id);
            }
        }
    }

    pub(super) fn smoke_step(&mut self, ctx: &Context) {
        let Some(output) = self.smoke_output.as_ref() else {
            return;
        };
        self.smoke_frames = self.smoke_frames.saturating_add(1);
        let screenshot = ctx.input(|input| {
            input.events.iter().find_map(|event| match event {
                egui::Event::Screenshot { image, .. } => Some(Arc::clone(image)),
                _ => None,
            })
        });
        if let Some(screenshot) = screenshot {
            let write_result = save_screenshot(output, &screenshot);
            if let Err(error) = write_result {
                if let Some(shared) = &self.smoke_result {
                    *shared.lock().unwrap_or_else(|poison| poison.into_inner()) =
                        Some(error.to_string());
                }
            } else if let Some(failure) = self.smoke_failure.clone().filter(|failure| !(smoke_pdf_path().is_some() && pdf_refusal(failure))).or_else(|| {
                // In a PDF run the window's message is the result that the report keeps, not a failure of the run.
                if smoke_pdf_path().is_some() {
                    return None;
                }
                self.notices
                    .current(Instant::now())
                    .filter(|notice| notice.severity == Severity::Error)
                    .map(|notice| notice.text.clone())
            }) {
                if let Some(shared) = &self.smoke_result {
                    *shared.lock().unwrap_or_else(|poison| poison.into_inner()) = Some(failure);
                }
            }
            ctx.send_viewport_cmd(ViewportCommand::Close);
            return;
        }

        if !self.smoke_scene_done && self.artifact.is_some() && self.pending.is_empty() {
            self.smoke_scene_done = true;
            self.smoke_scene_frame = self.smoke_frames;
            self.apply_smoke_scene();
        }
        if let Some(path) = smoke_pdf_path() {
            if PDF_STEP.load(Ordering::SeqCst) != PDF_DONE {
                self.smoke_pdf_step(&path);
            }
        }
        // `DESIGNLLM_SMOKE_HOME_REFUSE=1` (home): a poster template made at phone size through the real 만들기 path, so the
        // engine's TEMPLATE_FIT refusal comes back and its callout shows under the button.
        if !self.smoke_scene_done
            && !self.screen_is_editor()
            && self.workspace_loaded
            && self.pending.is_empty()
            && std::env::var_os("DESIGNLLM_SMOKE_HOME_REFUSE").is_some()
        {
            self.smoke_scene_done = true;
            self.smoke_scene_frame = self.smoke_frames;
            self.invoke_ui("home.tile", json!({ "tile": "poster" }));
            self.invoke_ui("home.preset", json!({ "preset": "phone" }));
            self.invoke_ui("home.create", json!({ "tile": "poster" }));
        }
        // `DESIGNLLM_SMOKE_OPEN` also names popovers other screens open from `smoke_open` (e.g. `fill_popover`, read by the
        // properties panel); the export popover is opened by the top bar.
        // Popovers fade in over 120 ms and panels slide for 160 ms (tokens `motion`): the capture waits for
        // them to end (a few dozen frames after the scene).
        let settled = !self.smoke_scene_done || self.smoke_frames >= self.smoke_scene_frame + 30;
        if !self.smoke_requested
            && settled
            && self.pending.is_empty()
            && (self.workspace_loaded || self.smoke_frames >= 40)
            && !self.smoke_pdf_waiting()
        {
            self.smoke_requested = true;
            ctx.send_viewport_cmd(ViewportCommand::Screenshot(egui::UserData::default()));
        }
        // A PDF read can take the worker's whole budget (90 s), so a PDF run gets a longer limit. WARP draws on the CPU
        // (task 65: about 0.7 s a frame at 1440 x 900 measured on the development PC), so its run gets one too.
        let warp = crate::renderer::selected() == crate::renderer::Renderer::WgpuWarp;
        let limit = if smoke_pdf_path().is_some() { 240 } else if warp { 120 } else { 20 };
        if self.smoke_started.elapsed() > Duration::from_secs(limit) {
            if let Some(shared) = &self.smoke_result {
                let message = if self.smoke_requested {
                    "미리보기 화면 캡처 결과를 받지 못했습니다"
                } else {
                    "미리보기 화면 캡처가 제한 시간 안에 시작되지 않았습니다"
                };
                *shared.lock().unwrap_or_else(|poison| poison.into_inner()) =
                    Some(message.into());
            }
            ctx.send_viewport_cmd(ViewportCommand::Close);
        }
    }
}
