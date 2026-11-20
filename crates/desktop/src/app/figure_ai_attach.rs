//! Evidence files for a Figure's AI request: the PDF, CSV/TSV and text files the user attaches in the
//! AI tab of the open Figure. They are a list of the editor's own, apart from the home list
//! (`home.attachments`). A request imports each file as a workspace asset (`asset.import`, the import
//! the home request uses), and sends `job.start` with those assets as `source_refs` only when every
//! import succeeded and the Figure is still the artifact and revision the request was asked at. A failed
//! or cancelled request keeps the prompt and the list. The files are read, never changed or removed.
//!
//! The list is changed only by the user's own clicks (not by a registry command), so an agent run cannot
//! attach a file from the disk. The list belongs to the Figure it was attached for: another document never
//! shows it or sends it (`figure_ai_files`), and switching documents drops it once the user has agreed (figure_close.rs).

use std::{collections::BTreeSet, path::PathBuf, time::Instant};

use designllm_contracts::{new_id, ArtifactKind, AssetRef, DltError, FigureRevisionRef, Intent, JobStartRequest};
use eframe::egui::{self, Ui};
use serde_json::{json, Value};

use super::{
    drafts::{figure_slot, Body, Live},
    figure_close::{job_running, LATE_START_WAIT},
    user_error, DesktopApp, PendingAction,
};

/// Most evidence files one request carries; each file is one host import.
pub(super) const MAX_FIGURE_AI_ATTACHMENTS: usize = 16;

/// Why a new AI request for the open Figure waits: the send control shows it, and the request paths refuse with it.
const FIGURE_JOB_WAIT: &str = "이 그림의 AI 작업이 진행 중입니다. 끝나면 다시 요청할 수 있습니다";

/// The same for another document (a canvas, slides or a text document).
const DOCUMENT_JOB_WAIT: &str = "이 작업물의 AI 작업이 진행 중입니다. 끝나면 다시 요청할 수 있습니다";

/// Why a new AI request waits while its evidence files are being imported.
const IMPORT_WAIT: &str = "근거 자료를 준비하는 중입니다. 끝나거나 취소한 뒤 다시 요청하세요";

/// The extensions the attach check accepts; `dialogs::open_figure_evidence` offers the same ones.
const EVIDENCE_EXTENSIONS: &[&str] = &["pdf", "csv", "tsv", "txt", "md"];

/// One evidence file in the list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct FigureAiAttachment {
    pub path: PathBuf,
    pub name: String,
}

/// The evidence list of one Figure, bound to the artifact it was attached for.
#[derive(Clone, Debug)]
pub(super) struct FigureAiAttachments {
    pub artifact_id: String,
    pub files: Vec<FigureAiAttachment>,
}

/// A Figure AI request whose evidence files are being imported, or whose job is being started.
pub(super) struct FigureAiRequest {
    pub token: String,
    pub artifact_id: String,
    /// The revision the request was asked at. The job starts only while the Figure is still at it.
    pub base_revision: u64,
    pub prompt: String,
    pub runtime: String,
    pub model: String,
    pub effort: String,
    pub allowed_intents: BTreeSet<Intent>,
    pub figure_refs: Vec<FigureRevisionRef>,
    /// The selection the request targets (`ai_target_ids`), as it was when the request started. Empty: the whole Figure.
    pub target_ids: Vec<String>,
    /// The list as it was when the request started; the imports go through it in order.
    pub files: Vec<FigureAiAttachment>,
    /// The assets of the files imported so far, in the order of `files`.
    pub imported: Vec<AssetRef>,
}

impl DesktopApp {
    /// The list belongs to the open Figure: it is shown there, and only its AI request carries it.
    pub(super) fn figure_ai_list_applies(&self) -> bool {
        self.screen_is_editor() && self.active.as_ref().is_some_and(|active| active.kind == ArtifactKind::Figure)
    }

    /// The files of the list when it belongs to the open document; a list of another document is never shown or sent.
    pub(super) fn figure_ai_files(&self) -> &[FigureAiAttachment] {
        let Some(active) = self.active.as_ref() else {
            return &[];
        };
        match self.figure_ai_attachments.as_ref() {
            Some(list) if list.artifact_id == active.id => &list.files,
            _ => &[],
        }
    }

    /// The list of the open document, for changes. `None` when it belongs to another document.
    fn figure_ai_list_mut(&mut self) -> Option<&mut FigureAiAttachments> {
        let active_id = &self.active.as_ref()?.id;
        self.figure_ai_attachments.as_mut().filter(|list| &list.artifact_id == active_id)
    }

    /// Whether the AI request carries the list now: the open Figure with at least one file.
    pub(super) fn figure_ai_sources_apply(&self) -> bool {
        self.figure_ai_list_applies() && !self.figure_ai_files().is_empty()
    }

    /// Why a new AI request for the open Figure waits: a job of this Figure is still running (shown, or kept aside), its job
    /// start has not been answered yet, or the window no longer waits for such a start (`LATE_START_WAIT`). `None` when a new
    /// request may start.
    pub(super) fn figure_job_wait(&self) -> Option<&'static str> {
        let active = self.active.as_ref().filter(|active| active.kind == ArtifactKind::Figure)?;
        self.document_job_wait(&active.id, FIGURE_JOB_WAIT)
    }

    /// Why a new AI request for the open document waits: its evidence files are being imported, or a job of the document
    /// runs or is being started. A Figure's words are `figure_job_wait`'s. `None` when a new request may start on this side.
    pub(super) fn ai_job_wait(&self) -> Option<&'static str> {
        if self.figure_ai_request.is_some() {
            return Some(IMPORT_WAIT);
        }
        let active = self.active.as_ref()?;
        if active.kind == ArtifactKind::Figure {
            return self.figure_job_wait();
        }
        self.document_job_wait(&active.id, DOCUMENT_JOB_WAIT)
    }

    /// Why a new request for the document `artifact_id` waits for its jobs: one runs (shown, or kept aside), a start is not
    /// answered yet (`words`), or a start the window no longer waits for is not answered (`LATE_START_WAIT`).
    fn document_job_wait(&self, artifact_id: &str, words: &'static str) -> Option<&'static str> {
        let running = self.job.iter().chain(self.parked_jobs.iter()).any(|job| job.artifact_id == artifact_id && job_running(job));
        let starting = self.pending.values().any(|pending| {
            matches!(&pending.action, PendingAction::StartJob | PendingAction::FigureAiJob { .. })
                && pending.artifact_id.as_deref() == Some(artifact_id)
        });
        if running || starting {
            return Some(words);
        }
        self.late_start_waits(artifact_id).then_some(LATE_START_WAIT)
    }

    /// Why the AI request of the open document cannot be sent now with this prompt, `None` when it can. The request control,
    /// `ai.request` and `ai.retry` all refuse with these words.
    pub(super) fn ai_request_block(&self, prompt: &str) -> Option<&'static str> {
        if self.active.is_none() {
            return Some("작업물을 열어야 요청할 수 있습니다");
        }
        if let Some(reason) = self.ai_selection_refusal() {
            return Some(reason);
        }
        if let Some(reason) = self.ai_job_wait() {
            return Some(reason);
        }
        if self.ai_model.trim().is_empty() {
            return Some("실행기 메뉴에서 모델을 고르세요");
        }
        if prompt.trim().is_empty() {
            return Some("요청 내용을 적어 주세요");
        }
        if !(self.allow_content || self.allow_design || self.allow_data) {
            return Some("내용, 디자인, 데이터 중 하나 이상을 허용해 주세요");
        }
        None
    }

    /// Adds one file to the list. Nothing reaches the host until a request starts.
    pub(super) fn add_figure_ai_attachment(&mut self, path: PathBuf) -> Result<(), String> {
        if !self.figure_ai_list_applies() {
            return Err("그림을 연 상태에서 근거 자료를 첨부할 수 있습니다".into());
        }
        if self.figure_ai_request.is_some() {
            return Err("AI 요청을 준비하는 동안에는 근거 자료를 바꿀 수 없습니다".into());
        }
        let extension = path.extension().and_then(|extension| extension.to_str()).unwrap_or("").to_ascii_lowercase();
        if !EVIDENCE_EXTENSIONS.contains(&extension.as_str()) {
            return Err("PDF, CSV/TSV, 텍스트(.txt, .md) 파일만 근거 자료로 첨부할 수 있습니다".into());
        }
        if !path.is_file() {
            return Err("근거 자료 파일을 찾지 못했습니다".into());
        }
        if self.figure_ai_files().iter().any(|file| file.path == path) {
            return Err("이미 첨부한 파일입니다".into());
        }
        if self.figure_ai_files().len() >= MAX_FIGURE_AI_ATTACHMENTS {
            return Err(format!("근거 자료는 한 요청에 {MAX_FIGURE_AI_ATTACHMENTS}개까지 첨부할 수 있습니다"));
        }
        let Some(artifact_id) = self.active.as_ref().map(|active| active.id.clone()) else {
            return Err("그림을 연 상태에서 근거 자료를 첨부할 수 있습니다".into());
        };
        let name = path.file_name().map_or_else(String::new, |name| name.to_string_lossy().into_owned());
        if self.figure_ai_list_mut().is_none() {
            self.figure_ai_attachments = Some(FigureAiAttachments { artifact_id, files: Vec::new() });
        }
        if let Some(list) = self.figure_ai_list_mut() {
            list.files.push(FigureAiAttachment { path, name });
        }
        Ok(())
    }

    /// Removes one file from the list. Refused while a request is using the list.
    pub(super) fn remove_figure_ai_attachment(&mut self, index: usize) -> Result<(), String> {
        if self.figure_ai_request.is_some() {
            return Err("AI 요청을 준비하는 동안에는 근거 자료를 뺄 수 없습니다".into());
        }
        if let Some(list) = self.figure_ai_list_mut() {
            if index < list.files.len() {
                list.files.remove(index);
            }
        }
        Ok(())
    }

    /// Drops the list when it belongs to another document than the open one (the list is never carried over).
    pub(super) fn forget_figure_ai_list_of_other_document(&mut self) {
        if self.figure_ai_list_mut().is_none() {
            self.figure_ai_attachments = None;
        }
    }

    /// Starts a request that carries the list: the imports first, then the job. `start_ai_job` (app.rs) calls it.
    pub(super) fn start_figure_ai_request(&mut self) {
        if self.figure_ai_request.is_some() {
            self.set_error("근거 자료를 준비하는 중입니다. 끝나거나 취소한 뒤 다시 요청하세요".into());
            return;
        }
        if let Some(reason) = self.figure_job_wait() {
            self.set_error(reason.into());
            return;
        }
        let Some((artifact_id, base_revision)) = self.active.as_ref().map(|active| (active.id.clone(), active.revision)) else {
            return;
        };
        let token = new_id();
        self.figure_ai_request = Some(FigureAiRequest {
            token: token.clone(),
            artifact_id,
            base_revision,
            prompt: self.ai_prompt.trim().into(),
            runtime: self.ai_runtime.clone(),
            model: self.ai_model.clone(),
            effort: self.ai_effort.clone(),
            allowed_intents: self.ai_intents(),
            figure_refs: self.ai_figure_refs.clone(),
            target_ids: self.ai_target_ids(),
            files: self.figure_ai_files().to_vec(),
            imported: Vec::new(),
        });
        self.send_figure_ai_import(&token, 0);
    }

    fn send_figure_ai_import(&mut self, token: &str, index: usize) {
        let Some(path) = self
            .figure_ai_request
            .as_ref()
            .filter(|request| request.token == token)
            .and_then(|request| request.files.get(index))
            .map(|file| file.path.clone())
        else {
            return;
        };
        let params = json!({ "path": path.to_string_lossy() });
        if self.call("asset.import", params, PendingAction::FigureAiImport { token: token.to_owned() }).is_none() {
            self.figure_ai_request = None;
        }
    }

    /// The reply of one evidence import: the next file, or the job once every file is imported. The files are imported one at
    /// a time, so the reply is for the file after the ones already imported.
    pub(super) fn figure_ai_import_done(&mut self, token: &str, value: Value) {
        let Some(current) = self.figure_ai_request.as_ref().filter(|request| request.token == token) else {
            // Cancelled or replaced: the asset stays in the workspace unused, and nothing else follows.
            return;
        };
        if !self.figure_ai_request_is_current(current) {
            self.figure_ai_request = None;
            self.notices.warning("그림이나 작업물이 바뀌어 AI 요청을 보내지 않았습니다. 요청문과 첨부는 남겨 두었습니다", Instant::now());
            return;
        }
        let asset: AssetRef = match serde_json::from_value(value) {
            Ok(asset) => asset,
            Err(_) => {
                self.figure_ai_request = None;
                self.set_error("근거 자료 가져오기 응답을 읽지 못했습니다. 요청문과 첨부는 남겨 두었습니다".into());
                return;
            }
        };
        let Some(request) = self.figure_ai_request.as_mut() else { return };
        request.imported.push(asset);
        let imported = request.imported.len();
        if imported < request.files.len() {
            self.send_figure_ai_import(token, imported);
        } else {
            self.send_figure_ai_job(token);
        }
    }

    /// Starts the job with every imported asset as a source, if the Figure is still the artifact and revision of the request.
    fn send_figure_ai_job(&mut self, token: &str) {
        let Some(request) = self.figure_ai_request.as_ref().filter(|request| request.token == token) else { return };
        if !self.figure_ai_request_is_current(request) {
            self.figure_ai_request = None;
            self.notices.warning("그림이나 작업물이 바뀌어 AI 요청을 보내지 않았습니다. 요청문과 첨부는 남겨 두었습니다", Instant::now());
            return;
        }
        let live = self.figure_request_live(request);
        let slot = figure_slot(&request.artifact_id);
        let params = JobStartRequest {
            source_refs: request.imported.clone(),
            figure_refs: request.figure_refs.clone(),
            target_ids: request.target_ids.clone(),
            ..JobStartRequest::new(
                new_id(),
                request.artifact_id.clone(),
                request.base_revision,
                request.allowed_intents.clone(),
                request.runtime.clone(),
                request.model.clone(),
                request.effort.clone(),
                request.prompt.clone(),
            )
        };
        // The request is marked in the figure's draft before it goes out, and the draft keeps the tab's text (jobs.rs).
        if !self.send_job_start(params, &slot, live, PendingAction::FigureAiJob { token: token.to_owned() }, true) {
            self.figure_ai_request = None;
        }
    }

    /// What a request carries, as its draft holds it: the prompt it was started with, its files and the figure references.
    fn figure_request_live(&self, request: &FigureAiRequest) -> Live {
        Live {
            artifact_id: Some(request.artifact_id.clone()),
            revision: Some(request.base_revision),
            body: Body::Figure {
                prompt: request.prompt.clone(),
                attachments: request.files.iter().map(|file| file.path.to_string_lossy().into_owned()).collect(),
                figure_refs: request.figure_refs.clone(),
            },
        }
    }

    /// The reply of `job.start` for the request: the job is shown, and the list stays for the next request. A reply of a request
    /// the window no longer holds (its document was left) is still recorded: the host runs its job (`late_job_start`). The
    /// request's draft is settled before this, by its answer (`answer_sent_drafts`).
    pub(super) fn figure_ai_job_started(&mut self, token: &str, value: Value) {
        if !self.figure_ai_request_holds(token) {
            self.late_job_start(value);
            return;
        }
        self.end_figure_ai_request(token);
        self.job_started(value);
    }

    /// A failed import of a request the window holds. The failure of a request it no longer holds is dropped.
    pub(super) fn fail_figure_ai_import(&mut self, token: &str, error: &DltError) {
        if !self.figure_ai_request_holds(token) {
            return;
        }
        self.end_figure_ai_request(token);
        let _ = self.state_moved_on(error, true);
        self.set_error(format!("근거 자료를 가져오지 못해 AI 요청을 보내지 않았습니다. 요청문과 첨부는 남겨 두었습니다: {}", user_error(error)));
    }

    /// A refused job start of a request the window holds. The refusal of a request it no longer holds is dropped.
    pub(super) fn fail_figure_ai_job(&mut self, token: &str, error: &DltError) {
        if !self.figure_ai_request_holds(token) {
            return;
        }
        self.end_figure_ai_request(token);
        let _ = self.state_moved_on(error, true);
        self.set_error(format!("AI 요청을 보내지 못했습니다. 요청문과 첨부는 남겨 두었습니다: {}", user_error(error)));
    }

    /// Whether the request the window holds is the one with this token.
    pub(super) fn figure_ai_request_holds(&self, token: &str) -> bool {
        self.figure_ai_request.as_ref().is_some_and(|request| request.token == token)
    }

    /// A reply the window dropped because the open document changed. An import leaves only an unused asset behind, and the
    /// request is ended. A job start is not ended here: its answer is recorded when it comes (`late_job_start`).
    pub(super) fn figure_ai_reply_dropped(&mut self, action: &PendingAction) {
        match action {
            PendingAction::FigureAiImport { token, .. } => {
                if self.figure_ai_request_holds(token) {
                    self.figure_ai_request = None;
                    self.notices.warning(
                        "작업물을 바꿔 AI 요청을 보내지 않았습니다. 요청문과 첨부는 남겨 두었습니다",
                        Instant::now(),
                    );
                }
            }
            PendingAction::FigureAiJob { token } => self.end_figure_ai_request(token),
            _ => {}
        }
    }

    /// Cancels a request while its files are still being imported. No job starts; the prompt and the list stay.
    pub(super) fn cancel_figure_ai_request(&mut self) -> Result<(), String> {
        let Some(request) = self.figure_ai_request.as_ref() else {
            return Err("취소할 AI 요청이 없습니다".into());
        };
        if request.imported.len() == request.files.len() {
            return Err("AI 작업을 시작하는 중이라 취소할 수 없습니다".into());
        }
        self.figure_ai_request = None;
        self.notices.info("요청을 취소했습니다. 요청문과 첨부는 남겨 두었습니다. 이미 가져온 자료는 사용되지 않은 채 남을 수 있습니다", Instant::now());
        Ok(())
    }

    pub(super) fn end_figure_ai_request(&mut self, token: &str) {
        if self.figure_ai_request.as_ref().is_some_and(|request| request.token == token) {
            self.figure_ai_request = None;
        }
    }

    /// The open Figure is still the artifact and revision the request was asked at.
    fn figure_ai_request_is_current(&self, request: &FigureAiRequest) -> bool {
        self.screen_is_editor()
            && self.active.as_ref().is_some_and(|active| {
                active.id == request.artifact_id && active.kind == ArtifactKind::Figure && active.revision == request.base_revision
            })
            && self.context.as_ref().is_some_and(|context| {
                context.artifact_id == request.artifact_id && context.revision == request.base_revision
            })
    }

    /// Puts a recovered request back into the open Figure's AI tab: its prompt, its evidence files and its figure references.
    /// The files are the ones that exist; the caller has checked the Figure and the empty tab (`drafts.rs`).
    pub(super) fn restore_figure_ai_draft(&mut self, prompt: String, files: Vec<PathBuf>, figure_refs: Vec<FigureRevisionRef>) {
        let Some(artifact_id) = self.active.as_ref().map(|active| active.id.clone()) else {
            return;
        };
        self.ai_prompt = prompt;
        // The references are shared with the home box, so they are added to, never replaced.
        for reference in figure_refs {
            if !self.ai_figure_refs.contains(&reference) {
                self.ai_figure_refs.push(reference);
            }
        }
        let files = files
            .into_iter()
            .map(|path| {
                let name = path.file_name().map_or_else(String::new, |name| name.to_string_lossy().into_owned());
                FigureAiAttachment { path, name }
            })
            .collect();
        self.figure_ai_attachments = Some(FigureAiAttachments { artifact_id, files });
    }

    fn attach_figure_ai_file_from_dialog(&mut self) {
        let Some(path) = crate::dialogs::open_figure_evidence(self.parent_hwnd) else { return };
        if let Err(message) = self.add_figure_ai_attachment(path) {
            self.set_error(message);
        }
    }

    /// The evidence list under the AI request box: the files with a removal for each, the attach button, and the
    /// progress with a cancel button while the imports run.
    pub(super) fn draw_figure_ai_attachments(&mut self, ui: &mut Ui) {
        if !self.figure_ai_list_applies() {
            return;
        }
        let busy = self.figure_ai_request.as_ref().map(|request| (request.imported.len(), request.files.len()));
        let mut attach = false;
        let mut remove = None;
        let mut cancel = false;
        ui.horizontal(|ui| {
            ui.label(format!("근거 자료 ({})", self.figure_ai_files().len()));
            if ui.add_enabled(busy.is_none(), egui::Button::new("첨부")).clicked() {
                attach = true;
            }
        });
        for (index, file) in self.figure_ai_files().iter().enumerate() {
            ui.horizontal(|ui| {
                ui.label(file.name.as_str());
                if ui.add_enabled(busy.is_none(), egui::Button::new("빼기")).clicked() {
                    remove = Some(index);
                }
            });
        }
        if let Some((imported, total)) = busy {
            ui.horizontal(|ui| {
                if imported < total {
                    ui.label(format!("근거 자료 가져오는 중 {imported}/{total}"));
                } else {
                    ui.label("AI 요청을 보내는 중");
                }
                if ui.add_enabled(imported < total, egui::Button::new("취소")).clicked() {
                    cancel = true;
                }
            });
        }
        if cancel {
            if let Err(message) = self.cancel_figure_ai_request() {
                self.set_error(message);
            }
        }
        if let Some(index) = remove {
            if let Err(message) = self.remove_figure_ai_attachment(index) {
                self.set_error(message);
            }
        }
        if attach {
            self.attach_figure_ai_file_from_dialog();
        }
    }
}
