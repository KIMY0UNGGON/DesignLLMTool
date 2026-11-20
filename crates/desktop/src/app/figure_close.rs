//! Figure work that a window close or a document switch would leave behind: PDF records held for their figure, a PDF import in
//! progress, evidence files no AI request has sent, a value of the figure's data under edit, and figure AI jobs (a job whose
//! start is unanswered, and jobs the host still runs). A close shows it in the close confirmation (`held_edit_close_warning` in
//! app.rs). A switch to another document asks first (`draw_figure_switch_dialog`). The jobs of a document the window leaves are
//! kept aside in `parked_jobs`: they are neither shown nor operated from the document opened next, and a close still counts
//! them. Nothing here discards, sends or applies anything without the user's answer.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use designllm_contracts::{ArtifactKind, ArtifactSummary, AssetRef, DltError, Job, JobStartRequest, JobStatus};
use eframe::egui::{self, Context, Id};
use serde_json::{json, Value};

use super::drafts::{same_content, Body, Draft};
use super::{DesktopApp, PendingAction};
use crate::notice::Severity;
use crate::rpc::RpcPayload;

/// What the window says when a job start was not answered as the document was left: the job may have started.
const JOB_START_UNKNOWN: &str = "AI 요청을 끝내지 못했습니다. 작업이 이미 시작됐을 수 있으니 작업 상태를 확인하세요";

/// What the window says when a value sent for the figure was not answered as the figure was left: the change may be in it.
const EDIT_UNKNOWN: &str = "전송 중이던 자료 값 변경은 이미 작업물에 반영됐을 수 있습니다. 작업물을 열어 확인하세요";

/// How long the window waits for the answer of a job start or an evidence import (`expire_awaited_starts`). The bridge gives up a
/// request 120 s after it was written (`DEFAULT_CALL_TIMEOUT`), and the RPC worker may first queue the request behind a long read.
/// The window cannot see when the worker sent it, so the limit is the bridge's plus 30 s for that queue. An answer that comes later
/// is still recorded (`late_starts`): a longer wait changes when the warning shows, not the outcome.
pub(super) const AWAITED_REPLY_LIMIT: Duration = Duration::from_secs(150);

/// What the window says when a job start it no longer waits for is answered with a job: the host runs that job.
pub(super) const LATE_JOB_START: &str = "이전 요청의 AI 작업이 시작됐습니다. 실행 상태를 확인하세요";

/// What the window says when the answer of such a start is not readable: its job may have started, and nothing else shows it.
pub(super) const LATE_JOB_UNREADABLE: &str = "이전 요청의 AI 작업 상태를 읽지 못했습니다. 작업 목록을 확인하세요";

/// What the window says when such a start is answered without knowing whether its job started (the connection was lost).
pub(super) const LATE_START_UNKNOWN: &str = "이전 AI 요청의 응답을 받지 못했습니다. 작업이 시작됐을 수 있으니 작업 상태를 확인하세요";

/// What the window says while a job start is unanswered and the window no longer waits for it: a new request for the same
/// document waits for the answer.
pub(super) const LATE_START_WAIT: &str = "이전 AI 요청의 응답을 기다리는 중입니다. 응답이 오면 다시 요청할 수 있습니다";

/// What the window says when a job start is not answered in time: the job may have started, so the answer is awaited.
const START_TIMED_OUT: &str = "AI 요청의 응답을 제때 받지 못했습니다. 작업이 이미 시작됐을 수 있으니 작업 목록을 확인하세요. 응답이 오기 전에는 다시 요청할 수 없습니다";

/// What the window says when an evidence import is not answered in time: no job has started, so the request can be sent again.
const IMPORT_TIMED_OUT: &str = "근거 자료 가져오기 응답을 제때 받지 못해 AI 요청을 보내지 않았습니다. 요청문과 첨부는 남겨 두었습니다";

/// Figure work the window holds, counted for the warnings and for what a discard drops.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct FigureWork {
    /// PDF records held and not on their way into the figure.
    pub pdf_pending: usize,
    /// PDF records whose commit is on its way.
    pub pdf_sending: usize,
    /// A PDF import (file or page read) of the open figure is in progress.
    pub pdf_import: bool,
    /// Evidence files no AI request has sent.
    pub files: usize,
    /// A value of the figure's data the user changed and has not kept (`dataset_edit`).
    pub cell_draft: bool,
    /// A CSV import of the figure's table that the user has not finished (`csv_draft`).
    pub csv_draft: bool,
    /// Figure AI job starts the host has not answered yet, including those the window no longer waits for (`late_starts`).
    pub job_starting: usize,
    /// Figure AI jobs the host still runs.
    pub jobs_running: usize,
    /// Values of the figure's data sent and not answered yet: the change may already be in the figure.
    pub edit_sending: usize,
}

/// A job start the window sent: its document, its request id, its content (the request without its id) and the draft it was sent
/// from, if any (`send_job_start`). It is kept until the host answers, so that a start whose answer was lost can be sent again as
/// it was (`with_job_request_id`).
#[derive(Clone, Debug, PartialEq)]
pub(super) struct SentStart {
    pub artifact_id: String,
    pub request_id: String,
    pub content: Value,
    pub draft: Option<Body>,
}

impl SentStart {
    /// The record of a `job.start` request. `None` when the params carry no request id or no artifact id.
    pub(super) fn of(params: &Value) -> Option<Self> {
        let mut content = params.clone();
        let fields = content.as_object_mut()?;
        let request_id = fields.remove("request_id")?.as_str()?.to_owned();
        let artifact_id = fields.get("artifact_id")?.as_str()?.to_owned();
        Some(Self { artifact_id, request_id, content, draft: None })
    }

    /// The params of the start as it was sent.
    fn params(&self) -> Value {
        let mut params = self.content.clone();
        params["request_id"] = json!(self.request_id);
        params
    }

    /// Returns `None` when the document matches but either start lacks a draft to compare.
    fn compare_draft(&self, artifact_id: &str, draft: Option<&Body>) -> Option<bool> {
        if self.artifact_id != artifact_id {
            return Some(false);
        }
        Some(same_content(self.draft.as_ref()?, draft?))
    }

    /// Whether a new start `sent` (sent from the draft `draft`, if any) of the same document is sent from the same draft. Two
    /// starts sent from drafts compare as drafts do (`same_content`); otherwise their prompts are compared.
    fn same_draft(&self, sent: &SentStart, draft: Option<&Body>) -> bool {
        if let Some(matches) = self.compare_draft(&sent.artifact_id, draft) {
            return matches;
        }
        let prompt = |content: &Value| content.get("prompt").and_then(Value::as_str).map(str::trim).map(str::to_owned);
        prompt(&self.content) == prompt(&sent.content)
    }

    /// Whether a new start carries the same request as this one apart from the base revision: the same intents, runtime, model,
    /// effort, delivery, targets and sources. An edit of the document since does not make it another request; any other change does.
    fn same_request(&self, sent: &SentStart) -> bool {
        match (self.content.as_object(), sent.content.as_object()) {
            (Some(kept), Some(new)) => {
                kept.len() == new.len() && kept.iter().all(|(key, value)| key == "base_revision" || new.get(key) == Some(value))
            }
            _ => self.content == sent.content,
        }
    }

    /// Whether a new start `sent` (sent from the draft `draft`, if any) asks for what this start asked: the same draft and the
    /// same request apart from the base revision.
    fn asked_again_by(&self, sent: &SentStart, draft: Option<&Body>) -> bool {
        self.same_draft(sent, draft) && self.same_request(sent)
    }
}

/// The most job starts whose outcome is unknown that the window keeps for a repeat, over all documents. A start recorded past it
/// drops one record, first one of a document that is not in the workspace's document list any more (deleted or no longer
/// listed; the window has no tabs, so this list is what it can open), otherwise the oldest, and the user is told
/// (`settle_job_start`). A record is not dropped when its document is left or goes, since its answer can still come. A repeat of
/// the dropped request then goes out as a new request. A start marked in a draft comes back after a restart from the draft file
/// (`UnknownStarts::recover`); the records themselves are kept in memory only.
pub(super) const UNKNOWN_STARTS_MAX: usize = 16;

/// Said when a request is the same as an unknown start but for the base revision: that start goes out again as it was.
pub(super) const REPEAT_OF_UNKNOWN_START: &str =
    "결과를 확인하지 못한 이전 AI 요청을 그대로 다시 보내 결과를 확인합니다. 그 뒤의 문서 편집은 이 요청에 들어가지 않습니다";

/// Said when a request has the draft of an unknown start but other settings (intents, runtime, model, effort, delivery): it goes
/// out as a new request, and the unknown start may still run.
pub(super) const UNKNOWN_START_MAY_RUN: &str =
    "결과를 확인하지 못한 같은 초안의 이전 AI 요청이 이미 실행 중일 수 있습니다. 바뀐 설정으로 새 요청을 보냈습니다";
const BOARD_UNKNOWN_START_MAY_RUN: &str =
    "같은 요청문으로 보낸 이전 AI 요청이 이미 실행 중일 수 있습니다. 바뀐 설정으로 새 요청을 보냈습니다";

/// The job starts whose answer left their outcome unknown, oldest first (`settle_job_start`). Each document may have several,
/// one per request (`asked_again_by`). Asking for one of them again sends it again as it was, with its id and its content, so the
/// host answers it from its receipt and makes no second job.
#[derive(Debug, Default)]
pub(super) struct UnknownStarts {
    starts: VecDeque<SentStart>,
    /// The words for records dropped past `UNKNOWN_STARTS_MAX` that the user has not been shown yet (`show_dropped_unknown_starts`).
    dropped: Vec<String>,
}

impl UnknownStarts {
    /// Keeps a start for a repeat. A record of the same request or with the same id is replaced. Past `UNKNOWN_STARTS_MAX` one
    /// record is dropped and returned: the oldest of a document not `listed` in the workspace, otherwise the oldest.
    fn keep(&mut self, sent: SentStart, listed: impl Fn(&str) -> bool) -> Option<SentStart> {
        self.starts.retain(|kept| {
            kept.artifact_id != sent.artifact_id
                || (kept.request_id != sent.request_id && !kept.asked_again_by(&sent, sent.draft.as_ref()))
        });
        self.starts.push_back(sent);
        if self.starts.len() <= UNKNOWN_STARTS_MAX {
            return None;
        }
        let at = self.starts.iter().position(|kept| !listed(&kept.artifact_id)).unwrap_or(0);
        self.starts.remove(at)
    }

    /// The unanswered start that a new start `sent` (sent from the draft `draft`, if any) asks for again.
    fn asked_again(&self, sent: &SentStart, draft: Option<&Body>) -> Option<&SentStart> {
        self.starts.iter().find(|kept| kept.asked_again_by(sent, draft))
    }

    /// Whether an unanswered start was sent from the same draft as `sent` (with other settings, when `asked_again` finds none).
    fn same_draft_kept(&self, sent: &SentStart, draft: Option<&Body>) -> bool {
        self.starts.iter().any(|kept| kept.same_draft(sent, draft))
    }

    /// The newest other unknown start from this document and draft, if its mark is still needed after a refusal.
    pub(super) fn other_draft_mark(&self, draft: &Draft, refused_id: &str) -> Option<&str> {
        let artifact_id = draft.artifact_id.as_deref()?;
        let kept = self.starts.iter().rev().find(|kept| {
            kept.request_id != refused_id && kept.compare_draft(artifact_id, Some(&draft.body)) == Some(true)
        })?;
        Some(&kept.request_id)
    }

    /// Try the already imported assets only for an exact repeat of a claimed request.
    pub(super) fn sources_for_repeat(&self, id: &str, request: &JobStartRequest, body: &Body) -> Option<Vec<AssetRef>> {
        let kept = self.starts.iter().find(|kept| kept.request_id == id)?;
        let mut sent = SentStart::of(&serde_json::to_value(request).ok()?)?;
        let sources = kept.content.get("source_refs")?;
        sent.content["source_refs"] = sources.clone();
        if !kept.asked_again_by(&sent, Some(body)) {
            return None;
        }
        serde_json::from_value(sources.clone()).ok()
    }

    /// Ends the record of the start of the document with that id; the other records stay.
    fn end(&mut self, artifact_id: &str, request_id: &str) {
        self.starts.retain(|kept| kept.artifact_id != artifact_id || kept.request_id != request_id);
    }

    /// Whether a start of the document is kept.
    #[cfg(test)]
    pub(super) fn contains_key(&self, artifact_id: &str) -> bool {
        self.starts.iter().any(|kept| kept.artifact_id == artifact_id)
    }

    /// The number of starts kept.
    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.starts.len()
    }
}

impl FigureWork {
    /// Whether something the user can discard is held: PDF records, a PDF import, evidence files or a changed value.
    fn holds_discardable(&self) -> bool {
        self.pdf_pending > 0 || self.pdf_sending > 0 || self.pdf_import || self.files > 0 || self.cell_draft || self.csv_draft
    }

    /// Whether figure jobs are the only figure work held: closing then cancels none of them and discards nothing.
    fn only_jobs(&self) -> bool {
        self.jobs_running > 0 && self.job_starting == 0 && !self.holds_discardable()
    }
}

/// Whether the host still runs the job (queued, running or validating).
pub(super) fn job_running(job: &Job) -> bool {
    super::chrome::job_status_running(job.status)
}

/// Whether `reported` is of an attempt older than the one `known` holds: `job.resume` makes a new attempt with its own id and
/// the next number (`Job::attempt_seq`). A job without a number is a first attempt: from this host only a `job.start` receipt
/// stored by a build before `attempt_seq` lacks it (`job.get` and `job.resume` read the number from the job's row). Without any
/// number (a host that reports none) the order of two attempts is not known. Two attempts with one number cannot both be the
/// host's (it counts each resume), so the one the window holds stays.
fn older_attempt(known: &Job, reported: &Job) -> bool {
    if known.attempt_seq.is_none() && reported.attempt_seq.is_none() {
        return false;
    }
    let (known_seq, reported_seq) = (known.attempt_seq.unwrap_or(1), reported.attempt_seq.unwrap_or(1));
    reported_seq < known_seq || (reported_seq == known_seq && reported.attempt_id != known.attempt_id)
}

/// Whether `reported` is a status the window has already passed: one of an older attempt (`older_attempt`), or one the same
/// attempt has passed. Within one attempt the host goes queued -> running/validating -> an end, and an ended attempt never runs
/// again (`set_job_status`).
fn older_status(known: &Job, reported: &Job) -> bool {
    older_attempt(known, reported)
        || (known.attempt_id == reported.attempt_id
            && ((!job_running(known) && job_running(reported))
                || (known.status != JobStatus::Queued && reported.status == JobStatus::Queued)))
}

/// The words for figure work, `None` when there is none. `leaving` names the action ("창을 닫으면" or "작업물을 바꾸면").
fn describe_figure_work(work: &FigureWork, leaving: &str) -> Option<String> {
    let mut items = Vec::new();
    if work.pdf_pending > 0 {
        items.push(format!("보류한 PDF 근거 {}개", work.pdf_pending));
    }
    if work.pdf_sending > 0 {
        items.push(format!("전송 중인 PDF 근거 {}개", work.pdf_sending));
    }
    if work.pdf_import {
        items.push("진행 중인 PDF 근거 가져오기 1개".to_owned());
    }
    if work.files > 0 {
        items.push(format!("보내지 않은 근거 자료 {}개", work.files));
    }
    if work.cell_draft {
        items.push("편집 중인 자료 값 1개".to_owned());
    }
    if work.csv_draft {
        items.push("편집 중인 CSV 가져오기 1개".to_owned());
    }
    if work.job_starting > 0 {
        items.push(format!("시작 응답을 기다리는 그림 AI 작업 {}개", work.job_starting));
    }
    if work.jobs_running > 0 {
        items.push(format!("실행 중인 그림 AI 작업 {}개", work.jobs_running));
    }
    if work.edit_sending > 0 {
        items.push(format!("전송 중인 자료 값 변경 {}개", work.edit_sending));
    }
    if items.is_empty() {
        return None;
    }
    let mut text = format!("그림에 남은 작업: {}.", items.join(", "));
    if work.pdf_pending + work.files > 0 {
        text.push_str(&format!(" {leaving} 보류한 PDF 근거와 보내지 않은 근거 자료는 버려집니다."));
    }
    if work.cell_draft {
        text.push_str(&format!(" {leaving} 편집 중인 자료 값은 버려집니다."));
    }
    if work.csv_draft {
        text.push_str(&format!(" {leaving} 편집 중인 CSV 가져오기는 버려집니다."));
    }
    if work.pdf_import {
        text.push_str(" 진행 중인 PDF 근거 가져오기의 결과도 그림에 넣지 않습니다.");
    }
    if work.job_starting > 0 {
        text.push_str(" 시작 응답을 받지 못한 AI 작업은 이미 시작됐을 수 있으니 작업 상태를 확인하세요.");
    }
    if work.jobs_running > 0 {
        text.push_str(" 실행 중인 AI 작업은 취소하지 않습니다.");
    }
    if work.pdf_sending > 0 {
        text.push_str(" 전송 중인 PDF 근거는 이미 작업물에 반영됐을 수 있으며, 되돌려지지 않습니다.");
    }
    if work.edit_sending > 0 {
        text.push_str(" 전송 중인 자료 값 변경은 이미 작업물에 반영됐을 수 있으며, 되돌려지지 않습니다.");
    }
    Some(text)
}

impl DesktopApp {
    /// The figure work a window close would leave behind, in words. `None` when there is none.
    pub(super) fn figure_close_warning(&self) -> Option<String> {
        describe_figure_work(&self.figure_work(true), "창을 닫으면")
    }

    /// The figure work a switch to another document would leave behind, in words. `None` when there is none. Only the
    /// open document's work counts: leaving it does not touch the jobs or the records kept aside for another document.
    pub(super) fn figure_switch_warning(&self) -> Option<String> {
        describe_figure_work(&self.figure_work(false), "작업물을 바꾸면")
    }

    /// The figure work the window holds. With `whole_session`, what is kept aside for other documents counts too (a close);
    /// without it, only the open document's work counts (a switch asks about what leaving it would lose).
    pub(super) fn figure_work(&self, whole_session: bool) -> FigureWork {
        let open = self.active.as_ref().map(|active| active.id.as_str());
        let mut work = FigureWork::default();
        for held in &self.held_pdf_evidence {
            // Leaving the open figure loses its own records only; the records held for another figure stay with that figure.
            if !whole_session && open != Some(held.target.artifact_id.as_str()) {
                continue;
            }
            if held.ticket.is_some() {
                work.pdf_sending += held.records.len();
            } else {
                work.pdf_pending += held.records.len();
            }
        }
        work.pdf_import = self.pdf_evidence_target.is_some();
        work.files = self.figure_ai_files().len();
        work.cell_draft = self.dataset_edit.as_ref().is_some_and(|edit| edit.draft != edit.original);
        work.csv_draft = self.csv_draft.is_some();
        // A start the window no longer waits for is still unanswered: the host may run its job, so it counts like a waited one.
        work.job_starting = self
            .pending
            .values()
            .filter(|pending| {
                matches!(&pending.action, PendingAction::StartJob | PendingAction::FigureAiJob { .. })
                    && pending.artifact_id.as_deref().is_some_and(|id| (whole_session || open == Some(id)) && self.is_figure_artifact(id))
            })
            .count()
            + self
                .late_starts
                .values()
                .filter(|id| (whole_session || open == Some(id.as_str())) && self.is_figure_artifact(id))
                .count();
        work.edit_sending = self
            .pending
            .values()
            .filter(|pending| {
                matches!(&pending.action, PendingAction::DatasetEdit)
                    && pending.artifact_id.as_deref().is_some_and(|id| (whole_session || open == Some(id)) && self.is_figure_artifact(id))
            })
            .count();
        work.jobs_running = self.figure_jobs(whole_session).into_iter().filter(|job| job_running(job)).count();
        work
    }

    /// The figure AI jobs a close or a switch would leave behind: with `whole_session`, the shown job and the jobs kept aside
    /// of every figure; without it, only the jobs of the open document, shown or kept aside. A document that is not a Figure
    /// has no figure jobs.
    fn figure_jobs(&self, whole_session: bool) -> Vec<&Job> {
        let open = self.active.as_ref().map(|active| active.id.as_str());
        self.job
            .iter()
            .chain(self.parked_jobs.iter())
            .filter(|job| self.is_figure_artifact(&job.artifact_id) && (whole_session || open == Some(job.artifact_id.as_str())))
            .collect()
    }

    /// Whether the artifact is a Figure the window knows: the open document, or one of the workspace's artifacts.
    fn is_figure_artifact(&self, id: &str) -> bool {
        self.active.as_ref().is_some_and(|active| active.id == id && active.kind == ArtifactKind::Figure)
            || self.artifacts.iter().any(|summary| summary.id == id && summary.kind == ArtifactKind::Figure)
    }

    /// Whether figure jobs are the only figure work left, and nothing else in the window needs an answer. Closing then
    /// discards nothing, so the close button only closes.
    pub(super) fn figure_only_job_remains(&self) -> bool {
        self.figure_work(true).only_jobs() && !self.has_non_figure_close_work()
    }

    /// Whether closing still needs an answer, not counting the figure jobs. Once the user has accepted a close, it goes
    /// through when only jobs remain: closing does not cancel them, and the warning has said so.
    pub(super) fn has_close_work_besides_figure_jobs(&self) -> bool {
        self.has_non_figure_close_work() || self.figure_work(true).holds_discardable()
    }

    /// Drops the figure work the user agreed to lose: the PDF records of the open figure not on their way, the PDF import in
    /// progress, the evidence files, the value under edit, and the AI request still on its way. A commit on its way stays until
    /// the host answers, and so do the records held for another figure. Returns the words the window shows after this when a
    /// job start was not answered, or a value sent was not answered: either may already be in the figure.
    pub(super) fn discard_figure_work(&mut self) -> Option<String> {
        let work = self.figure_work(false);
        let open = self.active.as_ref().map(|active| active.id.clone());
        self.held_pdf_evidence
            .retain(|held| held.ticket.is_some() || open.as_deref() != Some(held.target.artifact_id.as_str()));
        self.pdf_evidence_target = None;
        self.figure_ai_attachments = None;
        self.figure_ai_request = None;
        self.dataset_edit = None;
        self.csv_draft = None;
        // The user agreed to lose the open Figure's request and the value under edit: their drafts leave the file now.
        if let Some(active) = self.active.as_ref().filter(|active| active.kind == ArtifactKind::Figure) {
            let slot = super::drafts::figure_slot(&active.id);
            self.drafts.clear(&slot, super::drafts::Edit::Absent, Instant::now());
        }
        self.discard_cell_draft();
        self.ai_prompt.clear();
        self.ai_figure_refs.clear();
        let mut notes = Vec::new();
        if work.job_starting > 0 {
            notes.push(JOB_START_UNKNOWN);
        }
        if work.edit_sending > 0 {
            notes.push(EDIT_UNKNOWN);
        }
        (!notes.is_empty()).then(|| notes.join(" "))
    }

    /// Whether a switch to `summary` waits for the user's answer, because it loses something (`switch_loss`). The switch is
    /// kept in `pending_switch` until `draw_figure_switch_dialog` answers it.
    pub(super) fn ask_before_switch(&mut self, summary: &ArtifactSummary) -> bool {
        if self.switch_loss(summary).is_none() {
            return false;
        }
        self.pending_switch = Some(summary.clone());
        true
    }

    /// Whether `next` is another document than the open one.
    fn leaves_open_document(&self, next: &ArtifactSummary) -> bool {
        self.active.as_ref().is_some_and(|active| active.id != next.id)
    }

    /// What a switch to `next` would lose, in words; `None` when it loses nothing. Leaving the open document loses its figure
    /// work (`figure_switch_warning`). Opening the open document again loses the value under edit and the CSV draft, which
    /// `show_artifact` drops.
    pub(super) fn switch_loss(&self, next: &ArtifactSummary) -> Option<String> {
        if self.leaves_open_document(next) {
            return self.figure_switch_warning();
        }
        let work = FigureWork {
            cell_draft: self.dataset_edit.as_ref().is_some_and(|edit| edit.draft != edit.original),
            csv_draft: self.csv_draft.is_some(),
            ..FigureWork::default()
        };
        describe_figure_work(&work, "다시 열면")
    }

    /// The answer to the switch question. 전환 drops what the question named and then shows the other document; the note
    /// about a job start that was not answered comes after the switch, which clears the transient notices. 계속 작업 keeps the
    /// open document with everything held.
    pub(super) fn answer_pending_switch(&mut self, switch: bool) {
        let Some(summary) = self.pending_switch.take() else {
            return;
        };
        if !switch {
            return;
        }
        let note = if self.leaves_open_document(&summary) {
            self.discard_figure_work()
        } else {
            self.discard_cell_draft();
            None
        };
        self.show_artifact(summary);
        if let Some(message) = note {
            self.notices.warning(message, Instant::now());
        }
    }

    /// The question before the window leaves an open Figure that holds figure work of its own, or opens it again over a value
    /// under edit. 계속 작업 keeps the open document. 전환 discards what the question names, then shows the new document.
    /// Nothing is discarded before 전환.
    pub(super) fn draw_figure_switch_dialog(&mut self, ctx: &Context) {
        if self.close_confirmation_open {
            return;
        }
        let Some(target) = self.pending_switch.clone() else {
            return;
        };
        let Some(warning) = self.switch_loss(&target) else {
            // What the question was about is gone (a job ended, a commit landed): the switch is not made.
            self.pending_switch = None;
            return;
        };
        let (mut keep, mut switch) = (false, false);
        egui::Modal::new(Id::new("figure-switch-confirmation")).show(ctx, |ui| {
            ui.set_max_width(440.0);
            ui.heading("보류된 변경");
            ui.label(warning);
            ui.horizontal(|ui| {
                let keep_button = ui.button("계속 작업");
                crate::ui_record::note_human_only(ui, "계속 작업", &keep_button);
                keep = keep_button.clicked();
                let switch_button = ui.button("전환");
                crate::ui_record::note_human_only(ui, "전환", &switch_button);
                switch = switch_button.clicked();
            });
        });
        if keep {
            self.answer_pending_switch(false);
        }
        if switch {
            self.answer_pending_switch(true);
        }
    }

    /// The note for a switch that leaves a document other than a Figure with a job start unanswered: the job may have started.
    /// No question is asked about such a document, so the note comes after the switch (`show_artifact`). A Figure gets its note
    /// from `discard_figure_work`, after its own question. `None` when nothing of that kind is left.
    pub(super) fn unanswered_start_note(&self, next: &ArtifactSummary) -> Option<&'static str> {
        let left = self.active.as_ref().filter(|active| active.id != next.id && active.kind != ArtifactKind::Figure)?;
        let starting = self
            .pending
            .values()
            .any(|pending| matches!(&pending.action, PendingAction::StartJob) && pending.artifact_id.as_deref() == Some(left.id.as_str()));
        starting.then_some(JOB_START_UNKNOWN)
    }

    /// Stores a job the host started for the window. It is the newest job: the job of the open document is shown and polled,
    /// and the job it replaces stays kept aside while it runs (`place_jobs`). The next poll waits a full interval from now. A
    /// start answered late from its receipt, of a job the window already holds further on (a later attempt, or a later status of
    /// the same attempt: `older_status`), changes nothing.
    pub(super) fn record_started_job(&mut self, job: Job) {
        if self.job.iter().chain(&self.parked_jobs).any(|known| known.id == job.id && older_status(known, &job)) {
            return;
        }
        self.place_jobs(Some(job));
        self.last_job_poll = Instant::now();
    }

    /// Keeps the request a job start of this window was sent with, under the started job's id, for the job's label in the AI
    /// tab (`job_prompts`). The current text of the request box is not the job's request: the user may have typed since.
    pub(super) fn remember_job_prompt(&mut self, sent: &SentStart, result: &Result<RpcPayload, DltError>) {
        let Ok(RpcPayload::Json(job)) = result else { return };
        let (Some(job_id), Some(prompt)) = (job.get("id").and_then(Value::as_str), sent.content.get("prompt").and_then(Value::as_str)) else {
            return;
        };
        self.job_prompts.insert(job_id.to_owned(), prompt.to_owned());
    }

    /// The answer of a job start: the started job is recorded (`record_started_job`), an unreadable answer is reported.
    pub(super) fn job_started(&mut self, value: Value) {
        match serde_json::from_value::<Job>(value) {
            Ok(job) => self.record_started_job(job),
            Err(_) => self.set_error(String::from("AI 작업 상태를 읽지 못했습니다")),
        }
    }

    /// Stores the status the host reported for a job the window knows: the shown job, or a job kept aside, updated in place so
    /// that it keeps its place. A job the window does not know (it finished and was dropped, or its document is no longer
    /// listed) is ignored, so a late answer neither brings it back nor takes the place of the shown job. A running job kept
    /// aside takes the shown place once the shown job has ended (`place_jobs`).
    /// A status older than the one the window holds (a late `job.get` answer, of the same attempt or of one a resume replaced) is
    /// ignored.
    pub(super) fn update_job(&mut self, job: Job) {
        if let Some(shown) = self.job.as_ref().filter(|shown| shown.id == job.id) {
            if older_status(shown, &job) {
                return;
            }
            self.job = Some(job);
        } else if let Some(index) = self.parked_jobs.iter().position(|known| known.id == job.id) {
            if older_status(&self.parked_jobs[index], &job) {
                return;
            }
            if job_running(&job) {
                self.parked_jobs[index] = job;
            } else {
                self.parked_jobs.remove(index);
            }
        } else {
            return;
        }
        self.place_jobs(None);
    }

    /// Puts the known jobs in their place. The shown job is the newest running job of the open document, or else its newest
    /// job, so a job still running is not hidden behind one that has ended. Every other running job is kept aside
    /// (`parked_jobs`) for its status and warnings, and a finished one is dropped.
    pub(super) fn place_jobs(&mut self, started: Option<Job>) {
        let mut jobs = std::mem::take(&mut self.parked_jobs);
        jobs.extend(self.job.take());
        if let Some(job) = started {
            jobs.retain(|known| known.id != job.id);
            jobs.push(job);
        }
        let shown = self.active.as_ref().and_then(|active| {
            let of_open = |job: &Job| job.artifact_id == active.id;
            jobs.iter().rposition(|job| of_open(job) && job_running(job)).or_else(|| jobs.iter().rposition(|job| of_open(job)))
        });
        self.job = shown.map(|index| jobs.remove(index));
        jobs.retain(job_running);
        self.parked_jobs = jobs;
        let (shown, parked) = (&self.job, &self.parked_jobs);
        self.job_prompts.retain(|id, _| shown.iter().chain(parked).any(|job| job.id == *id));
    }

    /// Drops the jobs of the artifacts a workspace listing no longer has. Nothing can show or apply them, and their status
    /// polls would go on for good.
    pub(super) fn forget_unlisted_jobs(&mut self) {
        let artifacts = &self.artifacts;
        let listed = |job: &Job| artifacts.iter().any(|summary| summary.id == job.artifact_id);
        self.parked_jobs.retain(|job| listed(job));
        if self.job.as_ref().is_some_and(|job| !listed(job)) {
            self.job = None;
        }
    }

    /// Drops a job the host does not know (`job.get` answered not found), whether it is shown or kept aside: it can never
    /// finish, so its warnings and its polls go with it. A job kept aside that was shown meanwhile is not shown any more.
    pub(super) fn forget_job(&mut self, job_id: &str) {
        self.parked_jobs.retain(|job| job.id != job_id);
        if self.job.as_ref().is_some_and(|job| job.id == job_id) {
            self.job = None;
        }
        self.place_jobs(None);
    }

    /// Drops the PDF evidence held for a figure the workspace listing no longer has: no window can show it or put it back.
    pub(super) fn forget_unlisted_pdf_evidence(&mut self) {
        let artifacts = &self.artifacts;
        let listed = |id: &str| artifacts.iter().any(|summary| summary.id == id);
        self.held_pdf_evidence.retain(|held| listed(held.target.artifact_id.as_str()));
        if self.pdf_evidence_target.as_ref().is_some_and(|target| !listed(target.artifact_id.as_str())) {
            self.pdf_evidence_target = None;
        }
    }

    /// The answer of a job start the window no longer waits for: its document was left, or its wait timed out. The host may
    /// run the job, so a started job is recorded like any other (shown, or kept aside by its document). A refusal changes nothing.
    pub(super) fn late_job_start(&mut self, value: Value) {
        let Ok(job) = serde_json::from_value::<Job>(value) else {
            self.notices.warning(LATE_JOB_UNREADABLE, Instant::now());
            return;
        };
        self.record_started_job(job);
        self.notices.warning(LATE_JOB_START, Instant::now());
    }

    /// The answer of a job start the window no longer waits for (`late_starts`). A started job is recorded, an answer that leaves
    /// the outcome unknown says so, and a refusal changes nothing.
    pub(super) fn late_start_answered(&mut self, result: Result<RpcPayload, DltError>) {
        match result {
            Ok(RpcPayload::Json(value)) => self.late_job_start(value),
            Err(error) if super::messages::change_result_unknown(&error) => self.notices.warning(LATE_START_UNKNOWN, Instant::now()),
            _ => {}
        }
    }

    /// The answer of a job start the window sent. An answer that leaves the outcome unknown keeps the start for a repeat
    /// (`with_job_request_id`); any other answer ends the record of that start only. The records of other starts of the same
    /// document stay: their outcomes are still unknown. Unknown is judged as for the request's draft (`answer_sent_drafts`), so a
    /// start whose draft stays marked as maybe sent is repeated with the same id. A record dropped past `UNKNOWN_STARTS_MAX` is
    /// named to the user once the replies at hand are handled (`show_dropped_unknown_starts`): its repeat can make a second job.
    pub(super) fn settle_job_start(&mut self, sent: SentStart, result: &Result<RpcPayload, DltError>) {
        match result {
            Err(error) if super::messages::change_result_unknown(error) => {
                self.keep_unknown_start(sent);
            }
            _ => self.end_unknown_start(&sent.artifact_id, &sent.request_id),
        }
    }

    pub(super) fn keep_unknown_start(&mut self, sent: SentStart) {
        let artifacts = &self.artifacts;
        if let Some(dropped) = self.unknown_starts.keep(sent, |id| artifacts.iter().any(|item| item.id == id)) {
            let text = self.dropped_start_warning(&dropped);
            self.unknown_starts.dropped.push(text);
        }
    }

    /// Tells the user of the records dropped past `UNKNOWN_STARTS_MAX`. It runs after the replies at hand are handled, so the
    /// words of the reply that dropped a record do not cover them. The window shows one notice at a time: an error shown then
    /// stays, with these words after it, and keeps its origin (a later success of its method still takes it away).
    pub(super) fn show_dropped_unknown_starts(&mut self) {
        if self.unknown_starts.dropped.is_empty() {
            return;
        }
        let dropped = std::mem::take(&mut self.unknown_starts.dropped).join("\n");
        let now = Instant::now();
        let error = self
            .notices
            .current(now)
            .filter(|notice| notice.severity == Severity::Error)
            .map(|notice| (notice.text.clone(), notice.origin().map(str::to_owned)));
        match error {
            Some((error, origin)) => self.notices.error(format!("{error}\n{dropped}"), origin.as_deref(), now),
            None => self.notices.warning(dropped, now),
        }
    }

    /// The words for an unknown start whose record was dropped: its document and the start of its prompt.
    fn dropped_start_warning(&self, dropped: &SentStart) -> String {
        let document = self.artifacts.iter().find(|item| item.id == dropped.artifact_id).map_or(dropped.artifact_id.as_str(), |item| item.title.as_str());
        let prompt = dropped.content.get("prompt").and_then(Value::as_str).unwrap_or_default();
        let mut short: String = prompt.chars().take(20).collect();
        if prompt.chars().nth(20).is_some() {
            short.push('…');
        }
        format!(
            "결과를 확인하지 못한 AI 요청이 {UNKNOWN_STARTS_MAX}개를 넘어 '{document}'의 요청 \"{short}\"의 결과는 더 추적하지 않습니다. 이 요청을 다시 보내면 작업이 한 번 더 만들어질 수 있습니다"
        )
    }

    /// The params a job start goes out with, and the words to show once it has gone out. When the start asks again for an
    /// unanswered start (`asked_again_by`: the same draft and the same request apart from the base revision), that start is sent
    /// again as it was, with its id and its content: the host answers it from its receipt, so the repeat makes no second job, and
    /// an edit made since is not in it (`REPEAT_OF_UNKNOWN_START` when the params differ). A start of the same draft with other
    /// settings goes out as given, as a new request: the old one may still run (`UNKNOWN_START_MAY_RUN`), and its record stays.
    /// The record is only read here: a start that does not go out keeps it for the next try, and `end_unknown_start` ends it once
    /// the start has gone out.
    pub(super) fn with_job_request_id(&self, params: Value, draft: Option<&Body>) -> (Value, Option<&'static str>) {
        let Some(sent) = SentStart::of(&params) else {
            return (params, None);
        };
        if let Some(kept) = self.unknown_starts.asked_again(&sent, draft) {
            return (kept.params(), (kept.content != sent.content).then_some(REPEAT_OF_UNKNOWN_START));
        }
        let note = self.unknown_starts.same_draft_kept(&sent, draft).then_some(
            if draft.is_some() { UNKNOWN_START_MAY_RUN } else { BOARD_UNKNOWN_START_MAY_RUN },
        );
        (params, note)
    }

    /// Ends the record of an unanswered start of a document when a start with the record's id has gone out (the repeat; its own
    /// answer records it again if that answer is unknown too) or is answered. A start with another id (other content) leaves the
    /// records: those unanswered starts may still have made their jobs, and a repeat of each must carry its id.
    pub(super) fn end_unknown_start(&mut self, artifact_id: &str, request_id: &str) {
        self.unknown_starts.end(artifact_id, request_id);
    }

    /// A start has gone out with the params `with_job_request_id` gave: the record it repeated ends, and its words (`note`), if
    /// any, are shown.
    pub(super) fn unknown_start_sent(&mut self, artifact_id: &str, request_id: &str, note: Option<&'static str>) {
        self.end_unknown_start(artifact_id, request_id);
        if let Some(note) = note {
            self.notices.warning(note, Instant::now());
        }
    }

    /// Whether a job start of the document has no answer yet and the window no longer waits for it (`late_starts`).
    pub(super) fn late_start_waits(&self, artifact_id: &str) -> bool {
        self.late_starts.values().any(|id| id == artifact_id)
    }

    /// A switch to another document ends the wait for the job starts and evidence imports of the document left: the window no
    /// longer holds its requests back for them. A job start that is still answered is recorded (`late_job_start`); the answer of
    /// an import changes nothing. The figure AI request of the document left is ended.
    pub(super) fn release_waits_of_left_documents(&mut self, next_id: &str) {
        let left: Vec<u64> = self
            .pending
            .iter()
            .filter(|(_, pending)| pending.artifact_id.as_deref() != Some(next_id))
            .filter(|(_, pending)| {
                matches!(
                    &pending.action,
                    PendingAction::StartJob | PendingAction::FigureAiJob { .. } | PendingAction::FigureAiImport { .. }
                )
            })
            .map(|(ticket, _)| *ticket)
            .collect();
        for ticket in left {
            let Some(pending) = self.pending.remove(&ticket) else {
                continue;
            };
            self.awaited_starts.remove(&ticket);
            if matches!(&pending.action, PendingAction::StartJob | PendingAction::FigureAiJob { .. }) {
                self.late_starts.insert(ticket, pending.artifact_id.unwrap_or_default());
            }
        }
        if self.figure_ai_request.as_ref().is_some_and(|request| request.artifact_id.as_str() != next_id) {
            self.figure_ai_request = None;
        }
    }

    /// Stops waiting for a job start or an import that is not answered within `AWAITED_REPLY_LIMIT`, so that a lost answer or a
    /// stopped worker does not hold the figure's requests back for good. A job start answered later is still recorded.
    pub(super) fn expire_awaited_starts(&mut self) {
        let expired: Vec<u64> = self
            .awaited_starts
            .iter()
            .filter(|(_, sent)| sent.elapsed() >= AWAITED_REPLY_LIMIT)
            .map(|(ticket, _)| *ticket)
            .collect();
        for ticket in expired {
            self.awaited_starts.remove(&ticket);
            let Some(pending) = self.pending.remove(&ticket) else {
                continue;
            };
            match pending.action {
                PendingAction::StartJob => {
                    self.late_starts.insert(ticket, pending.artifact_id.unwrap_or_default());
                    self.notices.warning(START_TIMED_OUT, Instant::now());
                }
                PendingAction::FigureAiJob { token } => {
                    self.late_starts.insert(ticket, pending.artifact_id.unwrap_or_default());
                    self.end_figure_ai_request(&token);
                    self.notices.warning(START_TIMED_OUT, Instant::now());
                }
                PendingAction::FigureAiImport { token, .. } => {
                    // A request the window no longer holds (cancelled, or ended) is not reported: nothing waits for its import.
                    if self.figure_ai_request_holds(&token) {
                        self.end_figure_ai_request(&token);
                        self.set_error(IMPORT_TIMED_OUT.into());
                    }
                }
                _ => {}
            }
        }
    }

    /// Asks the host for the status of the running jobs kept aside. A job whose poll is still on its way is not asked again.
    /// The shown job is polled by `update` (app.rs).
    pub(super) fn request_parked_job_status(&mut self) {
        let polls: Vec<(String, String)> = self
            .parked_jobs
            .iter()
            .filter(|job| job_running(job))
            .filter(|job| {
                !self
                    .pending
                    .values()
                    .any(|pending| matches!(&pending.action, PendingAction::GetParkedJob { job_id } if *job_id == job.id))
            })
            .map(|job| (job.id.clone(), job.attempt_id.clone()))
            .collect();
        for (job_id, attempt_id) in polls {
            self.call(
                "job.get",
                json!({ "job_id": &job_id, "attempt_id": attempt_id }),
                PendingAction::GetParkedJob { job_id },
            );
        }
    }
}
