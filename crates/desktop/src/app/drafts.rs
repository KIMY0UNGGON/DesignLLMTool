//! Unsent AI requests and temporary editor drafts, kept in the workspace GUI folder (`.gui/drafts.json`, next to the
//! settings). A draft is written a moment after it changes, and again when a request is sent, answered or discarded. After a
//! crash or a closed window, the drafts in the file come back as recovered drafts: the window lists them, and the user
//! restores, copies or discards each one. Nothing is applied on its own.
//!
//! A request is written, marked with its request id, before it goes out. The mark stays until the answer. A draft still marked
//! after a restart may have been sent, so it is listed as such. An AI tab's request keeps its `job.start` params with the mark,
//! and the session of the window that marked it (`Draft::owner`). Once that window no longer runs, another window may restore
//! the draft into the same document at the same revision; the start is then kept for a repeat with the same id
//! (`UnknownStarts::recover`), so sending it again makes no second job. Any other marked draft is never restored into an editor
//! (only copied or discarded).
//! An accepted request's draft is removed; a refused one gives its content back to its editor. What an editor holds while a
//! request is on its way is a draft of its own, so two requests never clear each other.
//!
//! A draft is bound to the document and revision its content was last changed against. It is restored only into that
//! document at that revision, and only into an empty editor; anything else stays a copy-only draft. Files are kept as
//! paths and never copied into the file. Held PDF evidence keeps its reference and counts, not the text it read.
//!
//! Every window of the workspace writes the same file. A write holds the lock file `drafts.json.lock` while it reads the file
//! again, merges it and replaces it. Each window writes its own drafts. Recovery claims and discards check the owner under
//! the same lock. The file is read, and written, only up to `MAX_FILE_BYTES` (8 MiB); a larger file
//! is moved aside unread. A file that is not a draft file is moved aside too. Entries that do not parse
//! are skipped, and the file is copied aside before it is written again (once per content: a retried write does not copy the
//! same file again). A moved or copied file gets a name with a timestamp
//! that no existing file has, so an earlier `.unreadable` file is never overwritten.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use designllm_contracts::{new_id, ArtifactKind, CanvasBoardMemo, DltError, FigureRevisionRef};
use eframe::egui::{self, Align2, RichText};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::{
    home::{local_offset_minutes, modified_label, now_ms},
    messages::change_result_unknown,
    settings::write_atomic,
    truncate, DatasetCellEdit, DesktopApp, ImageCropStage, Screen,
};
use crate::{rpc::RpcPayload, theme};

/// The version of `drafts.json`. A file of a newer version is kept as it is and never written over.
const SCHEMA_VERSION: u64 = 1;
/// The largest `drafts.json` that is read (8 MiB). A larger file is moved aside without being read.
const MAX_FILE_BYTES: u64 = 8 * 1024 * 1024;
/// A change is written once it has been quiet this long, and at the latest this long after it first changed.
const QUIET: Duration = Duration::from_millis(500);
const LATEST: Duration = Duration::from_secs(3);
/// How long a write or an open waits for another window's lock of the draft file, and how often it asks again.
const LOCK_WAIT: Duration = Duration::from_millis(300);
const LOCK_RETRY: Duration = Duration::from_millis(20);
/// Why a write did not happen while another window held the draft file.
const LOCK_BUSY: &str = "다른 창이 초안 파일을 쓰고 있어 잠시 뒤 다시 저장합니다";
/// Why a recovered draft waits while its editor is in use.
const BUSY: &str = "현재 입력을 비우면 복원할 수 있습니다";
/// Why a draft that may have been sent is not restored.
const SENT_UNKNOWN: &str = "보냈을 수 있는 요청입니다. 결과를 알 수 없어 복원하지 않습니다. 복사하거나 버리세요";
const OTHER_OWNER: &str = "다른 창에서 사용 중인 초안입니다";
const DRAFT_CHANGED: &str = "초안이 변경되었거나 삭제되었습니다. 목록을 다시 확인하세요";
const NO_SESSION: &str = "초안의 소유자를 확인할 잠금을 얻지 못했습니다";
/// Why a recovered draft of the home request waits for the home screen.
const HOME_ONLY: &str = "홈 화면에서 복원할 수 있습니다";
const HOME_REFUSED: &str = "홈 요청을 보내지 못해 요청문과 첨부를 되돌렸습니다";
const HOME_UNKNOWN: &str = "홈 요청의 결과를 확인하지 못했습니다. 작업이 시작됐을 수 있어 되돌리지 않았습니다. 초안 목록에서 복사하거나 버리세요";
const CELL_UNKNOWN: &str = "자료 값 변경의 결과를 확인하지 못했습니다. 이미 반영됐을 수 있으니 작업물을 열어 확인하세요";

/// What an editor holds. Files appear as paths only.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(super) enum Body {
    /// The request box of the home screen: the prompt, its files and the figure references it carries.
    Home { prompt: String, attachments: Vec<String>, figure_refs: Vec<FigureRevisionRef> },
    /// The AI tab of an open Figure: the prompt, its evidence files and the figure references it carries.
    Figure { prompt: String, attachments: Vec<String>, figure_refs: Vec<FigureRevisionRef> },
    /// The AI tab of an open document that is not a Figure (a canvas, slides, a text document, a web project): the prompt and
    /// the figure references it carries.
    Ai { prompt: String, figure_refs: Vec<FigureRevisionRef> },
    /// A board memo or title being written, on the page it was made for.
    Note { memo: CanvasBoardMemo, page_index: u32, is_new: bool },
    /// The crop fields typed for one image node.
    Crop { node_id: String, asset_sha256: String, fields: [String; 4] },
    /// One table cell being edited, against the dataset version it was read from.
    Cell { dataset_id: String, version_hash: String, row_id: String, column_id: String, original: String, draft: String },
    /// Text typed into a node that the host has not confirmed yet, with the last confirmed text.
    Inline { node_id: String, page_index: u32, text: String, committed: String },
    /// Evidence a PDF page read produced that the figure does not hold yet: where it came from, not what it says.
    PdfEvidence { page_index: u32, file_name: String, asset_sha256: Option<String>, records: usize, message: String },
}

/// What an editor holds now, with the document and revision it belongs to (both None on the home screen).
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Live {
    pub artifact_id: Option<String>,
    pub revision: Option<u64>,
    pub body: Body,
}

/// The state of one slot's editor at a sync.
pub(super) enum Edit {
    /// The editor is not shown or is busy: its draft stays as it is.
    Absent,
    /// The editor holds nothing to keep.
    Empty,
    /// The editor holds this.
    Has(Live),
}

/// One saved draft. `key` is unique in the file; `slot` names the editor it belongs to while that editor holds it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(super) struct Draft {
    pub key: String,
    pub slot: String,
    pub artifact_id: Option<String>,
    pub revision: Option<u64>,
    /// Unix milliseconds of the last change of the content.
    pub saved_at_ms: u64,
    pub body: Body,
    /// The id of the request sent from this content, until its answer comes. A draft still marked after a restart may have
    /// been sent (`SENT_UNKNOWN`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sent: Option<String>,
    /// The `job.start` params a request from this content went out with, its request id among them, without the prompt when it is
    /// the draft's own (`marked_start` puts it back). While that id is the one in `sent`, a window that restores the draft after
    /// a restart sends the start again as it was, so the host answers it from its receipt (`DesktopApp::restore_draft`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start: Option<Value>,
    /// The session editing or sending this draft. Older files can contain ownerless drafts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
}

impl Body {
    /// The text of a request the draft holds (the home box or an AI tab).
    fn request_prompt(&self) -> Option<&str> {
        match self {
            Body::Home { prompt, .. } | Body::Figure { prompt, .. } | Body::Ai { prompt, .. } => Some(prompt),
            _ => None,
        }
    }
}

impl Draft {
    /// Whether the draft holds the start params of the request that marks it (`sent`).
    fn marks_start(&self) -> bool {
        self.sent.is_some() && self.start.as_ref().and_then(|start| start.get("request_id")).and_then(Value::as_str) == self.sent.as_deref()
    }

    /// The start params of the request that marks the draft, as they went out: the prompt is the draft's own, as it was sent
    /// (without its outer spaces, `start_ai_job`, `figure_ai_attach.rs`).
    fn marked_start(&self) -> Option<Value> {
        if !self.marks_start() {
            return None;
        }
        let mut start = self.start.clone()?;
        if let (Some(fields), Some(prompt)) = (start.as_object_mut(), self.body.request_prompt()) {
            fields.entry("prompt").or_insert_with(|| json!(prompt.trim()));
        }
        Some(start)
    }
}

/// The start params a draft keeps for `body`: the prompt is left out when it is the draft's own (`Draft::marked_start`).
fn kept_start(mut start: Value, body: &Body) -> Value {
    let own = body.request_prompt().map(str::trim);
    if let Some(fields) = start.as_object_mut() {
        if own.is_some() && fields.get("prompt").and_then(Value::as_str) == own {
            fields.remove("prompt");
        }
    }
    start
}

/// The lock file a window holds while it runs (`.gui/drafts-sessions/<id>.lock`), so that another window can tell whether the
/// requests marked with its id (`Draft::owner`) are still its own. The system lets the lock go when the process ends, however
/// it ends; a window that closes removes the file too.
struct Session {
    id: String,
    path: PathBuf,
    file: Option<std::fs::File>,
    /// Test only: the file stays when the session ends, as after a crash.
    keep_file: bool,
}

impl Drop for Session {
    fn drop(&mut self) {
        self.file = None;
        if !self.keep_file {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// The folder of the session lock files beside the draft file `path`.
fn sessions_dir(path: &Path) -> PathBuf {
    path.with_file_name("drafts-sessions")
}

/// Called under the drafts lock so a sweep cannot see the new file before its session locks it.
fn start_session(path: &Path) -> Result<Session, String> {
    let dir = sessions_dir(path);
    std::fs::create_dir_all(&dir).map_err(|error| error.to_string())?;
    let id = new_id();
    let lock_path = dir.join(format!("{id}.lock"));
    let file = std::fs::File::create_new(&lock_path).map_err(|error| error.to_string())?;
    file.try_lock().map_err(|error| match error {
        std::fs::TryLockError::WouldBlock => OTHER_OWNER.to_owned(),
        std::fs::TryLockError::Error(reason) => format!("{NO_SESSION}: {reason}"),
    })?;
    Ok(Session { id, path: lock_path, file: Some(file), keep_file: false })
}

/// An inaccessible lock counts as live. Only a missing file or a successful exclusive lock proves the owner is gone.
fn session_alive(path: &Path, id: &str) -> bool {
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return true;
    }
    let lock_path = sessions_dir(path).join(format!("{id}.lock"));
    let file = match std::fs::OpenOptions::new().read(true).write(true).open(&lock_path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return false,
        Err(_) => return true,
    };
    file.try_lock().is_err()
}

fn clean_sessions(path: &Path) {
    let Ok(entries) = std::fs::read_dir(sessions_dir(path)) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if let Some(id) = name.to_str().and_then(|name| name.strip_suffix(".lock")) {
            if entry.file_type().is_ok_and(|ty| ty.is_file()) && !session_alive(path, id) {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
}

#[derive(Serialize)]
struct DraftFile {
    schema_version: u64,
    drafts: Vec<Draft>,
}

enum ReadError {
    /// A file of a newer version.
    Newer,
    /// Not a draft file of any version.
    Unreadable,
    /// Larger than `MAX_FILE_BYTES`; it is not read.
    TooLarge,
    Io(String),
}

/// What `drafts.json` holds, read entry by entry: an entry that does not parse is counted and left out.
struct Loaded {
    drafts: Vec<Draft>,
    skipped: usize,
    /// A hash of the text read, so that a file already copied aside is not copied again (`DraftBook::kept_aside`).
    fingerprint: u64,
    /// Whether a file with a list of drafts was read. A missing file says nothing about what another window removed.
    present: bool,
}

/// The outcome of sending a request from a draft.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Outcome {
    /// A local guard blocked `job.start` or the window could not enqueue it for the worker.
    NotSent,
    /// The host took the request.
    Accepted,
    /// The host refused the request, so nothing was changed.
    Refused,
    /// The answer is lost or unclear: the request may have been received.
    Unknown,
}

/// A request sent from a draft, until its answer: the draft's key, the id it went out with, and the mark the draft carried before it
/// (an earlier request that is still unanswered, or none) with the start params the draft kept for it (`Draft::start`).
struct SentRequest {
    key: String,
    request_id: String,
    before: Option<String>,
    before_start: Option<Value>,
}

/// The drafts of the workspace window: the file's drafts, and which editor slot holds each live one.
#[derive(Default)]
pub(super) struct DraftBook {
    /// `.gui/drafts.json` of the workspace; None without a workspace, where nothing is written.
    path: Option<PathBuf>,
    drafts: BTreeMap<String, Draft>,
    /// Drafts that came back from the file and wait for the user's decision.
    recovered_keys: BTreeSet<String>,
    last_recovery_read: Option<Instant>,
    /// The draft each slot's editor holds, while it holds content.
    owned: BTreeMap<String, String>,
    /// What each slot held at its last sync (None: nothing), so an unchanged editor costs nothing.
    seen: BTreeMap<String, Option<Body>>,
    /// The request sent under each ticket, until its answer (`begin_send`).
    sent: BTreeMap<u64, SentRequest>,
    /// Pending removals. An accepted request id also authorizes removal of its matching mark.
    removed: BTreeMap<String, Option<String>>,
    /// The first change not yet written, and the last one.
    unsaved_since: Option<Instant>,
    last_change: Option<Instant>,
    /// A file of a newer version, or one that could not be read: drafts are not written.
    read_only: bool,
    /// The last write failed; reported once per run of failures.
    failing: bool,
    /// The fingerprint of the file with skipped entries that was last copied aside. A write that reads the same text again
    /// (a retry after a failed write) does not copy it once more.
    kept_aside: Option<u64>,
    notices: Vec<String>,
    /// The lock this window holds from its first write or recovery claim.
    session: Option<Session>,
    /// Unwritten edits. Only locally created drafts can be dirty before they have a session id.
    dirty: BTreeSet<String>,
    /// A crash-recovered mark this window has claimed and has not itself seen an unknown answer for.
    restored: BTreeMap<String, String>,
}

/// Where a recovered draft's document stands.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Placement {
    /// Open, at the draft's revision.
    Here,
    /// Open or listed at another revision.
    Stale,
    /// Listed at the draft's revision, not open.
    NotOpen,
    /// Not in the workspace list (or the list is not read yet).
    Missing,
}

/// Where a draft's document stands, given the open document and the workspace list.
pub(super) fn placement(artifact_id: &str, revision: u64, open: Option<(&str, u64)>, listed: Option<u64>) -> Placement {
    match (open, listed) {
        (Some((id, current)), _) if id == artifact_id => {
            if current == revision { Placement::Here } else { Placement::Stale }
        }
        (_, Some(current)) => {
            if current == revision { Placement::NotOpen } else { Placement::Stale }
        }
        _ => Placement::Missing,
    }
}

/// Whether a recovered draft may be restored now, what stands in the way, and the document to open for it.
pub(super) struct Standing {
    pub ready: bool,
    pub reason: Option<&'static str>,
    pub open: Option<String>,
}

impl Standing {
    fn ready() -> Self {
        Self { ready: true, reason: None, open: None }
    }

    fn blocked(reason: &'static str) -> Self {
        Self { ready: false, reason: Some(reason), open: None }
    }
}

/// Where the drafts of `workspace` are kept.
pub(super) fn drafts_path(workspace: &Path) -> PathBuf {
    workspace.join(".gui").join("drafts.json")
}

/// The draft slot of the AI tab of the Figure `artifact_id`.
pub(super) fn figure_slot(artifact_id: &str) -> String {
    format!("figure:{artifact_id}")
}

/// The draft slot of the AI tab of the document `artifact_id` of `kind`: a Figure's (`figure_slot`), or another document's.
pub(super) fn ai_tab_slot(kind: ArtifactKind, artifact_id: &str) -> String {
    if kind == ArtifactKind::Figure { figure_slot(artifact_id) } else { format!("ai:{artifact_id}") }
}

/// The content of one table cell edit, as a draft holds it.
pub(super) fn cell_body(edit: &DatasetCellEdit) -> Body {
    Body::Cell {
        dataset_id: edit.dataset_id.clone(),
        version_hash: edit.version_hash.clone(),
        row_id: edit.row_id.clone(),
        column_id: edit.column_id.clone(),
        original: edit.original.clone(),
        draft: edit.draft.clone(),
    }
}

fn read_file(path: &Path) -> Result<Loaded, ReadError> {
    let empty = Loaded { drafts: Vec::new(), skipped: 0, fingerprint: 0, present: false };
    let size = match std::fs::metadata(path) {
        Ok(metadata) => metadata.len(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(empty),
        Err(error) => return Err(ReadError::Io(error.to_string())),
    };
    if size > MAX_FILE_BYTES {
        return Err(ReadError::TooLarge);
    }
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(empty),
        Err(error) => return Err(ReadError::Io(error.to_string())),
    };
    let value: Value = serde_json::from_str(&text).map_err(|_| ReadError::Unreadable)?;
    match value.get("schema_version").and_then(Value::as_u64) {
        Some(SCHEMA_VERSION) => {}
        Some(version) if version > SCHEMA_VERSION => return Err(ReadError::Newer),
        _ => return Err(ReadError::Unreadable),
    }
    let entries = match value.get("drafts") {
        None => return Ok(empty),
        Some(Value::Array(entries)) => entries,
        Some(_) => return Err(ReadError::Unreadable),
    };
    let mut hasher = std::hash::DefaultHasher::new();
    std::hash::Hash::hash(&text, &mut hasher);
    let fingerprint = std::hash::Hasher::finish(&hasher);
    let mut loaded = Loaded { drafts: Vec::with_capacity(entries.len()), skipped: 0, fingerprint, present: true };
    for entry in entries {
        match Draft::deserialize(entry) {
            Ok(draft) => loaded.drafts.push(draft),
            Err(_) => loaded.skipped += 1,
        }
    }
    Ok(loaded)
}

/// A name beside `path` for a file moved or copied aside at `stamp` that no file has yet.
pub(super) fn quarantine_path(path: &Path, stamp: u64) -> PathBuf {
    let name = path.file_name().map_or_else(String::new, |name| name.to_string_lossy().into_owned());
    let mut candidate = path.with_file_name(format!("{name}.unreadable-{stamp}"));
    let mut number = 1;
    while candidate.exists() {
        candidate = path.with_file_name(format!("{name}.unreadable-{stamp}-{number}"));
        number += 1;
    }
    candidate
}

#[cfg(test)]
thread_local! {
    static FAIL_MOVE_ASIDE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static FAIL_COPY_ASIDE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static FAIL_REPLACE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Test only: makes `move_aside`, `copy_aside` or the replacing write fail on this thread while the guard lives; dropping it
/// lifts the failure.
#[cfg(test)]
pub(super) struct FailingAside(AsideKind);

#[cfg(test)]
#[derive(Clone, Copy)]
pub(super) enum AsideKind {
    Move,
    Copy,
    /// The write of the merged file (`write_atomic`).
    Replace,
}

#[cfg(test)]
impl FailingAside {
    pub(super) fn new(kind: AsideKind) -> Self {
        Self::set(kind, true);
        Self(kind)
    }

    fn set(kind: AsideKind, on: bool) {
        match kind {
            AsideKind::Move => FAIL_MOVE_ASIDE.with(|flag| flag.set(on)),
            AsideKind::Copy => FAIL_COPY_ASIDE.with(|flag| flag.set(on)),
            AsideKind::Replace => FAIL_REPLACE.with(|flag| flag.set(on)),
        }
    }
}

#[cfg(test)]
impl Drop for FailingAside {
    fn drop(&mut self) {
        Self::set(self.0, false);
    }
}

/// Moves a file that cannot be read aside, so that nothing is written over it. False when it stays where it is.
fn move_aside(path: &Path) -> bool {
    #[cfg(test)]
    if FAIL_MOVE_ASIDE.with(std::cell::Cell::get) {
        return false;
    }
    std::fs::rename(path, quarantine_path(path, now_ms())).is_ok()
}

fn copy_aside(path: &Path) -> bool {
    #[cfg(test)]
    if FAIL_COPY_ASIDE.with(std::cell::Cell::get) {
        return false;
    }
    std::fs::copy(path, quarantine_path(path, now_ms())).is_ok()
}

/// Holds the lock file beside `path` (`drafts.json.lock`, never removed) for one read, merge and replace, so that two windows
/// or threads writing the same file take turns and neither drops what the other merged. The lock goes with the returned
/// handle, on every return. A lock held elsewhere for longer than `LOCK_WAIT` is an error (`LOCK_BUSY`). A file system
/// without locks returns no handle.
pub(super) fn lock_drafts(path: &Path) -> Result<Option<std::fs::File>, String> {
    let name = path.file_name().map_or_else(String::new, |name| name.to_string_lossy().into_owned());
    let lock_path = path.with_file_name(format!("{name}.lock"));
    if let Some(parent) = lock_path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|error| error.to_string())?;
    // The window's thread asks, so it waits a bounded time: a window that holds the lock longer (one that hangs) makes this
    // write fail, and the drafts stay in memory for the next flush.
    let started = Instant::now();
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(Some(file)),
            Err(std::fs::TryLockError::WouldBlock) if started.elapsed() < LOCK_WAIT => std::thread::sleep(LOCK_RETRY),
            Err(std::fs::TryLockError::WouldBlock) => return Err(LOCK_BUSY.into()),
            Err(std::fs::TryLockError::Error(error)) if error.kind() == std::io::ErrorKind::Unsupported => return Ok(None),
            Err(std::fs::TryLockError::Error(error)) => return Err(error.to_string()),
        }
    }
}

/// Whether nothing is at `path` any more: another window moved or replaced the file between its read and the move here.
fn gone(path: &Path) -> bool {
    matches!(std::fs::symlink_metadata(path), Err(error) if error.kind() == std::io::ErrorKind::NotFound)
}

/// The paths that still name a file, and the names of those that are gone.
pub(super) fn split_files(paths: &[String]) -> (Vec<String>, Vec<String>) {
    let mut present = Vec::new();
    let mut missing = Vec::new();
    for path in paths {
        if Path::new(path).is_file() {
            present.push(path.clone());
        } else {
            missing.push(file_name(path));
        }
    }
    (present, missing)
}

fn file_name(path: &str) -> String {
    Path::new(path).file_name().map_or_else(|| path.to_owned(), |name| name.to_string_lossy().into_owned())
}

fn missing_files(body: &Body) -> Vec<String> {
    match body {
        Body::Home { attachments, .. } | Body::Figure { attachments, .. } => split_files(attachments).1,
        _ => Vec::new(),
    }
}

fn kind_label(body: &Body) -> &'static str {
    match body {
        Body::Home { .. } => "홈 요청",
        Body::Figure { .. } => "그림 AI 요청",
        Body::Ai { .. } => "AI 요청",
        Body::Note { .. } => "보드 메모",
        Body::Crop { .. } => "이미지 자르기",
        Body::Cell { .. } => "표 셀",
        Body::Inline { .. } => "텍스트 입력",
        Body::PdfEvidence { .. } => "PDF 근거",
    }
}

fn preview(body: &Body) -> String {
    let text = match body {
        Body::Home { prompt, attachments, .. } | Body::Figure { prompt, attachments, .. } => {
            if prompt.trim().is_empty() { format!("첨부 {}개", attachments.len()) } else { prompt.clone() }
        }
        Body::Ai { prompt, .. } => prompt.clone(),
        Body::Note { memo, .. } => memo.text.clone(),
        Body::Crop { fields, .. } => fields.join(", "),
        Body::Cell { original, draft, .. } => format!("{original} → {draft}"),
        Body::Inline { text, .. } => text.clone(),
        Body::PdfEvidence { file_name, page_index, records, .. } => {
            format!("{file_name} · {}쪽 · 근거 {records}개", page_index + 1)
        }
    };
    truncate(text.lines().next().unwrap_or_default(), 80)
}

/// What the copy button puts on the clipboard: the text of the draft.
fn copy_text(body: &Body) -> String {
    match body {
        Body::Home { prompt, .. } | Body::Figure { prompt, .. } | Body::Ai { prompt, .. } => prompt.clone(),
        Body::Note { memo, .. } => memo.text.clone(),
        Body::Crop { fields, .. } => fields.join(" "),
        Body::Cell { draft, .. } => draft.clone(),
        Body::Inline { text, .. } => text.clone(),
        Body::PdfEvidence { file_name, page_index, records, .. } => {
            format!("{file_name} · {}쪽 · 근거 {records}개", page_index + 1)
        }
    }
}

/// Whether a draft holds the request or text `sent` carries. An AI tab's prompt is compared without its outer spaces, as the
/// request is sent (`figure_ai_attach.rs`, `start_ai_job`).
pub(super) fn same_content(sent: &Body, held: &Body) -> bool {
    match (sent, held) {
        (
            Body::Figure { prompt: a, attachments: files_a, figure_refs: refs_a },
            Body::Figure { prompt: b, attachments: files_b, figure_refs: refs_b },
        ) => a.trim() == b.trim() && files_a == files_b && refs_a == refs_b,
        (Body::Ai { prompt: a, figure_refs: refs_a }, Body::Ai { prompt: b, figure_refs: refs_b }) => a.trim() == b.trim() && refs_a == refs_b,
        _ => sent == held,
    }
}

/// Two texts one after the other, the first one only when both have text.
fn join_text(first: &str, second: &str) -> String {
    match (first.trim().is_empty(), second.trim().is_empty()) {
        (true, _) => second.to_owned(),
        (false, true) => first.to_owned(),
        (false, false) => format!("{first}\n{second}"),
    }
}

impl DraftBook {
    /// The drafts the workspace's file holds, all of them recovered. A file that cannot be read is moved aside, and a file
    /// of a newer version is left alone; in both cases nothing is written over it.
    pub(super) fn open(path: PathBuf) -> Self {
        let mut book = Self { path: Some(path.clone()), ..Self::default() };
        // The read and what it moves or copies aside go under the lock a write holds, when there is a folder to hold it in (a
        // workspace without one has no draft file). A lock not had in time leaves the open as it was before locks.
        let _lock = if path.parent().is_some_and(Path::is_dir) { lock_drafts(&path).ok().flatten() } else { None };
        if _lock.is_some() {
            clean_sessions(&path);
        }
        match read_file(&path) {
            Ok(loaded) => {
                if loaded.skipped > 0 {
                    let kept = copy_aside(&path);
                    if kept {
                        book.kept_aside = Some(loaded.fingerprint);
                    }
                    let note = if kept { ". 원본 파일은 옆에 남겼습니다" } else { "" };
                    book.notices.push(format!("초안 {}개를 읽지 못해 건너뛰었습니다{note}", loaded.skipped));
                }
                for draft in loaded.drafts {
                    book.recovered_keys.insert(draft.key.clone());
                    book.drafts.insert(draft.key.clone(), draft);
                }
            }
            Err(ReadError::Newer) => {
                book.read_only = true;
                book.notices.push("더 새 버전의 초안 파일이어서 초안을 저장하지 않습니다".into());
            }
            Err(ReadError::Unreadable) => book.move_unreadable(&path, "초안 파일을 읽지 못해 옆으로 옮겼습니다"),
            Err(ReadError::TooLarge) => book.move_unreadable(&path, "초안 파일이 8 MiB를 넘어 읽지 않고 옆으로 옮겼습니다"),
            Err(ReadError::Io(message)) => {
                book.read_only = true;
                book.notices.push(format!("초안 파일을 읽지 못했습니다. 저장하지 않습니다: {message}"));
            }
        }
        book
    }

    /// Open has no retry: a file that cannot be moved is read-only for this window, with the reason in the notice.
    fn move_unreadable(&mut self, path: &Path, words: &str) {
        if move_aside(path) {
            self.notices.push(words.into());
        } else if gone(path) {
            // Another window moved it first; there is nothing left to keep from being written over.
        } else {
            self.read_only = true;
            self.notices.push("초안 파일을 읽지 못했습니다. 저장하지 않습니다".into());
        }
    }

    pub(super) fn take_notices(&mut self) -> Vec<String> {
        std::mem::take(&mut self.notices)
    }

    /// The recovered drafts, newest first.
    pub(super) fn recovered(&self) -> Vec<Draft> {
        let mut drafts: Vec<Draft> = self.recovered_keys.iter().filter_map(|key| self.drafts.get(key)).cloned().collect();
        drafts.sort_by(|a, b| b.saved_at_ms.cmp(&a.saved_at_ms));
        drafts
    }

    /// Whether the editor of a slot holds the draft `key` (the draft is then the editor's, not a recovered one).
    pub(super) fn held_by_editor(&self, key: &str) -> bool {
        self.owner_of(key).is_some()
    }

    fn owns(&self, draft: &Draft) -> bool {
        self.session.as_ref().is_some_and(|session| draft.owner.as_ref() == Some(&session.id))
    }

    fn owned_elsewhere(&self, draft: &Draft) -> bool {
        !self.owns(draft) && draft.owner.as_deref().is_some_and(|id| self.path.as_deref().is_none_or(|path| session_alive(path, id)))
    }

    pub(super) fn restored_mark(&self, slot: &str) -> Option<&str> {
        let key = self.owned.get(slot)?;
        let id = self.restored.get(key)?;
        (self.drafts.get(key)?.sent.as_ref() == Some(id)).then_some(id.as_str())
    }

    /// Recheck the session lock even when the draft's bytes have not changed since open.
    fn recovery_block(&self, draft: &Draft) -> Option<&'static str> {
        if self.owned_elsewhere(draft) {
            return Some(OTHER_OWNER);
        }
        if let Some(id) = &draft.sent {
            let local = self.owns(draft) || (draft.owner.is_none() && self.dirty.contains(&draft.key));
            if !draft.marks_start() || (local && self.restored.get(&draft.key) != Some(id)) {
                return Some(SENT_UNKNOWN);
            }
            let start = draft.start.as_ref()?;
            if draft.artifact_id.is_none() || start.get("artifact_id").and_then(Value::as_str) != draft.artifact_id.as_deref()
                || start.get("target_ids").is_some_and(|ids| !ids.as_array().is_some_and(|ids| ids.iter().all(Value::is_string)))
            {
                return Some(SENT_UNKNOWN);
            }
        }
        None
    }

    /// A display snapshot. Claims and discards check the current row again under the lock.
    pub(super) fn refresh(&mut self) {
        if let Some(Ok(loaded)) = self.path.as_deref().map(read_file) {
            self.reconcile(&loaded);
        }
    }

    fn session_id(&mut self, path: &Path) -> Result<&str, String> {
        if self.session.is_none() {
            self.session = Some(start_session(path)?);
        }
        self.session.as_ref().map(|session| session.id.as_str()).ok_or_else(|| NO_SESSION.to_owned())
    }

    /// Test only: the window's lock goes as when its process is killed: the lock file stays, held by nobody.
    #[cfg(test)]
    pub(super) fn end_session_as_crash(&mut self) {
        if let Some(mut session) = self.session.take() {
            session.keep_file = true;
        }
    }

    /// Keep new keys and the owner's unwritten edits or pending sends. A non-owner follows disk even for a dirty copy.
    /// Other missing rows stay deleted: removal required the owner or acceptance of the matching request id.
    fn reconcile(&mut self, loaded: &Loaded) {
        if !loaded.present {
            return;
        }
        let on_file: BTreeMap<&str, &Draft> = loaded.drafts.iter().map(|draft| (draft.key.as_str(), draft)).collect();
        let keys: Vec<_> = self.drafts.keys().cloned().collect();
        for key in keys {
            let disk = on_file.get(key.as_str()).copied();
            let local = &self.drafts[&key];
            let mine = self.owns(local);
            if disk.is_none() {
                let unsaved = self.dirty.contains(&key) && (mine || local.owner.is_none());
                if unsaved || (mine && self.sent.values().any(|request| request.key == key)) {
                    continue;
                }
            }
            if mine && disk.is_some_and(|draft| self.owns(draft)) {
                continue;
            }
            if disk == Some(local) {
                continue;
            }
            if let Some(slot) = self.owner_of(&key) {
                // Keep the last editor snapshot: a passive sync must not recreate the owner's deleted draft under a fresh key.
                self.owned.remove(&slot);
            }
            self.restored.remove(&key);
            self.dirty.remove(&key);
            if let Some(draft) = disk {
                self.recovered_keys.insert(key.clone());
                self.drafts.insert(key, draft.clone());
            } else {
                self.drafts.remove(&key);
                self.recovered_keys.remove(&key);
            }
        }
    }

    /// Brings a slot's draft up to what its editor holds. An absent editor leaves its draft alone.
    pub(super) fn sync(&mut self, slot: &str, edit: Edit, now: Instant) {
        let current = match edit {
            Edit::Absent => return,
            Edit::Empty => None,
            Edit::Has(live) => Some(live),
        };
        let body = current.as_ref().map(|live| live.body.clone());
        if self.seen.get(slot) == Some(&body) {
            return;
        }
        self.seen.insert(slot.to_owned(), body);
        self.store(slot, current, now);
    }

    /// Syncs the slots with a prefix together: a slot whose item is gone is emptied.
    pub(super) fn sync_group(&mut self, prefix: &str, items: Vec<(String, Live)>, now: Instant) {
        let present: BTreeSet<String> = items.iter().map(|(slot, _)| slot.clone()).collect();
        let gone: Vec<String> = self
            .owned
            .keys()
            .filter(|slot| slot.starts_with(prefix) && !present.contains(slot.as_str()))
            .cloned()
            .collect();
        for slot in gone {
            self.sync(&slot, Edit::Empty, now);
        }
        for (slot, live) in items {
            self.sync(&slot, Edit::Has(live), now);
        }
    }

    /// The draft of a slot is done (discarded, or its content is kept elsewhere): it is removed and written now.
    /// What the editor holds is not saved again unless it changes.
    pub(super) fn clear(&mut self, slot: &str, edit: Edit, now: Instant) {
        if let Some(key) = self.owned.get(slot).cloned() {
            self.forget_key(&key);
            self.changed(now);
        }
        let body = match edit {
            Edit::Has(live) => Some(live.body),
            _ => None,
        };
        self.seen.insert(slot.to_owned(), body);
        self.flush();
    }

    /// Claim into `slot`, or discard. The displayed row, its owner and the atomic replacement are checked under one lock.
    fn change_recovered(&mut self, expected: &Draft, slot: Option<&str>) -> Result<(), String> {
        let key = &expected.key;
        let path = self.path.clone().filter(|_| !self.read_only).ok_or(NO_SESSION)?;
        if !self.recovered_keys.contains(key) || slot.is_some_and(|slot| self.owned.contains_key(slot)) {
            return Err(DRAFT_CHANGED.into());
        }
        if self.unsaved_since.is_some() {
            self.write_report()?;
        }
        let _lock = lock_drafts(&path)?.ok_or(NO_SESSION)?;
        let read_current = || read_file(&path).map_err(|error| match error {
            ReadError::Io(message) => message,
            ReadError::Newer => "초안 파일 버전이 바뀌었습니다".into(),
            ReadError::Unreadable | ReadError::TooLarge => "초안 파일을 읽을 수 없습니다".into(),
        });
        let mut loaded = read_current()?;
        self.reconcile(&loaded);
        let index = loaded.drafts.iter().position(|draft| draft == expected).ok_or(DRAFT_CHANGED)?;
        if self.owned_elsewhere(expected) {
            return Err(OTHER_OWNER.into());
        }
        self.keep_skipped(&path, &loaded)?;
        if let Some(slot) = slot {
            if let Some(reason) = self.recovery_block(expected) {
                return Err(reason.into());
            }
            let draft = &mut loaded.drafts[index];
            draft.owner = Some(self.session_id(&path)?.to_owned());
            slot.clone_into(&mut draft.slot);
            let claimed = draft.clone();
            write_drafts(&path, loaded.drafts)?;
            // A successful rename belongs to us even if verification fails. Leave it on the recovery list until verified.
            if let Some(id) = &claimed.sent {
                self.restored.insert(key.clone(), id.clone());
            }
            self.drafts.insert(key.clone(), claimed);
            let verified = read_current()?;
            if verified.drafts.iter().find(|draft| &draft.key == key) != self.drafts.get(key) {
                return Err("저장한 초안의 소유자를 확인하지 못했습니다".into());
            }
            self.recovered_keys.remove(key);
            self.owned.insert(slot.to_owned(), key.clone());
            self.seen.remove(slot);
        } else {
            loaded.drafts.remove(index);
            write_drafts(&path, loaded.drafts)?;
            self.drafts.remove(key);
            self.recovered_keys.remove(key);
            self.restored.remove(key);
        }
        self.kept_aside = None;
        Ok(())
    }

    pub(super) fn take(&mut self, draft: &Draft, slot: &str) -> Result<(), String> {
        self.change_recovered(draft, Some(slot))
    }

    pub(super) fn discard(&mut self, key: &str) {
        let Some(draft) = self.drafts.get(key).filter(|_| self.recovered_keys.contains(key)) else { return };
        if self.owns(draft) || (draft.owner.is_none() && self.dirty.contains(key)) {
            // A local draft may be too large to save. Remove it before retrying the write.
            self.forget(key, Instant::now());
            return;
        }
        let draft = draft.clone();
        if let Err(reason) = self.change_recovered(&draft, None) {
            self.notices.push(format!("초안을 버리지 못했습니다: {reason}"));
        }
    }

    /// The editor of a slot no longer holds its draft (its document was left): the draft is listed as recovered, so the user
    /// decides about it later. A draft of a request on its way stays with its answer; a draft marked by a request whose answer
    /// came, or that was sent before a restart, has nothing more to wait for and is listed too.
    pub(super) fn release(&mut self, slot: &str) {
        let Some(key) = self.owned.remove(slot) else {
            return;
        };
        self.seen.remove(slot);
        let on_its_way = self.sent.values().any(|request| request.key == key);
        if self.drafts.contains_key(&key) && !on_its_way {
            self.recovered_keys.insert(key);
            self.flush();
        }
    }

    /// A request is about to be sent from `live`, the content it carries. When the slot's own draft holds that content, the draft
    /// takes the request id as its mark; otherwise the request gets a draft of its own, and the slot's draft (its newer text) is
    /// left as it is. The mark is written before the request goes out. `keep_owner` keeps the slot holding the draft (the figure's
    /// AI tab keeps its text); otherwise the editor has given the content up. `ticket` is the ticket the request will have
    /// (`rebind`). When the write fails, the marking is undone, the mark the draft carried before comes back, and nothing may be sent.
    pub(super) fn begin_send(&mut self, ticket: u64, slot: &str, live: Live, request_id: &str, keep_owner: bool, now: Instant) -> Result<(), String> {
        self.begin_send_start(ticket, slot, live, request_id, None, keep_owner, now)
    }

    /// `begin_send` for a `job.start` whose params are `start`: the draft keeps them with the mark, so that a restart can send the
    /// start again as it was (`Draft::start`).
    #[allow(clippy::too_many_arguments)]
    pub(super) fn begin_send_start(
        &mut self,
        ticket: u64,
        slot: &str,
        live: Live,
        request_id: &str,
        start: Option<Value>,
        keep_owner: bool,
        now: Instant,
    ) -> Result<(), String> {
        if self.read_only {
            return Err("초안 파일이 읽기 전용이어서 요청을 저장할 수 없습니다".into());
        }
        let start = start.map(|start| kept_start(start, &live.body));
        let owned_before = self.owned.get(slot).cloned().filter(|key| self.drafts.contains_key(key));
        let same = owned_before.as_ref().and_then(|key| self.drafts.get(key)).is_some_and(|draft| same_content(&draft.body, &live.body));
        let key = match (&owned_before, same) {
            (Some(key), true) => key.clone(),
            _ => new_id(),
        };
        let mut start_before = None;
        let before = if same {
            let before = self.drafts.get_mut(&key).and_then(|draft| {
                start_before = std::mem::replace(&mut draft.start, start);
                draft.sent.replace(request_id.to_owned())
            });
            if !keep_owner {
                self.owned.remove(slot);
                self.seen.remove(slot);
            }
            before
        } else {
            let entry = Draft {
                key: key.clone(),
                slot: slot.to_owned(),
                artifact_id: live.artifact_id.clone(),
                revision: live.revision,
                saved_at_ms: now_ms(),
                body: live.body.clone(),
                sent: Some(request_id.to_owned()),
                start,
                owner: None,
            };
            self.drafts.insert(key.clone(), entry);
            if keep_owner && owned_before.is_none() {
                self.owned.insert(slot.to_owned(), key.clone());
                self.seen.insert(slot.to_owned(), Some(live.body));
            }
            None
        };
        let already_dirty = !self.dirty.insert(key.clone());
        self.changed(now);
        if let Err(message) = self.write_report() {
            if !already_dirty {
                self.dirty.remove(&key);
            }
            if same {
                if let Some(draft) = self.drafts.get_mut(&key) {
                    draft.sent = before;
                    draft.start = start_before;
                }
                if let Some(owner) = owned_before.clone().filter(|_| !keep_owner) {
                    self.owned.insert(slot.to_owned(), owner);
                }
            } else {
                self.drafts.remove(&key);
                if keep_owner && owned_before.is_none() {
                    self.owned.remove(slot);
                }
            }
            self.seen.remove(slot);
            return Err(message);
        }
        if self.path.is_some() && !self.drafts.get(&key).is_some_and(|draft| self.owns(draft) && draft.sent.as_deref() == Some(request_id)) {
            return Err(DRAFT_CHANGED.into());
        }
        self.sent.insert(ticket, SentRequest { key, request_id: request_id.to_owned(), before, before_start: start_before });
        Ok(())
    }

    /// The request marked under `from` was sent under `to` (the ticket the host queue gave it).
    pub(super) fn rebind(&mut self, from: u64, to: u64) {
        if from != to {
            if let Some(request) = self.sent.remove(&from) {
                self.sent.insert(to, request);
            }
        }
    }

    fn sent_draft(&self, ticket: u64) -> Option<(&Draft, &str)> {
        let request = self.sent.get(&ticket)?;
        Some((self.drafts.get(&request.key)?, &request.request_id))
    }

    /// An unsent request restores the previous mark. A host refusal removes only the refused id. An unknown outcome keeps the
    /// draft marked, and acceptance removes it. The editor can recover the draft while it still holds the content that was sent.
    pub(super) fn answered(&mut self, ticket: u64, outcome: Outcome, other_mark: Option<&str>, now: Instant) -> Option<(Outcome, Draft)> {
        let request = self.sent.remove(&ticket)?;
        let key = request.key;
        let mut before = request.before;
        let before_start = request.before_start;
        if outcome == Outcome::Refused {
            // An identical resend passes the same input checks before receipt lookup (core/src/jobs.rs:108-128). The job and
            // receipt commit together (core/src/jobs.rs:196), so a host refusal rules out a job for this id only.
            before = before.filter(|id| id != &request.request_id).or_else(|| other_mark.map(str::to_owned));
            // A later request of the same draft that is still on its way found this request's mark: it gets the mark this request
            // found instead, so that its own refusal does not bring back the mark of a request already refused.
            for later in self.sent.values_mut().filter(|later| later.key == key && later.before.as_deref() == Some(request.request_id.as_str())) {
                later.before.clone_from(&before);
            }
        }
        let draft = self.drafts.get(&key)?;
        if draft.sent.is_none() {
            // The content changed after it was sent: what the editor holds now is newer than the request.
            return None;
        }
        if outcome == Outcome::Accepted {
            if let Some(slot) = self.owner_of(&key) {
                self.seen.insert(slot, Some(draft.body.clone()));
            }
            self.forget_key(&key);
            self.removed.insert(key, Some(request.request_id));
            self.changed(now);
            let _ = self.write_report();
            return None;
        }
        // Only an unsent figure job.start uses NotSent and keeps its editor text. Home and cell send failures use Refused.
        let answer = (outcome != Outcome::NotSent).then(|| (outcome, draft.clone()));
        if matches!(outcome, Outcome::NotSent | Outcome::Refused) {
            if let Some(entry) = self.drafts.get_mut(&key) {
                if entry.sent.as_deref() == Some(request.request_id.as_str()) {
                    entry.sent = before;
                    // The params of the mark that comes back, when the draft kept them for it; none for another request's mark.
                    entry.start = before_start;
                    if !entry.marks_start() {
                        entry.start = None;
                    }
                    self.dirty.insert(key.clone());
                }
            }
        }
        if outcome == Outcome::Unknown && self.restored.get(&key) == Some(&request.request_id) {
            self.restored.remove(&key);
        }
        if self.owner_of(&key).is_none() {
            self.recovered_keys.insert(key);
        }
        self.changed(now);
        let _ = self.write_report();
        answer
    }

    /// Removes a draft whose content now lives in an editor, or was sent and accepted, and writes the file now.
    pub(super) fn forget(&mut self, key: &str, now: Instant) {
        self.forget_key(key);
        self.changed(now);
        self.flush();
    }

    /// Keeps `live` as a recovered draft that is only copied or discarded (its document or table has moved on).
    pub(super) fn keep_recovered(&mut self, live: Live, now: Instant) {
        let key = new_id();
        let draft = Draft {
            key: key.clone(),
            slot: String::new(),
            artifact_id: live.artifact_id,
            revision: live.revision,
            saved_at_ms: now_ms(),
            body: live.body,
            sent: None,
            start: None,
            owner: None,
        };
        self.drafts.insert(key.clone(), draft);
        self.recovered_keys.insert(key.clone());
        self.dirty.insert(key);
        self.changed(now);
        let _ = self.write_report();
    }

    /// Writes the file once the last change has been quiet long enough, or has waited long enough.
    pub(super) fn flush_due(&mut self, now: Instant) {
        let (Some(since), Some(last)) = (self.unsaved_since, self.last_change) else {
            return;
        };
        if now.duration_since(last) < QUIET && now.duration_since(since) < LATEST {
            return;
        }
        if self.write_report().is_err() {
            self.unsaved_since = Some(now);
            self.last_change = Some(now);
        }
    }

    /// Writes the file now when there are unwritten changes (on close, and after a send or a discard). False when that write failed.
    pub(super) fn flush(&mut self) -> bool {
        if self.unsaved_since.is_some() {
            return self.write_report().is_ok();
        }
        true
    }

    /// Whether the last write of the file failed, so that drafts are not on disk yet.
    pub(super) fn save_failing(&self) -> bool {
        self.failing
    }

    fn owner_of(&self, key: &str) -> Option<String> {
        self.owned.iter().find(|(_, owned)| owned.as_str() == key).map(|(slot, _)| slot.clone())
    }

    /// Removes a draft from this window: from the list, from its slot and from the file on the next write.
    fn forget_key(&mut self, key: &str) {
        self.drafts.remove(key);
        self.recovered_keys.remove(key);
        self.owned.retain(|_, owned| owned.as_str() != key);
        self.dirty.remove(key);
        self.restored.remove(key);
        self.removed.insert(key.to_owned(), None);
    }

    /// Writes a slot's current content into its draft: a new draft, the changed content, or nothing left to keep.
    fn store(&mut self, slot: &str, current: Option<Live>, now: Instant) {
        let owned = self.owned.get(slot).cloned().filter(|key| self.drafts.contains_key(key));
        match (current, owned) {
            (Some(live), Some(key)) => {
                if let Some(draft) = self.drafts.get_mut(&key) {
                    // A change of the content is not the request that was sent: its mark goes.
                    if !same_content(&draft.body, &live.body) {
                        draft.sent = None;
                        draft.start = None;
                    }
                    draft.artifact_id = live.artifact_id;
                    draft.revision = live.revision;
                    draft.body = live.body;
                    draft.saved_at_ms = now_ms();
                }
                self.dirty.insert(key);
            }
            (Some(live), None) => {
                let key = new_id();
                self.owned.insert(slot.to_owned(), key.clone());
                self.dirty.insert(key.clone());
                self.drafts.insert(
                    key.clone(),
                    Draft {
                        key,
                        slot: slot.to_owned(),
                        artifact_id: live.artifact_id,
                        revision: live.revision,
                        saved_at_ms: now_ms(),
                        body: live.body,
                        sent: None,
                        start: None,
                        owner: None,
                    },
                );
            }
            (None, Some(key)) => {
                self.owned.remove(slot);
                self.forget_key(&key);
            }
            (None, None) => return,
        }
        self.changed(now);
    }

    fn changed(&mut self, now: Instant) {
        self.unsaved_since.get_or_insert(now);
        self.last_change = Some(now);
    }

    fn write_report(&mut self) -> Result<(), String> {
        match self.write() {
            Ok(()) => {
                self.unsaved_since = None;
                self.last_change = None;
                self.failing = false;
                Ok(())
            }
            Err(message) => {
                if !self.failing {
                    self.notices.push(format!("초안을 저장하지 못했습니다: {message}"));
                }
                self.failing = true;
                Err(message)
            }
        }
    }

    fn keep_skipped(&mut self, path: &Path, loaded: &Loaded) -> Result<(), String> {
        if loaded.skipped > 0 && self.kept_aside != Some(loaded.fingerprint) {
            if !copy_aside(path) {
                return Err("손상된 항목이 있는 초안 파일을 옆에 남기지 못해 저장하지 않습니다".into());
            }
            self.kept_aside = Some(loaded.fingerprint);
        }
        Ok(())
    }

    /// Merge editor changes with the latest file, keeping other windows' rows.
    fn write(&mut self) -> Result<(), String> {
        let Some(path) = self.path.clone().filter(|_| !self.read_only) else {
            return Ok(());
        };
        let _lock = lock_drafts(&path)?.ok_or(NO_SESSION)?;
        let mut merged: BTreeMap<String, Draft> = BTreeMap::new();
        match read_file(&path) {
            Ok(loaded) => {
                self.keep_skipped(&path, &loaded)?;
                self.reconcile(&loaded);
                for draft in loaded.drafts {
                    let removed = self.removed.get(&draft.key).is_some_and(|accepted| {
                        self.owns(&draft) || (accepted.is_some() && *accepted == draft.sent)
                    });
                    if !removed && !self.drafts.contains_key(&draft.key) {
                        merged.insert(draft.key.clone(), draft);
                    }
                }
            }
            Err(ReadError::Newer) => {
                self.read_only = true;
                return Err("다른 버전이 쓴 초안 파일이어서 저장하지 않습니다".into());
            }
            Err(ReadError::Unreadable | ReadError::TooLarge) => {
                // Write retries on the next flush (`failing`, `flush_due`), so it does not turn the window read-only as open does.
                if !move_aside(&path) && !gone(&path) {
                    return Err("손상된 초안 파일을 옮기지 못해 저장하지 않습니다".into());
                }
            }
            Err(ReadError::Io(message)) => return Err(message),
        }
        if !self.dirty.is_empty() {
            let owner = self.session_id(&path)?.to_owned();
            for key in &self.dirty {
                if let Some(draft) = self.drafts.get_mut(key) {
                    draft.owner.get_or_insert_with(|| owner.clone());
                }
            }
        }
        for (key, draft) in &self.drafts {
            merged.insert(key.clone(), draft.clone());
        }
        write_drafts(&path, merged.into_values().collect())?;
        self.removed.clear();
        self.dirty.clear();
        // The file now holds no skipped entry: damage that shows up later, even the same bytes again, is copied aside again.
        self.kept_aside = None;
        Ok(())
    }
}

fn write_drafts(path: &Path, drafts: Vec<Draft>) -> Result<(), String> {
    let text = serde_json::to_string_pretty(&DraftFile { schema_version: SCHEMA_VERSION, drafts }).map_err(|error| error.to_string())?;
    if text.len() as u64 > MAX_FILE_BYTES {
        return Err("초안이 8 MiB를 넘어 저장하지 않습니다. 긴 초안을 복사하거나 버려 주세요".into());
    }
    #[cfg(test)]
    if FAIL_REPLACE.with(std::cell::Cell::get) {
        return Err("test: the replacing write fails".into());
    }
    write_atomic(path, &text).map_err(|error| error.to_string())
}

impl DesktopApp {
    /// Brings every draft up to what its editor holds, and writes the changes once they are quiet. Called each frame, and
    /// before a request is sent.
    pub(super) fn sync_drafts(&mut self) {
        let now = Instant::now();
        let home = self.home_edit();
        self.drafts.sync("home", home, now);
        if let Some(slot) = self.active.as_ref().map(|active| ai_tab_slot(active.kind, &active.id)) {
            let edit = self.ai_tab_edit();
            self.drafts.sync(&slot, edit, now);
        }
        if let Some((slot, edit)) = self.note_edit() {
            self.drafts.sync(&slot, edit, now);
        }
        if let Some((slot, edit)) = self.crop_edit() {
            self.drafts.sync(&slot, edit, now);
        }
        let cell = self.cell_edit();
        self.drafts.sync("cell", cell, now);
        if let Some((slot, edit)) = self.inline_edit() {
            self.drafts.sync(&slot, edit, now);
        }
        let pdf = self.held_pdf_items();
        self.drafts.sync_group("pdf:", pdf, now);
        self.drafts.flush_due(now);
    }

    /// The home request box: its text and files. Files and text the box gives up for a request are its request's draft
    /// (`begin_send`), so what is in the box now is only what was typed or attached since.
    pub(super) fn home_edit(&self) -> Edit {
        if self.home.prompt.trim().is_empty() && self.home.attachments.is_empty() {
            return Edit::Empty;
        }
        Edit::Has(Live {
            artifact_id: None,
            revision: None,
            body: Body::Home {
                prompt: self.home.prompt.clone(),
                attachments: self.home.attachments.clone(),
                figure_refs: self.ai_figure_refs.clone(),
            },
        })
    }

    /// The AI tab of the open document: a Figure's with its evidence files, another document's with its text.
    pub(super) fn ai_tab_edit(&self) -> Edit {
        let Some(active) = self.active.as_ref() else {
            return Edit::Absent;
        };
        let body = if active.kind == ArtifactKind::Figure {
            let attachments: Vec<String> = self.figure_ai_files().iter().map(|file| file.path.to_string_lossy().into_owned()).collect();
            if self.ai_prompt.trim().is_empty() && attachments.is_empty() {
                return Edit::Empty;
            }
            Body::Figure { prompt: self.ai_prompt.clone(), attachments, figure_refs: self.ai_figure_refs.clone() }
        } else {
            if self.ai_prompt.trim().is_empty() {
                return Edit::Empty;
            }
            Body::Ai { prompt: self.ai_prompt.clone(), figure_refs: self.ai_figure_refs.clone() }
        };
        Edit::Has(Live { artifact_id: Some(active.id.clone()), revision: Some(active.revision), body })
    }

    /// The board memo or title editor, or the one whose change is on its way.
    fn note_edit(&self) -> Option<(String, Edit)> {
        let editor = self
            .board_memo_editor
            .as_ref()
            .or_else(|| self.board_memo_mutation.as_ref().and_then(|mutation| mutation.editor.as_ref()))?;
        let slot = format!("note:{}", editor.memo.id);
        let nothing = if editor.is_new {
            editor.memo.text.trim().is_empty()
        } else {
            editor.expected.iter().find(|memo| memo.id == editor.memo.id) == Some(&editor.memo)
        };
        let edit = if nothing {
            Edit::Empty
        } else {
            Edit::Has(Live {
                artifact_id: Some(editor.artifact_id.clone()),
                revision: Some(editor.revision),
                body: Body::Note { memo: editor.memo.clone(), page_index: editor.page_index, is_new: editor.is_new },
            })
        };
        Some((slot, edit))
    }

    /// The crop editor. Its draft counts while the user has typed in it; a change on its way keeps it.
    fn crop_edit(&self) -> Option<(String, Edit)> {
        let editor = self.image_crop_editor.as_ref()?;
        let slot = format!("crop:{}", editor.intent.token);
        let edit = if editor.outcome_uncertain {
            Edit::Absent
        } else {
            match (editor.stage, editor.draft.as_ref()) {
                (ImageCropStage::Editing, Some(fields)) if editor.draft_is_dirty() => Edit::Has(Live {
                    artifact_id: Some(editor.intent.artifact_id.clone()),
                    revision: Some(editor.intent.expected_revision),
                    body: Body::Crop {
                        node_id: editor.intent.node_id.clone(),
                        asset_sha256: editor.intent.expected_asset_sha256.clone(),
                        fields: fields.clone(),
                    },
                }),
                (ImageCropStage::Editing | ImageCropStage::Loading, _) => Edit::Empty,
                (ImageCropStage::Applying, _) => Edit::Absent,
            }
        };
        Some((slot, edit))
    }

    /// The table cell being edited. Closing the cell editor keeps nothing: the value is either committed or dropped.
    fn cell_edit(&self) -> Edit {
        let (Some(edit), Some(active)) = (self.dataset_edit.as_ref(), self.active.as_ref()) else {
            return Edit::Empty;
        };
        if edit.draft == edit.original {
            return Edit::Empty;
        }
        Edit::Has(Live { artifact_id: Some(active.id.clone()), revision: Some(active.revision), body: cell_body(edit) })
    }

    /// The text typed into the open inline session, while it differs from what the host confirmed.
    fn inline_edit(&self) -> Option<(String, Edit)> {
        let session = self.inline_text_edit.as_ref()?;
        let slot = format!("inline:{}", session.id);
        let edit = if self.text_draft == session.committed_text {
            Edit::Empty
        } else {
            Edit::Has(Live {
                artifact_id: Some(session.artifact_id.clone()),
                revision: Some(session.revision),
                body: Body::Inline {
                    node_id: session.node_id.clone(),
                    page_index: session.page_index,
                    text: self.text_draft.clone(),
                    committed: session.committed_text.clone(),
                },
            })
        };
        Some((slot, edit))
    }

    /// The held PDF evidence sets: their reference and counts.
    fn held_pdf_items(&self) -> Vec<(String, Live)> {
        self.held_pdf_evidence
            .iter()
            .map(|held| {
                (
                    format!("pdf:{}", held.id),
                    Live {
                        artifact_id: Some(held.target.artifact_id.clone()),
                        revision: Some(held.target.revision),
                        body: Body::PdfEvidence {
                            page_index: held.target.page_index,
                            file_name: held.target.file_name.clone(),
                            asset_sha256: held.target.asset_sha256.clone(),
                            records: held.records.len(),
                            message: held.message.clone(),
                        },
                    },
                )
            })
            .collect()
    }

    /// The window is about to show another document (or none). The drafts so far are written, the open document's AI tab draft is
    /// listed as recovered under its own document, and the AI prompt and figure references that the tab shares are cleared, so
    /// that nothing typed for one document is saved under the next.
    pub(super) fn leave_drafts_for_switch(&mut self) {
        self.sync_drafts();
        if let Some(active) = self.active.as_ref() {
            let slot = ai_tab_slot(active.kind, &active.id);
            self.drafts.release(&slot);
        }
        self.ai_prompt.clear();
        self.ai_figure_refs.clear();
    }

    /// The answer of a request sent from a draft, by its ticket (`process_replies`).
    pub(super) fn answer_sent_drafts(&mut self, ticket: u64, result: &Result<RpcPayload, DltError>) {
        // A refusal that says nothing was changed (busy, invalid, not found) is a refusal even when it may be retried. Only the
        // codes that can come after the host received the request count as unknown.
        let outcome = match result {
            Ok(_) => Outcome::Accepted,
            Err(error) if change_result_unknown(error) => Outcome::Unknown,
            Err(_) => Outcome::Refused,
        };
        self.answer_sent_outcome(ticket, outcome);
    }

    /// Applies an answer to a request sent from a draft. A refused home request or cell value comes back to its editor, when
    /// its editor is free; an unknown answer is reported and the draft is left for the user to check.
    pub(super) fn answer_sent_outcome(&mut self, ticket: u64, outcome: Outcome) {
        let now = Instant::now();
        let other_mark = match outcome {
            Outcome::Refused => self.drafts.sent_draft(ticket).and_then(|(draft, id)| self.unknown_starts.other_draft_mark(draft, id)),
            _ => None,
        };
        let Some((outcome, draft)) = self.drafts.answered(ticket, outcome, other_mark, now) else {
            return;
        };
        let Draft { key, artifact_id, body, .. } = draft;
        match (outcome, body) {
            (Outcome::Refused, Body::Home { prompt, attachments, figure_refs }) => {
                self.merge_home_request(prompt, attachments, figure_refs);
                self.drafts.forget(&key, now);
                // The box's text is written again at once: the request is no longer in the file.
                self.sync_drafts();
                self.drafts.flush();
                self.notices.warning(HOME_REFUSED, now);
            }
            (Outcome::Unknown, Body::Home { .. }) => self.notices.warning(HOME_UNKNOWN, now),
            (Outcome::Refused, Body::Cell { dataset_id, version_hash, row_id, column_id, original, draft }) => {
                // The editor holds the value itself while it is still open (the draft is its own then).
                if self.drafts.held_by_editor(&key) {
                    return;
                }
                let edit = DatasetCellEdit { dataset_id, version_hash, row_id, column_id, original, draft };
                let open_here = self.active.as_ref().is_some_and(|active| Some(active.id.as_str()) == artifact_id.as_deref());
                if self.dataset_edit.is_none() && open_here {
                    self.dataset_edit = Some(edit);
                    self.drafts.forget(&key, now);
                    self.set_error("자료 값 변경을 적용하지 못해 편집 중인 값으로 되돌렸습니다".into());
                } else {
                    self.set_error("자료 값 변경을 적용하지 못했습니다. 값은 초안 목록에 남겼습니다".into());
                }
            }
            (Outcome::Unknown, Body::Cell { .. }) => self.notices.warning(CELL_UNKNOWN, now),
            _ => {}
        }
    }

    /// Keeps a cell value that could not be applied as a recovered draft: its table has changed, or the window cannot send it.
    pub(super) fn keep_cell_as_copy(&mut self, edit: &DatasetCellEdit) {
        let (artifact_id, revision) = self.active.as_ref().map_or((None, None), |active| (Some(active.id.clone()), Some(active.revision)));
        self.drafts.keep_recovered(Live { artifact_id, revision, body: cell_body(edit) }, Instant::now());
    }

    /// The cell editor's value is discarded by the user: its draft goes with it, at once.
    pub(super) fn discard_cell_draft(&mut self) {
        self.dataset_edit = None;
        self.drafts.clear("cell", Edit::Absent, Instant::now());
    }

    /// The crop editor of `token` is done: applied, or discarded.
    pub(super) fn clear_crop_draft(&mut self, token: &str) {
        self.drafts.clear(&format!("crop:{token}"), Edit::Absent, Instant::now());
    }

    /// The inline text session `session_id` ended with its text confirmed or dropped.
    pub(super) fn clear_inline_draft(&mut self, session_id: u64) {
        self.drafts.clear(&format!("inline:{session_id}"), Edit::Absent, Instant::now());
    }

    /// The memo editor of `memo_id` is done: saved, deleted or cancelled.
    pub(super) fn clear_note_draft(&mut self, memo_id: &str) {
        self.drafts.clear(&format!("note:{memo_id}"), Edit::Absent, Instant::now());
    }

    /// Puts a home request's text and files back into the home box, ahead of what the box holds now. The figure references
    /// it carried are added to the shared list, which is never replaced.
    fn merge_home_request(&mut self, prompt: String, attachments: Vec<String>, figure_refs: Vec<FigureRevisionRef>) {
        self.home.prompt = join_text(&prompt, &self.home.prompt);
        let mut files = attachments;
        for path in self.home.attachments.drain(..) {
            if !files.contains(&path) {
                files.push(path);
            }
        }
        self.home.attachments = files;
        for reference in figure_refs {
            if !self.ai_figure_refs.contains(&reference) {
                self.ai_figure_refs.push(reference);
            }
        }
    }

    /// Whether a recovered draft may be restored now, and what stands in the way.
    pub(super) fn draft_standing(&self, draft: &Draft) -> Standing {
        if let Some(reason) = self.drafts.recovery_block(draft) {
            return Standing::blocked(reason);
        }
        match &draft.body {
            Body::Home { .. } => {
                if !matches!(self.screen, Screen::Home) {
                    return Standing::blocked(HOME_ONLY);
                }
                let busy = !self.home.prompt.trim().is_empty() || !self.home.attachments.is_empty() || !self.ai_figure_refs.is_empty();
                return if busy { Standing::blocked(BUSY) } else { Standing::ready() };
            }
            Body::Inline { .. } => return Standing::blocked("텍스트 입력 초안은 복사하거나 버릴 수 있습니다"),
            Body::PdfEvidence { .. } => return Standing::blocked("PDF 근거는 다시 가져와야 합니다. 보류한 내용은 저장되지 않습니다"),
            Body::Figure { .. } | Body::Ai { .. } | Body::Note { .. } | Body::Crop { .. } | Body::Cell { .. } => {}
        }
        let (Some(artifact_id), Some(revision)) = (draft.artifact_id.as_deref(), draft.revision) else {
            return Standing::blocked("작업물 정보가 없어 복사하거나 버릴 수 있습니다");
        };
        let open = self.active.as_ref().map(|active| (active.id.as_str(), active.revision));
        let listed = self.artifacts.iter().find(|summary| summary.id == artifact_id).map(|summary| summary.revision);
        match placement(artifact_id, revision, open, listed) {
            Placement::Missing if !self.workspace_loaded => return Standing::blocked("작업물 목록을 읽는 중입니다"),
            Placement::Missing => return Standing::blocked("작업물을 찾지 못해 복사하거나 버릴 수 있습니다"),
            Placement::Stale => return Standing::blocked("작업물이 바뀌어 자동으로 복원하지 않습니다. 복사하거나 버리세요"),
            Placement::NotOpen => {
                return Standing { ready: false, reason: Some("작업물을 열면 복원할 수 있습니다"), open: Some(artifact_id.to_owned()) };
            }
            Placement::Here => {}
        }
        let busy = match &draft.body {
            Body::Figure { .. } => {
                !self.ai_prompt.trim().is_empty() || !self.figure_ai_files().is_empty() || !self.ai_figure_refs.is_empty() || self.figure_ai_request.is_some()
            }
            Body::Ai { .. } => !self.ai_prompt.trim().is_empty() || !self.ai_figure_refs.is_empty(),
            Body::Note { memo, page_index, .. } => {
                if self.board_memo_editor.is_some() || self.board_memo_mutation.is_some() {
                    true
                } else {
                    return match self.board_memo_page(*page_index) {
                        Some((id, current, page_id, _)) if id == artifact_id && current == revision && page_id == memo.page_id => Standing::ready(),
                        _ => Standing::blocked("보드 페이지가 바뀌어 복원하지 않습니다"),
                    };
                }
            }
            Body::Crop { node_id, asset_sha256, .. } => {
                let ready = self.image_crop_editor.as_ref().is_some_and(|editor| {
                    editor.stage == ImageCropStage::Editing
                        && !editor.outcome_uncertain
                        && !editor.draft_is_dirty()
                        && editor.intent.artifact_id == artifact_id
                        && editor.intent.node_id == *node_id
                        && editor.intent.expected_asset_sha256 == *asset_sha256
                });
                return if ready { Standing::ready() } else { Standing::blocked("같은 이미지의 자르기 편집기를 열면 복원할 수 있습니다") };
            }
            Body::Cell { dataset_id, version_hash, .. } => {
                if self.dataset_edit.is_some() {
                    true
                } else {
                    let ready = self.sheet_dataset().is_some_and(|dataset| dataset.id == *dataset_id && dataset.version_hash == *version_hash);
                    return if ready { Standing::ready() } else { Standing::blocked("같은 자료의 표를 열면 복원할 수 있습니다") };
                }
            }
            Body::Home { .. } | Body::Inline { .. } | Body::PdfEvidence { .. } => false,
        };
        if busy { Standing::blocked(BUSY) } else { Standing::ready() }
    }

    /// Restores a recovered draft into its editor when it stands ready. The draft then belongs to that editor, and it is
    /// kept until the user sends, applies or discards it.
    pub(super) fn restore_draft(&mut self, key: &str) {
        let Some(draft) = self.drafts.drafts.get(key).cloned().filter(|_| self.drafts.recovered_keys.contains(key)) else {
            self.notices.warning(DRAFT_CHANGED, Instant::now());
            return;
        };
        let standing = self.draft_standing(&draft);
        if !standing.ready {
            self.notices.warning(standing.reason.unwrap_or(BUSY), Instant::now());
            return;
        }
        let slot = match (&draft.body, self.image_crop_editor.as_ref()) {
            (Body::Crop { .. }, Some(editor)) => format!("crop:{}", editor.intent.token),
            (Body::Crop { .. }, None) => return,
            (Body::Home { .. }, _) => "home".to_owned(),
            _ => draft.slot.clone(),
        };
        if let Err(reason) = self.drafts.take(&draft, &slot) {
            self.notices.warning(format!("초안을 복원하지 못했습니다: {reason}"), Instant::now());
            return;
        }
        if let Some(start) = draft.marked_start() {
            self.adopt_start_settings(&start);
            if let Some(mut sent) = super::figure_close::SentStart::of(&start) {
                sent.draft = Some(draft.body.clone());
                self.keep_unknown_start(sent);
            }
        }
        let missing = self.apply_draft(&draft);
        if !missing.is_empty() {
            let note = if draft.marks_start() && matches!(draft.body, Body::Figure { .. }) {
                "원본 파일을 찾지 못했습니다. 같은 요청을 다시 보내면 저장된 근거 자료를 사용합니다"
            } else {
                "첨부 파일을 찾지 못해 뺐습니다"
            };
            self.notices.warning(format!("{note}: {}", missing.join(", ")), Instant::now());
        }
        self.show_dropped_unknown_starts();
    }

    /// Restores the AI tab's choices, including the target scope checked by `ai_selection_refusal` before sending.
    fn adopt_start_settings(&mut self, start: &Value) {
        if let Some(intents) = start.get("allowed_intents").and_then(Value::as_array) {
            let has = |name: &str| intents.iter().any(|intent| intent.as_str() == Some(name));
            self.allow_content = has("content");
            self.allow_design = has("design");
            self.allow_data = has("data");
        }
        for (field, value) in [("runtime", &mut self.ai_runtime), ("model", &mut self.ai_model), ("effort", &mut self.ai_effort)] {
            if let Some(text) = start.get(field).and_then(Value::as_str) {
                text.clone_into(value);
            }
        }
        let delivery = json!({ "deliver_as": start.get("deliver_as").cloned().unwrap_or(Value::Null) });
        let _ = self.proposal_command("ai.deliver_as", &delivery);
        let targets = start.get("target_ids").and_then(Value::as_array).into_iter().flatten()
            .filter_map(Value::as_str).map(str::to_owned).collect();
        self.ui.user_selection = self.active.as_ref().map(|active| (self.view_id.clone(), active.id.clone(), targets));
    }

    /// Puts a restored draft's content into its editor. Returns the names of the files that are gone.
    fn apply_draft(&mut self, draft: &Draft) -> Vec<String> {
        match &draft.body {
            Body::Home { prompt, attachments, figure_refs } => {
                let (present, missing) = split_files(attachments);
                self.merge_home_request(prompt.clone(), present, figure_refs.clone());
                missing
            }
            Body::Figure { prompt, attachments, figure_refs } => {
                let (present, missing) = split_files(attachments);
                let files = if draft.marks_start() { attachments } else { &present };
                self.restore_figure_ai_draft(prompt.clone(), files.iter().map(PathBuf::from).collect(), figure_refs.clone());
                missing
            }
            Body::Ai { prompt, figure_refs } => {
                self.ai_prompt.clone_from(prompt);
                // The references are shared with the home box, so they are added to, never replaced.
                for reference in figure_refs {
                    if !self.ai_figure_refs.contains(reference) {
                        self.ai_figure_refs.push(reference.clone());
                    }
                }
                Vec::new()
            }
            Body::Note { memo, page_index, is_new } => {
                self.restore_board_memo_draft(memo.clone(), *page_index, *is_new);
                Vec::new()
            }
            Body::Crop { fields, .. } => {
                if let Some(editor) = self.image_crop_editor.as_mut() {
                    editor.draft = Some(fields.clone());
                }
                Vec::new()
            }
            Body::Cell { dataset_id, version_hash, row_id, column_id, original, draft: text } => {
                self.dataset_edit = Some(DatasetCellEdit {
                    dataset_id: dataset_id.clone(),
                    version_hash: version_hash.clone(),
                    row_id: row_id.clone(),
                    column_id: column_id.clone(),
                    original: original.clone(),
                    draft: text.clone(),
                });
                Vec::new()
            }
            Body::Inline { .. } | Body::PdfEvidence { .. } => Vec::new(),
        }
    }

    fn target_title(&self, draft: &Draft) -> String {
        match draft.artifact_id.as_deref() {
            None => "홈".to_owned(),
            Some(id) => self
                .artifacts
                .iter()
                .find(|summary| summary.id == id)
                .map_or_else(|| "작업물".to_owned(), |summary| summary.title.clone()),
        }
    }

    /// The recovered drafts, one block each, with the buttons that act on them. Nothing is applied until `복원` is pressed.
    pub(super) fn draw_draft_recovery(&mut self, ctx: &egui::Context) {
        let now = Instant::now();
        if !self.drafts.recovered_keys.is_empty()
            && self.drafts.last_recovery_read.is_none_or(|last| now.duration_since(last) >= Duration::from_secs(1))
        {
            self.drafts.last_recovery_read = Some(now);
            self.drafts.refresh();
        }
        let drafts = self.drafts.recovered();
        if drafts.is_empty() {
            return;
        }
        let palette = theme::palette(ctx);
        let (mut restore, mut discard, mut copy, mut open) = (None, None, None, None);
        egui::Window::new("복구할 초안")
            .anchor(Align2::RIGHT_BOTTOM, [-16.0, -16.0])
            .collapsible(false)
            .resizable(false)
            .default_width(360.0)
            .show(ctx, |ui| {
                for draft in &drafts {
                    let standing = self.draft_standing(draft);
                    let when = modified_label(draft.saved_at_ms, now_ms(), local_offset_minutes());
                    ui.label(RichText::new(format!("{} · {} · {when}", kind_label(&draft.body), self.target_title(draft))).strong());
                    ui.label(preview(&draft.body));
                    let missing = missing_files(&draft.body);
                    if !missing.is_empty() {
                        ui.colored_label(palette.warn, format!("첨부 파일 없음: {}", missing.join(", ")));
                    }
                    if let Some(reason) = standing.reason {
                        ui.label(RichText::new(reason).color(palette.ink_3));
                    }
                    ui.horizontal(|ui| {
                        let restore_button = ui.add_enabled(standing.ready, egui::Button::new("복원"));
                        crate::ui_record::note_human_only(ui, "복원", &restore_button);
                        if restore_button.clicked() {
                            restore = Some(draft.key.clone());
                        }
                        if standing.open.is_some() && ui.button("열기").clicked() {
                            open = standing.open.clone();
                        }
                        if ui.button("복사").clicked() {
                            copy = Some(copy_text(&draft.body));
                        }
                        let discard_button = ui.add_enabled(standing.reason != Some(OTHER_OWNER), egui::Button::new("버리기"));
                        crate::ui_record::note_human_only(ui, "버리기", &discard_button);
                        if discard_button.clicked() {
                            discard = Some(draft.key.clone());
                        }
                    });
                    ui.separator();
                }
            });
        if let Some(text) = copy {
            ctx.copy_text(text);
        }
        if let Some(artifact_id) = open {
            self.invoke_ui("nav.open_project", json!({ "artifact_id": artifact_id }));
        }
        if let Some(key) = discard {
            self.drafts.discard(&key);
        }
        if let Some(key) = restore {
            self.restore_draft(&key);
        }
    }
}
