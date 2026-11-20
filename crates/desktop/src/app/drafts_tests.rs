//! The draft file and the drafts of the window: round trips, a crash before the write, stale revisions, missing files, the
//! marks of requests on their way, clearing on send, the merge between windows, and the guards for files of another version or
//! with broken content. The window is driven through its own handlers; no runtime runs here.

use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
    sync::mpsc::{Receiver, Sender},
    time::{Duration, Instant},
};

use designllm_contracts::{new_id, ArtifactKind, ArtifactSummary, CanvasBoardMemo, CanvasBoardMemoKind, DltError, ErrorCode, FigureRevisionRef, Rect as EmuRect};
use eframe::egui::{Context, RawInput, ViewportCommand, ViewportEvent, ViewportId};
use serde_json::{json, Value};

use super::figure_close::{SentStart, REPEAT_OF_UNKNOWN_START, UNKNOWN_STARTS_MAX, UNKNOWN_START_MAY_RUN};
use super::drafts::{
    drafts_path, figure_slot, lock_drafts, placement, quarantine_path, split_files, AsideKind, Body, Draft, DraftBook, Edit, FailingAside, Live, Outcome,
    Placement,
};
use super::test_support::Host;
use super::{
    state::{ApplyContext, HeldEditState, ImageReplacementIntent},
    BoardMemoEditor, DatasetCellEdit, DesktopApp, HeldPdfEvidence, ImageCropEditor, ImageCropStage,
    HeldEdit, InlineTextEdit, PdfEvidenceTarget, PendingAction, Screen,
};
use crate::grapheme::TextInputEdit;
use crate::rpc::{RpcPayload, RpcReply, RpcWorker, Work};

fn workspace() -> PathBuf {
    std::env::temp_dir().join(format!("designllm-drafts-{}", new_id()))
}

fn home_live(prompt: &str) -> Live {
    Live {
        artifact_id: None,
        revision: None,
        body: Body::Home { prompt: prompt.into(), attachments: Vec::new(), figure_refs: Vec::new() },
    }
}

fn figure_live(prompt: &str, files: &[String], revision: u64) -> Live {
    Live {
        artifact_id: Some("figure-1".into()),
        revision: Some(revision),
        body: Body::Figure { prompt: prompt.into(), attachments: files.to_vec(), figure_refs: Vec::new() },
    }
}

fn home_body(prompt: &str) -> Body {
    Body::Home { prompt: prompt.into(), attachments: Vec::new(), figure_refs: Vec::new() }
}

fn summary(id: &str, kind: ArtifactKind, revision: u64) -> ArtifactSummary {
    ArtifactSummary {
        id: id.into(),
        kind,
        title: id.into(),
        revision,
        engine_id: "engine".into(),
        dirty: false,
        modified_at_ms: None,
        active_jobs: Vec::new(),
    }
}

/// The JSON of a created canvas, as the host answers `create_from_home`.
fn canvas_answer(id: &str) -> Value {
    json!({
        "artifact": { "id": id, "kind": "canvas", "title": id, "revision": 1, "engine_id": "canvas", "dirty": false, "modified_at_ms": null, "active_jobs": [] },
        "job": null,
    })
}

fn reference(number: u64) -> FigureRevisionRef {
    FigureRevisionRef { figure_id: format!("figure-{number}"), figure_revision: number }
}

fn cell_edit(draft: &str) -> DatasetCellEdit {
    DatasetCellEdit {
        dataset_id: "ds-1".into(),
        version_hash: "v1".into(),
        row_id: "r1".into(),
        column_id: "value".into(),
        original: "0.20".into(),
        draft: draft.into(),
    }
}

/// A window on a workspace folder: on the home screen, with a model, and the drafts of that folder.
struct Harness {
    app: DesktopApp,
    replies: Sender<RpcReply>,
    work: Receiver<Work>,
    ctx: Context,
    root: PathBuf,
}

/// A window on the workspace folder `root`, with the drafts of that folder; see `Harness`.
fn window_on(root: &Path) -> (DesktopApp, Sender<RpcReply>, Receiver<Work>) {
    let (rpc, replies, work) = RpcWorker::detached();
    let mut app = DesktopApp::new(rpc, "초안".into(), None, None);
    while work.try_recv().is_ok() {}
    app.drafts = DraftBook::open(drafts_path(root));
    app.workspace_loaded = true;
    app.screen = Screen::Home;
    app.ai_model = "mock-model".into();
    app.ai_runtime = "mock".into();
    app.ai_effort = "high".into();
    (app, replies, work)
}

impl Harness {
    fn new() -> Self {
        let root = workspace();
        std::fs::create_dir_all(&root).expect("the workspace folder");
        let (app, replies, work) = window_on(&root);
        Self { app, replies, work, ctx: Context::default(), root }
    }

    /// The window ends as a killed process does: none of its code runs any more (no drop, no write of its drafts), and its
    /// unanswered requests are never answered, and the system lets its locks go (its session's lock file stays). A new window
    /// then opens the same workspace folder.
    fn crash_and_restart(&mut self) {
        self.app.drafts.end_session_as_crash();
        let (app, replies, work) = window_on(&self.root);
        std::mem::forget(std::mem::replace(&mut self.app, app));
        self.replies = replies;
        self.work = work;
    }

    /// Types `prompt` into the home box and sends it as the window does; returns the request's ticket.
    fn create_home(&mut self, prompt: &str) -> u64 {
        self.app.home.prompt = prompt.into();
        self.app.create_from_home("blank", prompt.into()).expect("the home request is sent");
        self.last_home_ticket()
    }

    /// The ticket of the newest home request the window sent.
    fn last_home_ticket(&mut self) -> u64 {
        let mut ticket = None;
        while let Ok(work) = self.work.try_recv() {
            if let Work::Home { ticket: sent, .. } = work {
                ticket = Some(sent);
            }
        }
        ticket.expect("a home request was sent")
    }

    /// Lets the window handle the answer of the request under `ticket`.
    fn answer(&mut self, ticket: u64, result: Result<Value, DltError>) {
        self.replies.send(RpcReply { ticket, result: result.map(RpcPayload::Json) }).expect("the window still listens");
        self.app.process_replies(&self.ctx);
    }

    /// The drafts the file holds now (what a restart would list), read by a second window.
    fn saved(&self) -> Vec<Draft> {
        DraftBook::open(drafts_path(&self.root)).recovered()
    }

    /// Syncs the editors and writes the file now, as a quiet moment would.
    fn settle(&mut self) {
        self.app.sync_drafts();
        self.app.drafts.flush();
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        // The window's own drop writes its drafts; a book without a file writes nothing, so the folder is not made again.
        self.app.drafts = DraftBook::default();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[test]
fn every_kind_of_draft_survives_a_save_and_a_load() {
    let root = workspace();
    let path = drafts_path(&root);
    let memo = CanvasBoardMemo {
        kind: CanvasBoardMemoKind::Memo,
        id: "memo-1".into(),
        page_id: "page-1".into(),
        bounds: EmuRect { x: 10, y: 20, width: 300, height: 200 },
        text: "확인할 부분".into(),
        color: "#FFF2A8".into(),
    };
    let bodies = vec![
        Body::Home { prompt: "로그인 화면".into(), attachments: vec!["C:/refs/a.png".into()], figure_refs: Vec::new() },
        Body::Figure { prompt: "축 정리".into(), attachments: vec!["C:/data/run.csv".into()], figure_refs: Vec::new() },
        Body::Note { memo, page_index: 2, is_new: false },
        Body::Crop { node_id: "node-7".into(), asset_sha256: "ab".repeat(32), fields: ["1".into(), "2".into(), "300".into(), "200".into()] },
        Body::Cell {
            dataset_id: "ds-1".into(),
            version_hash: "v9".into(),
            row_id: "r3".into(),
            column_id: "c2".into(),
            original: "0.41".into(),
            draft: "0.52".into(),
        },
        Body::Inline { node_id: "text-1".into(), page_index: 0, text: "수정 중".into(), committed: "수정".into() },
        Body::PdfEvidence {
            page_index: 4,
            file_name: "paper.pdf".into(),
            asset_sha256: Some("cd".repeat(32)),
            records: 3,
            message: "보류".into(),
        },
    ];
    let now = Instant::now();
    let mut book = DraftBook::open(path.clone());
    for (index, body) in bodies.iter().enumerate() {
        let live = Live {
            artifact_id: Some(format!("doc-{index}")),
            revision: Some(index as u64 + 1),
            body: body.clone(),
        };
        book.sync(&format!("slot-{index}"), Edit::Has(live), now);
    }
    book.flush();
    let reopened = DraftBook::open(path.clone());
    let mut loaded = reopened.recovered();
    assert_eq!(loaded.len(), bodies.len());
    loaded.sort_by(|a, b| a.slot.cmp(&b.slot));
    for (index, draft) in loaded.iter().enumerate() {
        assert_eq!(draft.body, bodies[index], "the {index}th kind comes back as it was saved");
        assert_eq!(draft.artifact_id.as_deref(), Some(format!("doc-{index}").as_str()));
        assert_eq!(draft.revision, Some(index as u64 + 1));
    }
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_crash_before_the_quiet_time_keeps_the_last_written_state() {
    let root = workspace();
    let path = drafts_path(&root);
    let start = Instant::now();
    {
        let mut book = DraftBook::open(path.clone());
        book.sync("home", Edit::Has(home_live("첫 문장")), start);
        book.flush_due(start + Duration::from_millis(600));
        book.sync("home", Edit::Has(home_live("첫 문장 둘째 문장")), start + Duration::from_secs(1));
        book.flush_due(start + Duration::from_millis(1100));
        // The process ends here, without a flush: the second change was not written yet.
    }
    let reopened = DraftBook::open(path);
    let drafts = reopened.recovered();
    assert_eq!(drafts.len(), 1);
    assert_eq!(drafts[0].body, home_body("첫 문장"), "only the written state is recovered");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_draft_that_keeps_changing_is_written_at_the_latest_after_its_first_change() {
    let root = workspace();
    let path = drafts_path(&root);
    let start = Instant::now();
    let mut book = DraftBook::open(path.clone());
    for step in 0..=8u64 {
        let at = start + Duration::from_millis(400 * step);
        book.sync("home", Edit::Has(home_live(&format!("입력 {step}"))), at);
        book.flush_due(at);
        if step < 7 {
            assert!(!path.exists(), "not written while the changes keep coming (step {step})");
        }
    }
    assert!(path.exists(), "written once the first change is three seconds old");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_recovered_draft_goes_into_an_empty_slot_once() {
    let root = workspace();
    let path = drafts_path(&root);
    let now = Instant::now();
    let mut first = DraftBook::open(path.clone());
    first.sync("home", Edit::Has(home_live("보낼 요청")), now);
    first.flush();
    drop(first);

    let mut book = DraftBook::open(path.clone());
    let draft = book.recovered().pop().expect("the saved request");
    book.take(&draft, "home").expect("the empty slot takes the draft");
    assert!(book.held_by_editor(&draft.key));
    assert_eq!(DraftBook::open(path.clone()).recovered()[0].body, home_body("보낼 요청"));
    assert!(book.recovered().is_empty(), "a taken draft is no longer waiting for a decision");
    assert!(book.take(&draft, "home").is_err(), "a taken draft cannot be taken twice");
    book.sync("home", Edit::Has(home_live("보낼 요청 수정")), now + Duration::from_secs(1));
    book.flush();
    let reopened = DraftBook::open(path);
    assert_eq!(reopened.recovered().len(), 1, "the editor's change goes into the same draft");
    assert_eq!(reopened.recovered()[0].body, home_body("보낼 요청 수정"));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_recovered_draft_is_not_taken_into_a_slot_that_holds_content() {
    let root = workspace();
    let path = drafts_path(&root);
    let now = Instant::now();
    let mut first = DraftBook::open(path.clone());
    first.sync("home", Edit::Has(home_live("지난 요청")), now);
    first.flush();
    drop(first);

    let mut book = DraftBook::open(path.clone());
    let draft = book.recovered().pop().expect("the saved request");
    book.sync("home", Edit::Has(home_live("새 요청")), now);
    assert!(book.take(&draft, "home").is_err(), "the slot holds the new request");
    assert_eq!(book.recovered().len(), 1, "the old request stays recovered until the user decides");
    book.flush();
    let reopened = DraftBook::open(path);
    let bodies: Vec<Body> = reopened.recovered().into_iter().map(|draft| draft.body).collect();
    assert_eq!(bodies.len(), 2, "the new request is saved beside the old one");
    assert!(bodies.contains(&home_body("지난 요청")));
    assert!(bodies.contains(&home_body("새 요청")));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn the_revision_a_draft_was_made_against_is_kept_until_its_content_changes() {
    let root = workspace();
    let path = drafts_path(&root);
    let now = Instant::now();
    let files: Vec<String> = Vec::new();
    let mut book = DraftBook::open(path.clone());
    book.sync("figure:figure-1", Edit::Has(figure_live("그래프 제목", &files, 4)), now);
    book.sync("figure:figure-1", Edit::Has(figure_live("그래프 제목", &files, 5)), now + Duration::from_secs(1));
    book.flush();
    let reopened = DraftBook::open(path);
    let drafts = reopened.recovered();
    assert_eq!(drafts[0].revision, Some(4), "a revision change alone does not rebind the draft");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn placement_tells_a_stale_revision_from_the_one_on_screen() {
    assert_eq!(placement("figure-1", 4, Some(("figure-1", 4)), None), Placement::Here);
    assert_eq!(placement("figure-1", 4, Some(("figure-1", 5)), Some(5)), Placement::Stale, "open at another revision");
    assert_eq!(placement("figure-1", 4, None, Some(5)), Placement::Stale, "listed at another revision");
    assert_eq!(placement("figure-1", 4, None, Some(4)), Placement::NotOpen);
    assert_eq!(placement("figure-1", 4, Some(("other", 9)), None), Placement::Missing);
    assert_eq!(placement("figure-1", 4, None, None), Placement::Missing);
}

#[test]
fn a_missing_attachment_is_listed_and_its_draft_stays_recovered() {
    let root = workspace();
    std::fs::create_dir_all(&root).expect("the test folder");
    let present = root.join("evidence.csv");
    std::fs::write(&present, "x,y\n1,2\n").expect("the evidence file");
    let gone = root.join("gone.pdf");
    let paths = vec![present.to_string_lossy().into_owned(), gone.to_string_lossy().into_owned()];
    let (kept, missing) = split_files(&paths);
    assert_eq!(kept, vec![paths[0].clone()]);
    assert_eq!(missing, vec!["gone.pdf".to_owned()]);

    let path = drafts_path(&root);
    let now = Instant::now();
    let mut first = DraftBook::open(path.clone());
    first.sync("figure:figure-1", Edit::Has(figure_live("표 정리", &paths, 2)), now);
    first.flush();
    drop(first);
    let reopened = DraftBook::open(path);
    let drafts = reopened.recovered();
    assert_eq!(drafts.len(), 1, "a missing file does not discard the draft");
    let Body::Figure { attachments, .. } = &drafts[0].body else { panic!("a figure draft") };
    assert_eq!(attachments, &paths, "the list is kept as saved; the window shows what is gone");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn the_draft_file_keeps_file_paths_and_never_file_contents() {
    let root = workspace();
    std::fs::create_dir_all(&root).expect("the test folder");
    let evidence = root.join("evidence.txt");
    std::fs::write(&evidence, "MARKER-7f3a-CONTENT").expect("the evidence file");
    let path = drafts_path(&root);
    let mut book = DraftBook::open(path.clone());
    book.sync(
        "home",
        Edit::Has(Live {
            artifact_id: None,
            revision: None,
            body: Body::Home {
                prompt: "요약".into(),
                attachments: vec![evidence.to_string_lossy().into_owned()],
                figure_refs: Vec::new(),
            },
        }),
        Instant::now(),
    );
    book.flush();
    let text = std::fs::read_to_string(&path).expect("the draft file");
    assert!(text.contains("evidence.txt"), "the path is kept");
    assert!(!text.contains("MARKER-7f3a-CONTENT"), "the file's content is not copied");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn clearing_on_send_removes_the_draft_and_the_same_content_is_not_saved_again() {
    let root = workspace();
    let path = drafts_path(&root);
    let now = Instant::now();
    let files: Vec<String> = Vec::new();
    let mut book = DraftBook::open(path.clone());
    book.sync("figure:figure-1", Edit::Has(figure_live("요청 문장", &files, 4)), now);
    book.flush();
    assert_eq!(DraftBook::open(path.clone()).recovered().len(), 1);

    book.clear("figure:figure-1", Edit::Has(figure_live("요청 문장", &files, 4)), now + Duration::from_secs(1));
    assert!(DraftBook::open(path.clone()).recovered().is_empty(), "the file no longer holds the sent request");

    book.sync("figure:figure-1", Edit::Has(figure_live("요청 문장", &files, 4)), now + Duration::from_secs(2));
    book.flush_due(now + Duration::from_secs(10));
    assert!(DraftBook::open(path.clone()).recovered().is_empty(), "the box still shows what was sent: nothing is saved again");

    book.sync("figure:figure-1", Edit::Has(figure_live("요청 문장 수정", &files, 4)), now + Duration::from_secs(11));
    book.flush();
    assert_eq!(DraftBook::open(path).recovered().len(), 1, "a change after the send is a new draft");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn an_absent_editor_keeps_its_draft_and_an_empty_one_drops_it() {
    let root = workspace();
    let path = drafts_path(&root);
    let now = Instant::now();
    let mut book = DraftBook::open(path.clone());
    book.sync("crop:token-1", Edit::Has(home_live("잘라 둔 값")), now);
    book.sync("crop:token-1", Edit::Absent, now + Duration::from_secs(1));
    book.flush();
    assert_eq!(DraftBook::open(path.clone()).recovered().len(), 1, "a closed-for-now editor keeps its draft");
    book.sync("crop:token-1", Edit::Empty, now + Duration::from_secs(2));
    book.flush();
    assert!(DraftBook::open(path).recovered().is_empty(), "an editor that holds nothing drops its draft");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_discarded_draft_is_gone_after_a_restart() {
    let root = workspace();
    let path = drafts_path(&root);
    let now = Instant::now();
    let mut first = DraftBook::open(path.clone());
    first.sync("home", Edit::Has(home_live("버릴 요청")), now);
    first.flush();
    drop(first);
    let mut book = DraftBook::open(path.clone());
    let key = book.recovered()[0].key.clone();
    book.discard(&key);
    assert!(book.recovered().is_empty());
    assert!(DraftBook::open(path).recovered().is_empty(), "the discard is written at once");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_file_of_a_newer_version_is_never_written_over() {
    let root = workspace();
    std::fs::create_dir_all(root.join(".gui")).expect("the GUI folder");
    let path = drafts_path(&root);
    let newer = r#"{"schema_version": 2, "drafts": [], "future": true}"#;
    std::fs::write(&path, newer).expect("the newer file");
    let mut book = DraftBook::open(path.clone());
    assert!(book.recovered().is_empty());
    assert!(!book.take_notices().is_empty(), "the user is told why nothing is saved");
    book.sync("home", Edit::Has(home_live("새 요청")), Instant::now());
    book.flush();
    assert_eq!(std::fs::read_to_string(&path).expect("the file"), newer);
    let _ = std::fs::remove_dir_all(&root);
}

/// The names in the GUI folder that a file moved aside got.
fn moved_aside_names(root: &Path) -> Vec<String> {
    std::fs::read_dir(root.join(".gui"))
        .expect("the GUI folder")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with("drafts.json.unreadable-"))
        .collect()
}

#[test]
fn an_unreadable_file_is_moved_aside_and_drafts_start_fresh() {
    let root = workspace();
    std::fs::create_dir_all(root.join(".gui")).expect("the GUI folder");
    let path = drafts_path(&root);
    std::fs::write(&path, "not a draft file").expect("the broken file");
    let mut book = DraftBook::open(path.clone());
    assert!(book.recovered().is_empty());
    assert!(!book.take_notices().is_empty());
    assert!(!path.exists(), "the broken file is no longer where drafts are written");
    assert_eq!(moved_aside_names(&root).len(), 1, "the broken file is kept aside, under a timestamped name");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_moved_aside_file_never_takes_the_name_of_an_existing_one() {
    let root = workspace();
    std::fs::create_dir_all(root.join(".gui")).expect("the GUI folder");
    let path = drafts_path(&root);
    let first = quarantine_path(&path, 42);
    std::fs::write(&first, "an earlier broken file").expect("an earlier moved file");
    let second = quarantine_path(&path, 42);
    assert_ne!(first, second, "the same stamp gives another name");
    assert!(!second.exists());
    assert_eq!(std::fs::read_to_string(&first).expect("the earlier file"), "an earlier broken file");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_file_over_the_size_cap_is_moved_aside_without_being_read() {
    let root = workspace();
    std::fs::create_dir_all(root.join(".gui")).expect("the GUI folder");
    let path = drafts_path(&root);
    let large = std::fs::File::create(&path).expect("the large file");
    large.set_len(8 * 1024 * 1024 + 1).expect("the file is over the cap");
    drop(large);
    let mut book = DraftBook::open(path.clone());
    assert!(book.recovered().is_empty());
    assert!(!path.exists(), "the large file is no longer where drafts are read");
    assert!(book.take_notices().iter().any(|notice| notice.contains("8 MiB")), "the user is told about the cap");
    assert_eq!(moved_aside_names(&root).len(), 1);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_damaged_entry_is_skipped_and_the_good_ones_are_kept_with_a_copy_of_the_file() {
    let root = workspace();
    std::fs::create_dir_all(root.join(".gui")).expect("the GUI folder");
    let path = drafts_path(&root);
    let good = serde_json::to_value(Draft {
        key: "good-1".into(),
        slot: "home".into(),
        artifact_id: None,
        revision: None,
        saved_at_ms: 1,
        body: home_body("좋은 요청"),
        sent: None,
        start: None,
        owner: None,
    })
    .expect("a draft");
    let text = json!({ "schema_version": 1, "drafts": [good, { "key": "broken", "slot": 3 }] }).to_string();
    std::fs::write(&path, text).expect("the file");
    let mut book = DraftBook::open(path.clone());
    let recovered = book.recovered();
    assert_eq!(recovered.len(), 1, "the good entry comes back");
    assert_eq!(recovered[0].body, home_body("좋은 요청"));
    assert!(!book.take_notices().is_empty(), "the user is told that an entry was skipped");
    assert_eq!(moved_aside_names(&root).len(), 1, "the file as it was is kept aside");
    book.flush();
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_damaged_file_that_cannot_be_moved_aside_is_not_written_over() {
    let root = workspace();
    std::fs::create_dir_all(root.join(".gui")).expect("the GUI folder");
    let path = drafts_path(&root);
    let mut book = DraftBook::open(path.clone());
    book.sync("home", Edit::Has(home_live("첫 요청")), Instant::now());
    book.flush();
    let damaged = "not a draft file";
    std::fs::write(&path, damaged).expect("the file is damaged after the window wrote it");
    let failing = FailingAside::new(AsideKind::Move);
    book.sync("home", Edit::Has(home_live("둘째 요청")), Instant::now());
    assert!(!book.flush(), "the write is refused while the file cannot be moved");
    drop(failing);
    assert_eq!(std::fs::read_to_string(&path).expect("the file"), damaged, "the damaged file is not written over");
    assert!(book.take_notices().iter().any(|notice| notice.contains("옮기지 못해")), "the user is told that the drafts were not saved");
    assert!(book.flush(), "the next write saves once the file can be moved");
    assert_eq!(moved_aside_names(&root).len(), 1, "the damaged file is moved aside once it can be");
    let aside = root.join(".gui").join(&moved_aside_names(&root)[0]);
    assert_eq!(std::fs::read_to_string(&aside).expect("the moved file"), damaged);
    let recovered = DraftBook::open(path.clone()).recovered();
    assert_eq!(recovered.len(), 1);
    assert_eq!(recovered[0].body, home_body("둘째 요청"));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_damaged_entry_whose_file_cannot_be_copied_aside_is_not_merged_away() {
    let root = workspace();
    std::fs::create_dir_all(root.join(".gui")).expect("the GUI folder");
    let path = drafts_path(&root);
    let good = serde_json::to_value(Draft {
        key: "good-1".into(),
        slot: "home".into(),
        artifact_id: None,
        revision: None,
        saved_at_ms: 1,
        body: home_body("좋은 요청"),
        sent: None,
        start: None,
        owner: None,
    })
    .expect("a draft");
    let damaged = json!({ "schema_version": 1, "drafts": [good, { "key": "broken", "slot": 3 }] }).to_string();
    std::fs::write(&path, &damaged).expect("the file");
    // The copy fails from the open on, so no copy of this content exists when the window writes.
    let failing = FailingAside::new(AsideKind::Copy);
    let mut book = DraftBook::open(path.clone());
    assert_eq!(book.recovered().len(), 1);
    let _ = book.take_notices();
    book.sync("home", Edit::Has(home_live("새 요청")), Instant::now());
    assert!(!book.flush(), "the write is refused while the damaged entry cannot be kept aside");
    drop(failing);
    assert_eq!(std::fs::read_to_string(&path).expect("the file"), damaged, "the file with the damaged entry is not written over");
    assert!(book.take_notices().iter().any(|notice| notice.contains("옆에 남기지")), "the user is told that the drafts were not saved");
    assert!(book.flush(), "the next write saves once the copy can be made");
    let recovered = DraftBook::open(path.clone()).recovered();
    assert_eq!(recovered.len(), 2, "the good entry and the new draft are saved once the copy can be made");
    assert_eq!(moved_aside_names(&root).len(), 1, "the file as it was is kept aside once, by the write that could copy it");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_write_that_keeps_failing_copies_the_damaged_file_aside_once_and_tells_once() {
    let root = workspace();
    std::fs::create_dir_all(root.join(".gui")).expect("the GUI folder");
    let path = drafts_path(&root);
    let good = serde_json::to_value(Draft {
        key: "good-1".into(),
        slot: "home".into(),
        artifact_id: None,
        revision: None,
        saved_at_ms: 1,
        body: home_body("좋은 요청"),
        sent: None,
        start: None,
        owner: None,
    })
    .expect("a draft");
    let damaged = json!({ "schema_version": 1, "drafts": [good, { "key": "broken", "slot": 3 }] }).to_string();
    std::fs::write(&path, &damaged).expect("the file");
    let mut book = DraftBook::open(path.clone());
    let _ = book.take_notices();
    assert_eq!(moved_aside_names(&root).len(), 1, "the open keeps the file as it was aside");
    let failing = FailingAside::new(AsideKind::Replace);
    book.sync("home", Edit::Has(home_live("새 요청")), Instant::now());
    for _ in 0..3 {
        assert!(!book.flush(), "the write fails");
    }
    assert!(book.save_failing());
    assert_eq!(moved_aside_names(&root).len(), 1, "a retried write does not copy the same file aside again");
    let told = book.take_notices().iter().filter(|notice| notice.starts_with("초안을 저장하지 못했습니다")).count();
    assert_eq!(told, 1, "a run of failed writes is told once");
    drop(failing);
    assert!(book.flush(), "the next write saves");
    assert!(!book.save_failing(), "the failure has ended");
    assert_eq!(moved_aside_names(&root).len(), 1);
    assert_eq!(DraftBook::open(path.clone()).recovered().len(), 2, "the good entry and the new draft are saved");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn damage_that_comes_back_after_a_save_is_copied_aside_again() {
    let root = workspace();
    std::fs::create_dir_all(root.join(".gui")).expect("the GUI folder");
    let path = drafts_path(&root);
    let damaged = json!({ "schema_version": 1, "drafts": [{ "key": "broken", "slot": 3 }] }).to_string();
    std::fs::write(&path, &damaged).expect("the file");
    let mut book = DraftBook::open(path.clone());
    let _ = book.take_notices();
    book.sync("home", Edit::Has(home_live("새 요청")), Instant::now());
    assert!(book.flush(), "the write saves");
    assert_eq!(moved_aside_names(&root).len(), 1, "the open kept the damaged file aside, and the write does not copy it again");

    // The same damaged bytes are put back (by hand, or by another tool): this is new damage, and it is kept aside before
    // the next write goes over it.
    std::fs::write(&path, &damaged).expect("the damage again");
    book.sync("home", Edit::Has(home_live("새 요청 수정")), Instant::now());
    assert!(book.flush(), "the write saves");
    assert_eq!(moved_aside_names(&root).len(), 2, "the damage that came back is copied aside too");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_write_while_another_window_holds_the_lock_gives_up_in_time_and_keeps_the_draft() {
    let root = workspace();
    let path = drafts_path(&root);
    let mut book = DraftBook::open(path.clone());
    book.sync("home", Edit::Has(home_live("잠금 뒤 요청")), Instant::now());
    let held = lock_drafts(&path).expect("the lock file").expect("this file system locks");
    let started = Instant::now();
    assert!(!book.flush(), "the write does not wait for the lock without end");
    assert!(started.elapsed() < Duration::from_secs(2), "the window's thread is back in time: {:?}", started.elapsed());
    assert!(!path.exists(), "nothing is read, merged or replaced while the lock is held");
    assert!(book.save_failing(), "the drafts are not on disk, so a close asks first");
    assert_eq!(book.take_notices().len(), 1, "the user is told once");
    assert!(!book.flush(), "a second try while the lock is held fails too");
    assert!(book.take_notices().is_empty(), "and is not told again");

    drop(held);
    assert!(book.flush(), "the next write saves once the lock is free");
    assert!(!book.save_failing());
    let saved = DraftBook::open(path.clone()).recovered();
    assert_eq!(saved.len(), 1, "the draft kept in memory is in the file");
    assert_eq!(saved[0].body, home_body("잠금 뒤 요청"));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn closing_while_the_drafts_cannot_be_written_keeps_the_window_open_and_asks() {
    let mut h = Harness::new();
    h.app.drafts.sync("home", Edit::Has(home_live("닫기 전 요청")), Instant::now());
    block_draft_folder(&h.root, true);
    let mut refused = RawInput::default();
    refused.viewports.get_mut(&ViewportId::ROOT).expect("root viewport").events.push(ViewportEvent::Close);
    let ctx = h.ctx.clone();
    let closing = ctx.run(refused, |ctx| h.app.guard_held_edit_close(ctx));
    let cancelled = |output: &eframe::egui::FullOutput| {
        output.viewport_output.get(&ViewportId::ROOT).is_some_and(|viewport| viewport.commands.contains(&ViewportCommand::CancelClose))
    };
    assert!(cancelled(&closing), "the window stays open while the drafts are not on disk");
    assert!(h.app.close_confirmation_open, "the dialog asks before the window closes");
    block_draft_folder(&h.root, false);
    let mut again = RawInput::default();
    again.viewports.get_mut(&ViewportId::ROOT).expect("root viewport").events.push(ViewportEvent::Close);
    let closed = ctx.run(again, |ctx| h.app.guard_held_edit_close(ctx));
    assert!(!cancelled(&closed), "once the drafts are written, the close goes through");
    assert_eq!(h.saved().len(), 1, "the draft is in the file");
}

#[test]
fn two_windows_on_one_workspace_merge_their_drafts_by_key() {
    let root = workspace();
    let path = drafts_path(&root);
    let now = Instant::now();
    let mut first = DraftBook::open(path.clone());
    let mut second = DraftBook::open(path.clone());
    first.sync("home", Edit::Has(home_live("첫 창의 요청")), now);
    first.flush();
    second.sync("figure:figure-1", Edit::Has(figure_live("둘째 창의 요청", &[], 2)), now);
    second.flush();
    assert_eq!(DraftBook::open(path.clone()).recovered().len(), 2, "the second window keeps the first window's draft");

    first.sync("home", Edit::Has(home_live("첫 창의 요청 수정")), now + Duration::from_secs(1));
    first.flush();
    let saved = DraftBook::open(path.clone()).recovered();
    assert_eq!(saved.len(), 2, "the first window's change is written and the other draft stays");
    assert!(saved.iter().any(|draft| draft.body == home_body("첫 창의 요청 수정")));
    assert!(saved.iter().any(|draft| matches!(draft.body, Body::Figure { .. })));

    first.clear("home", Edit::Absent, now + Duration::from_secs(2));
    let left = DraftBook::open(path).recovered();
    assert_eq!(left.len(), 1, "a draft this window removed is dropped; the other window's draft stays");
    assert!(matches!(left[0].body, Body::Figure { .. }));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn the_pdf_evidence_group_drops_a_set_that_has_left() {
    let root = workspace();
    let path = drafts_path(&root);
    let now = Instant::now();
    let held = |id: &str| {
        (
            format!("pdf:{id}"),
            Live {
                artifact_id: Some("figure-1".into()),
                revision: Some(3),
                body: Body::PdfEvidence {
                    page_index: 1,
                    file_name: "paper.pdf".into(),
                    asset_sha256: None,
                    records: 2,
                    message: String::new(),
                },
            },
        )
    };
    let mut book = DraftBook::open(path.clone());
    book.sync_group("pdf:", vec![held("a"), held("b")], now);
    book.flush();
    assert_eq!(DraftBook::open(path.clone()).recovered().len(), 2);
    book.sync_group("pdf:", vec![held("b")], now + Duration::from_secs(1));
    book.flush();
    assert_eq!(DraftBook::open(path).recovered().len(), 1, "the set that was put into its figure is gone");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_changed_request_drops_its_mark_and_its_answer_does_not_remove_the_newer_draft() {
    let root = workspace();
    let path = drafts_path(&root);
    let now = Instant::now();
    let mut book = DraftBook::open(path.clone());
    let sent = figure_live("축 정리", &[], 3);
    book.sync("figure:figure-1", Edit::Has(sent.clone()), now);
    book.begin_send(7, "figure:figure-1", sent, "request-1", true, now).expect("written");
    assert_eq!(DraftBook::open(path.clone()).recovered()[0].sent.as_deref(), Some("request-1"));
    book.sync("figure:figure-1", Edit::Has(figure_live("축 정리 수정", &[], 3)), now + Duration::from_secs(1));
    book.flush();
    assert!(DraftBook::open(path.clone()).recovered()[0].sent.is_none(), "a changed request is no longer the one sent");
    book.answered(7, Outcome::Accepted, None, now + Duration::from_secs(2));
    assert_eq!(DraftBook::open(path).recovered().len(), 1, "an answer for changed text does not remove the newer draft");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_sent_draft_is_written_before_the_answer_and_a_restart_does_not_restore_it() {
    let root = workspace();
    let path = drafts_path(&root);
    let now = Instant::now();
    let mut book = DraftBook::open(path.clone());
    book.begin_send(3, "home", home_live("보낸 요청"), "request-9", false, now).expect("written");
    // Nothing is answered: the process ends here.
    drop(book);
    let restarted = DraftBook::open(path);
    let drafts = restarted.recovered();
    assert_eq!(drafts.len(), 1, "the request is in the file before its answer");
    assert_eq!(drafts[0].sent.as_deref(), Some("request-9"), "the mark says the request may have been sent");
    assert_eq!(drafts[0].body, home_body("보낸 요청"));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn sync_drafts_keeps_each_document_s_draft_under_its_own_id_when_the_window_switches() {
    let mut h = Harness::new();
    h.app.home.prompt = "홈 문장".into();
    h.app.active = Some(summary("figure-1", ArtifactKind::Figure, 3));
    h.app.ai_prompt = "그림 문장".into();
    h.settle();
    h.app.show_artifact(summary("canvas-1", ArtifactKind::Canvas, 1));
    assert!(h.app.ai_prompt.is_empty(), "the shared prompt is not carried to another document");
    h.settle();
    let saved = h.saved();
    let figure = saved.iter().find(|draft| matches!(draft.body, Body::Figure { .. })).expect("the figure's draft");
    assert_eq!(figure.artifact_id.as_deref(), Some("figure-1"), "the figure's text is saved under the figure");
    assert!(matches!(&figure.body, Body::Figure { prompt, .. } if prompt == "그림 문장"));
    assert!(saved.iter().any(|draft| draft.body == home_body("홈 문장")), "the home box is still saved");
}

#[test]
fn home_request_keeps_the_text_typed_while_it_waits_as_a_new_draft() {
    let mut h = Harness::new();
    let ticket = h.create_home("첫 요청");
    assert!(h.app.home.prompt.is_empty(), "the box gave up its text for the request");
    h.app.home.prompt = " 추가 문장".into();
    h.settle();
    h.answer(ticket, Ok(canvas_answer("canvas-1")));
    h.settle();
    assert_eq!(h.app.home.prompt, " 추가 문장", "the text typed meanwhile stays in the box");
    let saved = h.saved();
    assert_eq!(saved.len(), 1, "the accepted request's draft is done");
    assert_eq!(saved[0].body, home_body(" 추가 문장"));
    assert!(saved[0].sent.is_none());
}

#[test]
fn home_request_refused_returns_its_files_merged_with_the_ones_added_meanwhile() {
    let mut h = Harness::new();
    h.app.home.attachments = vec!["C:/refs/a.png".into()];
    let ticket = h.create_home("요청");
    assert!(h.app.home.attachments.is_empty());
    h.app.home.attachments.push("C:/refs/b.png".into());
    h.answer(ticket, Err(DltError::new(ErrorCode::InvalidArgument, "refused")));
    assert_eq!(h.app.home.attachments, ["C:/refs/a.png", "C:/refs/b.png"], "nothing added meanwhile is lost");
    assert_eq!(h.app.home.prompt, "요청", "the text comes back to the box");
    h.settle();
    let saved = h.saved();
    assert_eq!(saved.len(), 1, "the box's content is the one draft now");
    assert!(matches!(&saved[0].body, Body::Home { attachments, .. } if attachments.len() == 2));
    assert!(saved[0].sent.is_none());
}

#[test]
fn a_home_request_with_other_text_than_the_box_leaves_the_box_text_in_place() {
    let mut h = Harness::new();
    h.app.home.prompt = "박스 문장".into();
    h.app.create_from_home("blank", "명령 문장".into()).expect("the request is sent");
    let ticket = h.last_home_ticket();
    assert_eq!(h.app.home.prompt, "박스 문장", "the box keeps its own text");
    h.settle();
    assert!(h.saved().iter().any(|draft| draft.sent.is_some() && draft.body == home_body("명령 문장")), "the request has its own draft");
    h.answer(ticket, Err(DltError::new(ErrorCode::InvalidArgument, "refused")));
    assert_eq!(h.app.home.prompt, "명령 문장\n박스 문장", "a refused request's text comes back ahead of the box's text");
}

#[test]
fn two_home_requests_on_their_way_do_not_clear_each_other() {
    let mut h = Harness::new();
    let first = h.create_home("첫째 요청");
    h.app.home.prompt = "둘째 요청".into();
    let second = h.create_home("둘째 요청");
    assert_ne!(first, second);
    h.answer(first, Ok(canvas_answer("canvas-1")));
    h.settle();
    let saved = h.saved();
    assert_eq!(saved.len(), 1, "only the second request is still on its way");
    assert!(matches!(&saved[0].body, Body::Home { prompt, .. } if prompt == "둘째 요청"));
    assert!(saved[0].sent.is_some(), "the second request keeps its mark");
    h.answer(second, Err(DltError::new(ErrorCode::InvalidArgument, "refused")));
    assert_eq!(h.app.home.prompt, "둘째 요청", "the refused request comes back to the box");
}

#[test]
fn an_unknown_home_answer_is_not_put_back_and_the_draft_says_it_may_have_been_sent() {
    let mut h = Harness::new();
    let ticket = h.create_home("요청");
    h.answer(ticket, Err(DltError::new(ErrorCode::Unavailable, "no answer")));
    assert!(h.app.home.prompt.is_empty(), "a request whose outcome is unknown is not sent again from the box");
    let saved = h.saved();
    assert_eq!(saved.len(), 1, "the draft stays for the user to check");
    assert!(saved[0].sent.is_some());
    let standing = h.app.draft_standing(&saved[0]);
    assert!(!standing.ready, "it is not restored");
    assert!(standing.reason.is_some(), "the reason is shown");
}

#[test]
fn home_request_is_written_with_its_mark_before_it_goes_out() {
    let mut h = Harness::new();
    let _ticket = h.create_home("요청");
    // Nothing is answered yet: a crash now must not bring the request back as an ordinary draft.
    let saved = h.saved();
    assert_eq!(saved.len(), 1, "the request is in the file before the answer");
    assert!(saved[0].sent.is_some());
    assert!(!h.app.draft_standing(&saved[0]).ready, "a draft that may have been sent is never restored");
}

#[test]
fn restore_draft_puts_a_home_request_back_only_on_the_home_screen_and_never_replaces_shared_references() {
    let mut h = Harness::new();
    let path = drafts_path(&h.root);
    let mut book = DraftBook::open(path.clone());
    let live = Live { artifact_id: None, revision: None, body: Body::Home { prompt: "지난 요청".into(), attachments: Vec::new(), figure_refs: vec![reference(4)] } };
    book.sync("home", Edit::Has(live), Instant::now());
    book.flush();
    drop(book);
    h.app.drafts = DraftBook::open(path);
    let key = h.app.drafts.recovered()[0].key.clone();

    h.app.screen = Screen::Editor;
    h.app.restore_draft(&key);
    assert!(h.app.home.prompt.is_empty(), "a home draft is not restored over an open document");

    h.app.screen = Screen::Home;
    h.app.ai_figure_refs = vec![reference(9)];
    h.app.restore_draft(&key);
    assert!(h.app.home.prompt.is_empty(), "a box with references in use is not replaced");
    assert_eq!(h.app.ai_figure_refs, [reference(9)], "the shared references are left alone");

    h.app.ai_figure_refs.clear();
    h.app.restore_draft(&key);
    assert_eq!(h.app.home.prompt, "지난 요청");
    assert_eq!(h.app.ai_figure_refs, [reference(4)], "the draft's references are added");
}

#[test]
fn a_refused_cell_value_comes_back_to_its_editor_and_stays_in_its_draft_until_the_answer() {
    let mut h = Harness::new();
    h.app.active = Some(summary("figure-1", ArtifactKind::Figure, 3));
    // The commit closes the editor and sends the value: the value is written with the request mark first.
    let edit = cell_edit("0.25");
    h.app.dataset_edit = None;
    let live = Live { artifact_id: Some("figure-1".into()), revision: Some(3), body: super::drafts::cell_body(&edit) };
    let ticket = h.app.next_ticket;
    h.app.drafts.begin_send(ticket, "cell", live, "cell-request-1", false, Instant::now()).expect("written");
    let waiting = h.saved();
    assert_eq!(waiting.len(), 1, "the value is in the file until the answer");
    assert!(waiting[0].sent.is_some());
    h.app.answer_sent_outcome(ticket, Outcome::Refused);
    assert_eq!(h.app.dataset_edit.as_ref().map(|edit| edit.draft.as_str()), Some("0.25"), "the editor gets the value back");
    assert!(h.saved().is_empty(), "the value is no longer kept twice");
}

#[test]
fn a_refused_cell_value_whose_table_is_gone_stays_as_a_copy_for_the_user() {
    let mut h = Harness::new();
    h.app.active = Some(summary("figure-2", ArtifactKind::Figure, 5));
    let edit = cell_edit("0.25");
    let live = Live { artifact_id: Some("figure-1".into()), revision: Some(3), body: super::drafts::cell_body(&edit) };
    let ticket = h.app.next_ticket;
    h.app.drafts.begin_send(ticket, "cell", live, "cell-request-2", false, Instant::now()).expect("written");
    h.app.answer_sent_outcome(ticket, Outcome::Refused);
    assert!(h.app.dataset_edit.is_none(), "another document's table is not opened with the value");
    let kept = h.saved();
    assert_eq!(kept.len(), 1, "the value is kept as a recovered draft");
    assert!(kept[0].sent.is_none());
    assert!(!h.app.draft_standing(&kept[0]).ready, "it can only be copied or discarded");
}

#[test]
fn a_figure_request_accepted_after_its_editor_was_left_clears_its_draft() {
    let mut h = Harness::new();
    h.app.active = Some(summary("figure-1", ArtifactKind::Figure, 3));
    h.app.ai_prompt = "그래프 정리".into();
    h.settle();
    let ticket = h.app.next_ticket;
    let live = figure_live("그래프 정리", &[], 3);
    h.app.drafts.begin_send(ticket, &figure_slot("figure-1"), live, "job-request-1", true, Instant::now()).expect("written");
    // The window leaves the figure before the answer comes.
    h.app.show_artifact(summary("canvas-1", ArtifactKind::Canvas, 1));
    assert_eq!(h.saved().len(), 1, "the request is still in the file");
    h.app.answer_sent_drafts(ticket, &Ok(RpcPayload::Json(json!({ "id": "job-1" }))));
    h.settle();
    assert!(h.saved().is_empty(), "the accepted request's draft is done, though its editor was left");
}

#[test]
fn text_typed_into_the_figure_tab_while_its_request_is_on_its_way_stays_a_draft_of_its_own() {
    let mut h = Harness::new();
    h.app.active = Some(summary("figure-1", ArtifactKind::Figure, 3));
    h.app.ai_prompt = "요청 문장".into();
    h.settle();
    // The tab's text grows while the evidence is imported: the request still carries the text it started with.
    h.app.ai_prompt = "요청 문장 더 쓴 문장".into();
    h.settle();
    let ticket = h.app.next_ticket;
    h.app.drafts.begin_send(ticket, &figure_slot("figure-1"), figure_live("요청 문장", &[], 3), "job-request-2", true, Instant::now()).expect("written");
    let saved = h.saved();
    assert_eq!(saved.len(), 2, "the sent text and the newer text are two drafts");
    assert!(saved.iter().any(|draft| draft.sent.as_deref() == Some("job-request-2") && matches!(&draft.body, Body::Figure { prompt, .. } if prompt == "요청 문장")));
    assert!(saved.iter().any(|draft| draft.sent.is_none() && matches!(&draft.body, Body::Figure { prompt, .. } if prompt.ends_with("더 쓴 문장"))));
    h.app.answer_sent_drafts(ticket, &Ok(RpcPayload::Json(Value::Null)));
    h.settle();
    let left = h.saved();
    assert_eq!(left.len(), 1, "the accepted request's draft is done");
    assert!(matches!(&left[0].body, Body::Figure { prompt, .. } if prompt.ends_with("더 쓴 문장")), "the newer text is kept");
    assert_eq!(h.app.ai_prompt, "요청 문장 더 쓴 문장", "the tab keeps its text");
}

#[test]
fn a_consented_discard_of_the_figure_request_leaves_the_file_at_once() {
    let mut h = Harness::new();
    h.app.active = Some(summary("figure-1", ArtifactKind::Figure, 3));
    h.app.ai_prompt = "버릴 요청".into();
    h.settle();
    assert_eq!(h.saved().len(), 1);
    let _ = h.app.discard_figure_work();
    assert!(h.saved().is_empty(), "the discard is written at once, not at the next quiet time");
    assert!(h.app.ai_prompt.is_empty());
}

#[test]
fn a_figure_draft_that_is_only_listed_is_restored_into_its_tab_when_its_figure_is_open_again() {
    let mut h = Harness::new();
    let mut book = DraftBook::open(drafts_path(&h.root));
    book.sync("figure:figure-1", Edit::Has(figure_live("다시 열 요청", &[], 3)), Instant::now());
    book.flush();
    drop(book);
    h.app.drafts = DraftBook::open(drafts_path(&h.root));
    h.app.artifacts = vec![summary("figure-1", ArtifactKind::Figure, 3)];
    h.app.active = Some(summary("figure-1", ArtifactKind::Figure, 3));
    let key = h.app.drafts.recovered()[0].key.clone();
    h.app.restore_draft(&key);
    assert_eq!(h.app.ai_prompt, "다시 열 요청", "the tab gets the text back");
    assert!(h.app.drafts.recovered().is_empty());
}

/// A figure whose AI tab holds `prompt`, with no evidence files: the window as it is before a request.
fn figure_tab(h: &mut Harness, prompt: &str) {
    h.app.screen = Screen::Editor;
    h.app.active = Some(summary("figure-1", ArtifactKind::Figure, 3));
    h.app.ai_prompt = prompt.into();
    h.settle();
}

#[test]
fn a_figure_request_without_files_is_marked_in_its_draft_before_it_goes_out() {
    let mut h = Harness::new();
    figure_tab(&mut h, "축 정리");
    let ticket = h.app.next_ticket;
    h.app.start_ai_job();
    let waiting = h.saved();
    assert_eq!(waiting.len(), 1, "the request is in the file before its answer");
    let start = h.work.try_iter().find_map(|work| match work {
        Work::Call { method, params, .. } if method == "job.start" => Some(params),
        _ => None,
    });
    let start = start.expect("the start is sent");
    assert_eq!(waiting[0].sent.as_deref(), start["request_id"].as_str(), "the draft carries the id the start was sent with");
    assert!(matches!(&waiting[0].body, Body::Figure { prompt, .. } if prompt == "축 정리"));
    h.answer(ticket, Ok(json!({ "id": "job-1" })));
    h.settle();
    assert!(h.saved().is_empty(), "an accepted request's draft is done");
}

#[test]
fn a_figure_request_without_files_stays_marked_after_its_figure_is_left_and_is_done_once_accepted() {
    let mut h = Harness::new();
    figure_tab(&mut h, "축 정리");
    let ticket = h.app.next_ticket;
    h.app.start_ai_job();
    h.app.show_artifact(summary("canvas-1", ArtifactKind::Canvas, 1));
    let left = h.saved();
    assert_eq!(left.len(), 1, "the request is still in the file");
    assert!(left[0].sent.is_some(), "it is not an ordinary draft that a restart would offer for restoring");
    h.answer(ticket, Ok(json!({ "id": "job-1" })));
    h.settle();
    assert!(h.saved().is_empty(), "accepted after its figure was left: the draft is done");
}

#[test]
fn a_figure_request_without_files_that_is_refused_goes_back_to_its_tab_as_text() {
    let mut h = Harness::new();
    figure_tab(&mut h, "축 정리");
    let ticket = h.app.next_ticket;
    h.app.start_ai_job();
    h.answer(ticket, Err(DltError::new(ErrorCode::InvalidArgument, "refused")));
    h.settle();
    assert_eq!(h.app.ai_prompt, "축 정리", "the tab keeps its text");
    let saved = h.saved();
    assert_eq!(saved.len(), 1, "the text is one draft, the tab's own");
    assert!(saved[0].sent.is_none(), "a refused request is no longer marked");
}

/// A new memo with text on the canvas the window shows: its editor holds a draft.
fn new_memo_editor(text: &str) -> BoardMemoEditor {
    BoardMemoEditor {
        artifact_id: "canvas-1".into(),
        revision: 3,
        page_id: "page-1".into(),
        page_index: 0,
        expected: Vec::new(),
        memo: CanvasBoardMemo {
            kind: CanvasBoardMemoKind::Memo,
            id: "memo-1".into(),
            page_id: "page-1".into(),
            bounds: EmuRect { x: 10, y: 20, width: 300, height: 200 },
            text: text.into(),
            color: "#FFF2A8".into(),
        },
        is_new: true,
        message: None,
    }
}

/// An image crop with a typed change, on the image of the figure the window shows.
fn crop_editor(token: &str) -> ImageCropEditor {
    ImageCropEditor {
        intent: ImageReplacementIntent {
            token: token.into(),
            artifact_id: "figure-1".into(),
            expected_revision: 3,
            page_index: 0,
            page_id: "page-1".into(),
            node_id: "node-7".into(),
            expected_asset_sha256: "ab".repeat(32),
            selection_generation: 0,
            view_id: "view-1".into(),
        },
        stage: ImageCropStage::Editing,
        original: None,
        dimensions: Some((100, 80)),
        draft: Some(["1".into(), "2".into(), "30".into(), "40".into()]),
        requested_crop: None,
        outcome_uncertain: false,
        refresh_requested: false,
        refresh_failed: false,
        message: None,
    }
}

/// PDF evidence read for the figure, not yet put into it.
fn held_pdf() -> HeldPdfEvidence {
    HeldPdfEvidence {
        id: "held-1".into(),
        target: PdfEvidenceTarget {
            artifact_id: "figure-1".into(),
            revision: 3,
            page_index: 0,
            file_name: "paper.pdf".into(),
            asset_sha256: None,
            awaiting: None,
        },
        records: Vec::new(),
        message: String::new(),
        ticket: None,
    }
}

/// An inline text session on a canvas node, as the text editor opens it.
fn inline_session(committed: &str) -> InlineTextEdit {
    InlineTextEdit {
        id: 4,
        view_id: "view-1".into(),
        actor: "desktop:inline:view-1".into(),
        artifact_id: "canvas-1".into(),
        node_id: "text-1".into(),
        page_index: 0,
        kind: ArtifactKind::Canvas,
        revision: 1,
        committed_text: committed.into(),
        queue: VecDeque::new(),
        in_flight: None,
        composing: false,
        ime_commit_pending: false,
        ime_cancelled_frame: false,
        ime_generation: 0,
        reservation_id: None,
        reservation_node_ids: Vec::new(),
        reservation_pending: false,
        reservation_release_pending: false,
        reservation_release_failed: false,
        release_when_reserved: false,
        finish_requested: false,
        layout_pending: false,
        conflict: None,
        retry_allowed: false,
        retry_expected: false,
        retry_same_request: false,
        reservation_expires: None,
    }
}

#[test]
fn a_consented_discard_of_held_edits_takes_their_drafts_out_of_the_file() {
    let mut h = Harness::new();
    h.app.board_memo_editor = Some(new_memo_editor("버릴 메모"));
    h.app.image_crop_editor = Some(crop_editor("crop-1"));
    h.app.held_pdf_evidence.push(held_pdf());
    h.settle();
    assert_eq!(h.saved().len(), 3, "the memo, the crop and the PDF records are in the file");
    h.app.discard_held_edits_and_close(&Context::default());
    assert!(h.app.board_memo_editor.is_none() && h.app.image_crop_editor.is_none() && h.app.held_pdf_evidence.is_empty());
    assert!(h.saved().is_empty(), "the discarded drafts are not written back when the window closes");
}

#[test]
fn closing_an_inline_text_session_takes_the_text_it_drops_out_of_the_file() {
    let mut h = Harness::new();
    h.app.inline_text_edit = Some(inline_session("원래 글"));
    h.app.text_draft = "고친 글".into();
    h.settle();
    assert_eq!(h.saved().len(), 1, "the typed text is in the file");
    h.app.finalize_close_after_inline_cleanup(&Context::default());
    assert!(h.app.inline_text_edit.is_none());
    assert!(h.saved().is_empty(), "the text the session drops is not written back");
}

#[test]
fn leaving_after_a_failed_release_drops_the_board_draft_and_keeps_the_pdf_records_held() {
    let mut h = Harness::new();
    h.app.board_memo_editor = Some(new_memo_editor("버릴 메모"));
    h.app.held_pdf_evidence.push(held_pdf());
    h.settle();
    assert_eq!(h.saved().len(), 2, "the memo and the PDF records are in the file");
    h.app.force_close_after_release_failure(&Context::default());
    assert!(h.app.board_memo_editor.is_none());
    let left = h.saved();
    assert_eq!(left.len(), 1, "only the PDF records remain, as they were held");
    assert!(matches!(left[0].body, Body::PdfEvidence { .. }));
}

/// The request ids of the `job.start` calls the window sent since the last call of this helper.
fn sent_start_ids(h: &mut Harness) -> Vec<String> {
    h.work
        .try_iter()
        .filter_map(|work| match work {
            Work::Call { method, params, .. } if method == "job.start" => params["request_id"].as_str().map(str::to_owned),
            _ => None,
        })
        .collect()
}

/// Makes the draft folder of the window unusable (a file stands where the folder is), or usable again.
fn block_draft_folder(root: &Path, blocked: bool) {
    let folder = root.join(".gui");
    if blocked {
        let _ = std::fs::remove_dir_all(&folder);
        std::fs::write(&folder, "a file, not a folder").expect("a file takes the folder's place");
    } else {
        std::fs::remove_file(&folder).expect("the file that took the folder's place is removed");
    }
}

/// The window's worker is gone: a call cannot be handed to it. `restore_worker` gives the window a working one again.
fn lose_worker(h: &mut Harness) {
    h.work = std::sync::mpsc::channel::<Work>().1;
}

fn restore_worker(h: &mut Harness) {
    let (rpc, replies, work) = RpcWorker::detached();
    h.app.rpc = rpc;
    h.replies = replies;
    h.work = work;
}

#[test]
fn a_start_whose_mark_cannot_be_written_keeps_the_id_of_the_unanswered_start_it_repeats() {
    let mut h = Harness::new();
    figure_tab(&mut h, "축 정리");
    let ticket = h.app.next_ticket;
    h.app.start_ai_job();
    let first = sent_start_ids(&mut h);
    assert_eq!(first.len(), 1, "the first start is sent");
    h.answer(ticket, Err(DltError::new(ErrorCode::Unavailable, "no answer")));
    assert!(h.app.unknown_starts.contains_key("figure-1"), "an unanswered start is remembered with its id");

    block_draft_folder(&h.root, true);
    h.app.start_ai_job();
    assert!(sent_start_ids(&mut h).is_empty(), "a start whose mark cannot be written does not go out");
    assert!(h.app.unknown_starts.contains_key("figure-1"), "a failed try does not end the record of the unanswered start");

    block_draft_folder(&h.root, false);
    h.app.start_ai_job();
    assert_eq!(sent_start_ids(&mut h), first, "the next try repeats that id, so the host makes no second job");
}

#[test]
fn a_start_the_worker_does_not_take_keeps_the_id_of_the_unanswered_start_it_repeats() {
    let mut h = Harness::new();
    figure_tab(&mut h, "축 정리");
    let ticket = h.app.next_ticket;
    h.app.start_ai_job();
    let first = sent_start_ids(&mut h);
    h.answer(ticket, Err(DltError::new(ErrorCode::Unavailable, "no answer")));
    lose_worker(&mut h);
    h.app.start_ai_job();
    assert!(h.app.unknown_starts.contains_key("figure-1"), "a start the worker did not take keeps the record");
    assert_eq!(h.saved()[0].sent.as_ref(), Some(&first[0]), "the unsent retry keeps the original mark on disk");
    restore_worker(&mut h);
    h.app.start_ai_job();
    assert_eq!(sent_start_ids(&mut h), first, "the next try repeats that id, so the host makes no second job");
}

#[test]
fn a_board_request_keeps_the_id_of_the_unanswered_start_it_repeats_when_the_worker_does_not_take_it() {
    let mut h = Harness::new();
    figure_tab(&mut h, "축 정리");
    let figure_ticket = request_from_tab(&mut h, "축 정리");
    let figure = sent_start_ids(&mut h);
    h.answer(figure_ticket, Err(DltError::new(ErrorCode::Unavailable, "no answer")));
    h.app.leave_drafts_for_switch();
    h.app.active = Some(summary("canvas-1", ArtifactKind::Canvas, 1));
    h.app.allow_data = true;
    let ticket = request_from_tab(&mut h, "보드 요청");
    let first = sent_start_ids(&mut h);
    h.answer(ticket, Err(DltError::new(ErrorCode::Unavailable, "no answer")));
    lose_worker(&mut h);
    h.app.start_ai_job();
    assert!(h.app.unknown_starts.contains_key("canvas-1"), "a start the worker did not take keeps the record");
    // 48b: the board request has a draft of its own, marked with the id the unsent retry left (before 48b it had none).
    let marks = |saved: &[Draft], artifact_id: &str| saved.iter().find(|draft| draft.artifact_id.as_deref() == Some(artifact_id)).and_then(|draft| draft.sent.clone());
    let saved = h.saved();
    assert_eq!(saved.len(), 2, "the figure's draft and the board request's draft");
    assert_eq!(marks(&saved, "figure-1").as_ref(), Some(&figure[0]), "the figure draft keeps its mark");
    assert_eq!(marks(&saved, "canvas-1").as_ref(), Some(&first[0]), "the board draft keeps the mark of the unanswered start");
    restore_worker(&mut h);
    let retry = request_from_tab(&mut h, "보드 요청");
    assert_eq!(sent_start_ids(&mut h), first, "the next try repeats that id, so the host makes no second job");

    h.answer(retry, Err(DltError::new(ErrorCode::Unavailable, "no answer")));
    h.app.allow_data = false;
    request_from_tab(&mut h, "보드 요청");
    let notice = h.app.notices.current(Instant::now()).expect("the changed settings show a warning");
    assert_eq!(notice.text, UNKNOWN_START_MAY_RUN, "the board request is sent from its draft, as a figure's is");
}

#[test]
fn a_change_that_lands_while_its_inline_session_closes_leaves_no_draft_after_a_forced_close() {
    let mut h = Harness::new();
    // The document is at revision 3 while the change to the text (revision 2) is confirmed: the session stays open for it.
    h.app.active = Some(summary("canvas-1", ArtifactKind::Canvas, 3));
    let mut session = inline_session("A");
    session.in_flight = Some(TextInputEdit {
        request_id: "edit-1".into(),
        range: 0..0,
        paragraph_index: 0,
        paragraph_range: None,
        caret_after: 2,
        paragraph_after: 0,
        caret_after_in_paragraph: 2,
        text: "B".into(),
        ime_commit: false,
        ime_generation: 0,
        after_text: Some("AB".into()),
    });
    session.reservation_id = Some("reservation-1".into());
    h.app.inline_text_edit = Some(session);
    // The window closes the session (`start_inline_text_close_cleanup`): its editor shows the committed text.
    h.app.close_after_inline_cleanup = true;
    h.app.close_inline_session_id = Some(4);
    h.app.close_confirmation_open = true;
    h.app.text_draft = "A".into();
    h.app.complete_inline_text_change(4, &json!({ "revision": 2 }));
    h.settle();
    h.app.fail_close_inline_release(4);
    h.app.force_close_after_release_failure(&Context::default());
    assert!(h.saved().is_empty(), "the text of the closed session is not written back");
}

#[test]
fn a_forced_close_drops_the_board_draft_and_keeps_the_figure_crop_and_pdf_drafts() {
    let mut h = Harness::new();
    figure_tab(&mut h, "그림 문장");
    h.app.board_memo_editor = Some(new_memo_editor("버릴 메모"));
    h.app.image_crop_editor = Some(crop_editor("crop-1"));
    h.app.held_pdf_evidence.push(held_pdf());
    h.settle();
    assert_eq!(h.saved().len(), 4, "the figure, the memo, the crop and the PDF records are in the file");
    h.app.force_close_after_release_failure(&Context::default());
    let left = h.saved();
    assert!(!left.iter().any(|draft| matches!(draft.body, Body::Note { .. })), "the board draft is dropped");
    assert!(left.iter().any(|draft| matches!(draft.body, Body::Figure { .. })), "the figure's text stays as it was");
    assert!(left.iter().any(|draft| matches!(draft.body, Body::Crop { .. })), "the crop stays as it was");
    assert!(left.iter().any(|draft| matches!(draft.body, Body::PdfEvidence { .. })), "the PDF records stay as they were held");
    assert_eq!(left.len(), 3);
}

#[test]
fn an_unanswered_request_stays_marked_when_its_retry_cannot_be_written_and_after_a_restart() {
    let mut h = Harness::new();
    figure_tab(&mut h, "축 정리");
    let ticket = h.app.next_ticket;
    h.app.start_ai_job();
    sent_start_ids(&mut h);
    h.answer(ticket, Err(DltError::new(ErrorCode::Unavailable, "no answer")));
    assert!(h.saved()[0].sent.is_some(), "the unanswered request is marked in the file");

    // The retry cannot write its mark, so it does not go out. Once the folder is writable again, the next quiet write saves the tab.
    block_draft_folder(&h.root, true);
    h.app.start_ai_job();
    assert!(sent_start_ids(&mut h).is_empty(), "a retry whose mark cannot be written does not go out");
    block_draft_folder(&h.root, false);
    h.settle();

    // After a restart the request is still marked: it is not offered as a plain draft to restore.
    let restarted = h.saved();
    assert_eq!(restarted.len(), 1);
    assert!(restarted[0].sent.is_some(), "the unanswered request is still marked after a restart");
    assert!(!h.app.draft_standing(&restarted[0]).ready, "and it is not restored");
}

/// The retry of an unanswered request goes out with the same id and is refused: the host made no job for that id (it would have
/// answered from its receipt), so the draft is no longer marked by it, and the next request is a new one. A refusal of a request
/// with another id keeps the earlier mark (`the_same_draft_with_other_settings_after_an_unknown_start_is_a_new_request`).
#[test]
fn a_refused_retry_with_the_same_id_takes_back_the_mark_after_a_restart() {
    let mut h = Harness::new();
    figure_tab(&mut h, "축 정리");
    let ticket = h.app.next_ticket;
    h.app.start_ai_job();
    let first = sent_start_ids(&mut h);
    h.answer(ticket, Err(DltError::new(ErrorCode::Unavailable, "no answer")));
    let retry = h.app.next_ticket;
    h.app.start_ai_job();
    assert_eq!(sent_start_ids(&mut h), first, "the retry goes out with the same id");
    h.answer(retry, Err(DltError::new(ErrorCode::InvalidArgument, "refused")));

    let restarted = h.saved();
    assert_eq!(restarted.len(), 1);
    assert!(restarted[0].sent.is_none(), "the refused id no longer marks the draft");
    h.app.start_ai_job();
    let next = sent_start_ids(&mut h);
    assert_eq!(next.len(), 1);
    assert_ne!(next, first, "the next request is a new one, as the draft shows");
}

#[test]
fn a_refused_same_id_retry_restores_the_other_unknown_mark_for_the_same_draft() {
    let mut h = Harness::new();
    figure_tab(&mut h, "축 정리");
    h.app.allow_data = true;
    let first_ticket = request_from_tab(&mut h, "축 정리");
    let first = sent_start_ids(&mut h);
    h.answer(first_ticket, Err(DltError::new(ErrorCode::Io, "no answer")));

    h.app.allow_data = false;
    let second_ticket = request_from_tab(&mut h, "축 정리");
    let second = sent_start_ids(&mut h);
    assert_ne!(first, second, "the changed settings start a different request");
    h.answer(second_ticket, Err(DltError::new(ErrorCode::Io, "no answer")));
    assert_eq!(h.saved()[0].sent.as_ref(), Some(&second[0]));

    let retry = request_from_tab(&mut h, "축 정리");
    assert_eq!(sent_start_ids(&mut h), second);
    h.answer(retry, Err(DltError::invalid("refused")));
    assert_eq!(h.saved()[0].sent.as_ref(), Some(&first[0]), "only the refused id loses its mark");

    h.app.allow_data = true;
    request_from_tab(&mut h, "축 정리");
    assert_eq!(sent_start_ids(&mut h), first, "the restored mark matches the request that can still be retried");
}

#[test]
fn a_refusal_takes_back_its_own_mark_and_keeps_an_earlier_unanswered_one() {
    let root = workspace();
    let path = drafts_path(&root);
    let now = Instant::now();
    let mut book = DraftBook::open(path.clone());
    let live = figure_live("축 정리", &[], 3);
    book.begin_send(1, "figure:figure-1", live.clone(), "request-1", true, now).expect("written");
    let _ = book.answered(1, Outcome::Unknown, None, now + Duration::from_secs(1));
    book.begin_send(2, "figure:figure-1", live, "request-2", true, now + Duration::from_secs(2)).expect("written");
    let _ = book.answered(2, Outcome::Refused, None, now + Duration::from_secs(3));
    let saved = DraftBook::open(path).recovered();
    assert_eq!(saved.len(), 1);
    assert_eq!(saved[0].sent.as_deref(), Some("request-1"), "the earlier unanswered request keeps its mark");
    let _ = std::fs::remove_dir_all(&root);
}

/// The answer of a figure request decides its draft's mark and the record of its start (`unknown_starts`) by one judgement. An
/// answer that leaves the outcome unknown keeps both, and the next press of the request button repeats the id; a refusal that
/// says nothing was done ends both, even when it may be retried.
#[test]
fn a_figure_request_and_its_draft_take_the_same_unknown_judgement() {
    let answer = |code: ErrorCode, retryable: bool| {
        let mut error = DltError::new(code, "the answer");
        error.retryable = retryable;
        error
    };
    let cases = [
        (answer(ErrorCode::Io, false), true),
        (answer(ErrorCode::Io, true), true),
        (answer(ErrorCode::Internal, false), true),
        (answer(ErrorCode::StorageFailure, false), true),
        (answer(ErrorCode::StorageFailure, true), true),
        (answer(ErrorCode::HostNotRunning, true), true),
        (answer(ErrorCode::Unavailable, false), true),
        (answer(ErrorCode::TargetBusy, true), false),
        (answer(ErrorCode::RevisionConflict, true), false),
        (answer(ErrorCode::InvalidArgument, false), false),
    ];
    for (error, unknown) in cases {
        let label = format!("{:?} retryable={}", error.code, error.retryable);
        let mut h = Harness::new();
        figure_tab(&mut h, "축 정리");
        let ticket = request_from_tab(&mut h, "축 정리");
        let first = sent_start_ids(&mut h);
        assert_eq!(first.len(), 1, "{label}: the start is sent");
        h.answer(ticket, Err(error));
        let marked = h.saved().iter().any(|draft| draft.sent.as_deref() == Some(first[0].as_str()));
        assert_eq!(marked, unknown, "{label}: the draft stays marked as maybe sent only when the outcome is unknown");
        assert_eq!(h.app.unknown_starts.contains_key("figure-1"), unknown, "{label}: the start is kept for a repeat just as its draft");
        request_from_tab(&mut h, "축 정리");
        let retry = sent_start_ids(&mut h);
        assert_eq!(retry.len(), 1, "{label}: the next try goes out");
        assert_eq!(retry == first, unknown, "{label}: the next try repeats the id only when the outcome is unknown");
        let _ = std::fs::remove_dir_all(&h.root);
    }
}

/// Sends `prompt` from the AI tab as its request button does (`ai.request`); returns the ticket the start goes out with.
fn request_from_tab(h: &mut Harness, prompt: &str) -> u64 {
    let ticket = h.app.next_ticket;
    h.app.invoke("ai.request", json!({ "prompt": prompt })).expect("the request is sent");
    ticket
}

/// A start whose outcome is unknown keeps its record while a start of the same figure with other content is refused or
/// accepted: the repeat of the first request still carries its id, so the host makes no second job for it.
#[test]
fn an_unknown_start_keeps_its_record_through_the_answer_of_a_start_with_other_content() {
    for later_accepted in [false, true] {
        let label = format!("later accepted={later_accepted}");
        let mut h = Harness::new();
        figure_tab(&mut h, "축 정리");
        let first_ticket = request_from_tab(&mut h, "축 정리");
        let first = sent_start_ids(&mut h);
        h.answer(first_ticket, Err(DltError::new(ErrorCode::Io, "no answer")));
        assert!(h.app.unknown_starts.contains_key("figure-1"), "{label}: the first start is kept for a repeat");

        let later_ticket = request_from_tab(&mut h, "범례 정리");
        let later = sent_start_ids(&mut h);
        assert_eq!(later.len(), 1, "{label}: the later start goes out");
        assert_ne!(later, first, "{label}: other content is another request");
        assert!(h.app.unknown_starts.contains_key("figure-1"), "{label}: sending another request leaves the record");
        let answer = if later_accepted { Ok(json!({ "id": "job-2", "attempt_id": "attempt-2", "artifact_id": "figure-1", "status": "succeeded" })) } else { Err(DltError::invalid("refused")) };
        h.answer(later_ticket, answer);
        h.app.job = None;
        h.app.parked_jobs.clear();
        assert!(h.app.unknown_starts.contains_key("figure-1"), "{label}: the answer of another request leaves the record");

        request_from_tab(&mut h, "축 정리");
        assert_eq!(sent_start_ids(&mut h), first, "{label}: the repeat of the first request carries its id");
        let _ = std::fs::remove_dir_all(&h.root);
    }
}

/// Two requests of one figure whose outcomes are both unknown: each repeat carries its own id, and the answer that accepts the
/// repeat of the first ends only the first's record.
#[test]
fn unknown_starts_of_one_figure_each_keep_their_own_id() {
    let mut h = Harness::new();
    figure_tab(&mut h, "축 정리");
    let a_ticket = request_from_tab(&mut h, "축 정리");
    let a = sent_start_ids(&mut h);
    h.answer(a_ticket, Err(DltError::new(ErrorCode::Io, "no answer")));
    let b_ticket = request_from_tab(&mut h, "범례 정리");
    let b = sent_start_ids(&mut h);
    assert_eq!(b.len(), 1, "the second request goes out");
    assert_ne!(a, b, "other content is another request");
    h.answer(b_ticket, Err(DltError::new(ErrorCode::Io, "no answer")));

    let a_repeat = request_from_tab(&mut h, "축 정리");
    assert_eq!(sent_start_ids(&mut h), a, "the repeat of the first request carries its id");
    h.answer(a_repeat, Ok(json!({ "id": "job-1", "attempt_id": "attempt-1", "artifact_id": "figure-1", "status": "succeeded" })));
    h.app.job = None;
    h.app.parked_jobs.clear();

    let b_repeat = request_from_tab(&mut h, "범례 정리");
    assert_eq!(sent_start_ids(&mut h), b, "the acceptance of the first leaves the second's record: its repeat carries its id");
    h.answer(b_repeat, Err(DltError::new(ErrorCode::Io, "no answer")));
    request_from_tab(&mut h, "축 정리");
    let a_again = sent_start_ids(&mut h);
    assert_eq!(a_again.len(), 1);
    assert_ne!(a_again, a, "an accepted request has no record left: asking again is a new request");
    let _ = std::fs::remove_dir_all(&h.root);
}

/// The `job.start` params the window sent since the last call of this helper.
fn sent_starts(h: &mut Harness) -> Vec<Value> {
    h.work
        .try_iter()
        .filter_map(|work| match work {
            Work::Call { method, params, .. } if method == "job.start" => Some(params),
            _ => None,
        })
        .collect()
}

/// A request whose outcome is unknown, then an edit of the document, then the same draft asked again: the recorded request
/// goes out again as it was (its id and its base revision), so the host answers it from its receipt and makes no second job.
/// The user is told that the edit is not in it.
#[test]
fn the_same_draft_asked_again_after_an_edit_resends_the_unknown_request_as_it_was() {
    let mut h = Harness::new();
    figure_tab(&mut h, "축 정리");
    let ticket = request_from_tab(&mut h, "축 정리");
    let first = sent_starts(&mut h);
    assert_eq!(first.len(), 1, "the first request goes out");
    h.answer(ticket, Err(DltError::new(ErrorCode::Io, "no answer")));

    h.app.active = Some(summary("figure-1", ArtifactKind::Figure, 4));
    request_from_tab(&mut h, "축 정리");
    assert_eq!(sent_starts(&mut h), first, "the same draft resends the unknown request with its id and its content");
    let notice = h.app.notices.current(Instant::now()).map(|notice| notice.text.clone());
    assert_eq!(notice.as_deref(), Some(REPEAT_OF_UNKNOWN_START), "the user is told the edit is not in the request");
    let _ = std::fs::remove_dir_all(&h.root);
}

/// A request whose outcome is unknown, then a change of the request's settings (data changes no longer allowed), then the same
/// draft asked again: the old request is not sent again with its old permissions; the new settings go out as a new request, and
/// the user is told that the old one may still run. The old record stays: with the old settings back, it goes out again as it was.
#[test]
fn the_same_draft_with_other_settings_after_an_unknown_start_is_a_new_request() {
    let mut h = Harness::new();
    figure_tab(&mut h, "축 정리");
    h.app.allow_data = true;
    let ticket = request_from_tab(&mut h, "축 정리");
    let first = sent_starts(&mut h);
    assert_eq!(first.len(), 1);
    h.answer(ticket, Err(DltError::new(ErrorCode::Io, "no answer")));

    h.app.allow_data = false;
    let second_ticket = request_from_tab(&mut h, "축 정리");
    let second = sent_starts(&mut h);
    assert_eq!(second.len(), 1, "the request goes out");
    assert_ne!(second[0]["request_id"], first[0]["request_id"], "with a new id");
    assert_ne!(second[0]["allowed_intents"], first[0]["allowed_intents"], "and with the new settings");
    let notice = h.app.notices.current(Instant::now()).map(|notice| notice.text.clone());
    assert_eq!(notice.as_deref(), Some(UNKNOWN_START_MAY_RUN), "the user is told the old request may still run");
    assert!(h.app.unknown_starts.contains_key("figure-1"), "the old request keeps its record");

    h.answer(second_ticket, Err(DltError::invalid("refused")));
    assert!(h.saved().iter().any(|draft| draft.sent.as_deref() == first[0]["request_id"].as_str()), "the old mark comes back");
    h.app.allow_data = true;
    request_from_tab(&mut h, "축 정리");
    assert_eq!(sent_starts(&mut h), first, "with the old settings back, the old request goes out again as it was");
    let _ = std::fs::remove_dir_all(&h.root);
}

/// The repeat of one unknown request is refused: the record of the other unknown request of the figure stays, and its repeat
/// carries its id.
#[test]
fn a_refused_repeat_leaves_the_record_of_another_unknown_request() {
    let mut h = Harness::new();
    figure_tab(&mut h, "축 정리");
    let a_ticket = request_from_tab(&mut h, "축 정리");
    let a = sent_start_ids(&mut h);
    h.answer(a_ticket, Err(DltError::new(ErrorCode::Io, "no answer")));
    let b_ticket = request_from_tab(&mut h, "범례 정리");
    let b = sent_start_ids(&mut h);
    h.answer(b_ticket, Err(DltError::new(ErrorCode::Io, "no answer")));

    let a_repeat = request_from_tab(&mut h, "축 정리");
    assert_eq!(sent_start_ids(&mut h), a);
    h.answer(a_repeat, Err(DltError::invalid("refused")));
    assert!(h.app.unknown_starts.contains_key("figure-1"), "the other record stays");
    request_from_tab(&mut h, "범례 정리");
    assert_eq!(sent_start_ids(&mut h), b, "the repeat of the other request carries its id");
    let _ = std::fs::remove_dir_all(&h.root);
}

/// The records of unknown starts are bounded: past `UNKNOWN_STARTS_MAX` the oldest is dropped and its repeat goes out with a new
/// id, while the others keep theirs. A repeat whose answer is unknown again replaces its own record instead of adding one.
#[test]
fn unknown_starts_past_the_limit_drop_the_oldest_record() {
    let mut h = Harness::new();
    figure_tab(&mut h, "요청 0");
    let mut ids = Vec::new();
    for n in 0..=UNKNOWN_STARTS_MAX {
        let ticket = request_from_tab(&mut h, &format!("요청 {n}"));
        let sent = sent_start_ids(&mut h);
        assert_eq!(sent.len(), 1, "request {n} goes out");
        h.answer(ticket, Err(DltError::new(ErrorCode::Io, "no answer")));
        ids.push(sent);
    }
    assert_eq!(h.app.unknown_starts.len(), UNKNOWN_STARTS_MAX, "the records stop at the limit");
    // The answer that dropped the record is an error itself: both are shown in the same frame, the error first.
    let (severity, notice, origin) = h
        .app
        .notices
        .current(Instant::now())
        .map(|notice| (notice.severity, notice.text.clone(), notice.origin().map(str::to_owned)))
        .expect("a notice");
    assert_eq!(severity, crate::notice::Severity::Error, "the error of the answer stays an error");
    assert_eq!(origin.as_deref(), Some("job.start"), "the error keeps its origin, so a later success of its method takes it away");
    assert!(notice.starts_with("파일에 접근하지 못했습니다"), "the error of the answer is still shown: {notice}");
    assert!(notice.contains("\"요청 0\"") && notice.contains("figure-1"), "the user is told which request is no longer followed: {notice}");

    let repeat = request_from_tab(&mut h, "요청 1");
    assert_eq!(sent_start_ids(&mut h), ids[1], "a request within the limit keeps its id");
    h.answer(repeat, Err(DltError::new(ErrorCode::Io, "no answer")));
    assert_eq!(h.app.unknown_starts.len(), UNKNOWN_STARTS_MAX, "an unknown repeat replaces its own record");

    request_from_tab(&mut h, "요청 0");
    let oldest = sent_start_ids(&mut h);
    assert_eq!(oldest.len(), 1);
    assert_ne!(oldest, ids[0], "the oldest record was dropped: its repeat is a new request");
    let _ = std::fs::remove_dir_all(&h.root);
}

/// The limit counts the records of every document: a start of another document past it drops the oldest record of the first.
#[test]
fn the_limit_of_unknown_starts_counts_every_document() {
    let mut h = Harness::new();
    let start = |artifact: &str, id: &str, prompt: &str| json!({ "artifact_id": artifact, "request_id": id, "prompt": prompt });
    for n in 0..UNKNOWN_STARTS_MAX {
        let sent = SentStart::of(&start("figure-1", &format!("request-{n}"), &n.to_string())).expect("a start");
        h.app.settle_job_start(sent, &Err(DltError::new(ErrorCode::Io, "no answer")));
    }
    let other = SentStart::of(&start("figure-2", "request-other", "0")).expect("a start");
    h.app.settle_job_start(other, &Err(DltError::new(ErrorCode::Io, "no answer")));
    assert_eq!(h.app.unknown_starts.len(), UNKNOWN_STARTS_MAX);
    assert!(h.app.unknown_starts.contains_key("figure-2"));
    let request_id = |h: &Harness, artifact: &str, prompt: &str| h.app.with_job_request_id(start(artifact, "new", prompt), None).0["request_id"].clone();
    assert_eq!(request_id(&h, "figure-1", "0"), json!("new"), "the oldest record of the first document was dropped");
    assert_eq!(request_id(&h, "figure-1", "1"), json!("request-1"));
    assert_eq!(request_id(&h, "figure-2", "0"), json!("request-other"), "the same content of another document keeps its own id");
    let _ = std::fs::remove_dir_all(&h.root);
}

/// Past the limit a record of a document no longer in the workspace list (deleted; the window has no tabs) goes first, even when a record of a listed document is older; a
/// record is not dropped when its document is left or goes, since its answer can still come. With no such record the oldest goes. Each
/// drop names its request to the user.
#[test]
fn past_the_limit_a_record_of_a_document_gone_from_the_workspace_list_goes_first_and_the_user_is_told() {
    let mut h = Harness::new();
    h.app.artifacts = vec![summary("figure-1", ArtifactKind::Figure, 3)];
    let record = |h: &mut Harness, artifact: &str, id: &str, prompt: &str| {
        let sent = SentStart::of(&json!({ "artifact_id": artifact, "request_id": id, "prompt": prompt })).expect("a start");
        h.app.settle_job_start(sent, &Err(DltError::new(ErrorCode::Io, "no answer")));
        // As at the end of `process_replies`.
        h.app.show_dropped_unknown_starts();
    };
    let request_id = |h: &Harness, prompt: &str| {
        h.app.with_job_request_id(json!({ "artifact_id": "figure-1", "request_id": "new", "prompt": prompt }), None).0["request_id"].clone()
    };
    let notice = |h: &Harness| h.app.notices.current(Instant::now()).map(|notice| notice.text.clone()).unwrap_or_default();
    record(&mut h, "figure-1", "request-0", "0");
    record(&mut h, "gone-1", "request-gone", "목록에서 빠진 문서의 요청");
    for n in 1..UNKNOWN_STARTS_MAX - 1 {
        record(&mut h, "figure-1", &format!("request-{n}"), &n.to_string());
    }
    assert_eq!(h.app.unknown_starts.len(), UNKNOWN_STARTS_MAX);
    assert!(h.app.unknown_starts.contains_key("gone-1"), "the record of a document gone from the list is kept within the limit");
    assert!(notice(&h).is_empty(), "nothing is dropped within the limit");

    record(&mut h, "figure-1", "request-last", "last");
    assert_eq!(h.app.unknown_starts.len(), UNKNOWN_STARTS_MAX);
    assert!(!h.app.unknown_starts.contains_key("gone-1"), "the record of the document gone from the list goes first");
    assert_eq!(request_id(&h, "0"), json!("request-0"), "the older record of the listed document stays");
    assert!(notice(&h).contains("\"목록에서 빠진 문서의 요청\"") && notice(&h).contains("gone-1"), "the user is told: {}", notice(&h));

    record(&mut h, "figure-1", "request-more", "more");
    assert_eq!(request_id(&h, "0"), json!("new"), "with no record of a document gone from the list the oldest goes");
    assert!(notice(&h).contains("\"0\""), "the user is told: {}", notice(&h));
    let _ = std::fs::remove_dir_all(&h.root);
}

/// Three requests of the same draft on their way at once, refused one by one in every order: each refusal takes the chain of
/// marks past the refused request, so no refused id is left as the draft's mark.
#[test]
fn refusals_of_three_overlapping_requests_in_any_order_leave_no_mark() {
    let orders = [[1, 2, 3], [1, 3, 2], [2, 1, 3], [2, 3, 1], [3, 1, 2], [3, 2, 1]];
    for order in orders {
        let root = workspace();
        let path = drafts_path(&root);
        let now = Instant::now();
        let mut book = DraftBook::open(path.clone());
        let live = figure_live("축 정리", &[], 3);
        book.sync("figure:figure-1", Edit::Has(live.clone()), now);
        for ticket in 1..=3u64 {
            book.begin_send(ticket, "figure:figure-1", live.clone(), &format!("request-{ticket}"), true, now).expect("written");
        }
        for ticket in order {
            let _ = book.answered(ticket, Outcome::Refused, None, now);
        }
        let saved = DraftBook::open(path).recovered();
        assert_eq!(saved.len(), 1, "{order:?}: the tab's draft stays");
        assert_eq!(saved[0].sent, None, "{order:?}: no refused request is left as the mark");
        let _ = std::fs::remove_dir_all(&root);
    }
}

/// The answer of a save is judged as every other request (`change_result_unknown`): an answer that can come after the host
/// received the save leaves it unknown, so the later `artifact.saved` event still says it was saved; a refusal ends it.
#[test]
fn a_save_answer_is_judged_unknown_by_the_code_and_not_by_retryable() {
    use super::state::SaveRpcOutcome;
    use super::test_support::Harness as Window;

    let answer = |code: ErrorCode, retryable: bool| {
        let mut error = DltError::new(code, "the answer");
        error.retryable = retryable;
        error
    };
    for (error, unknown) in [(answer(ErrorCode::Io, false), true), (answer(ErrorCode::Internal, false), true), (answer(ErrorCode::TargetBusy, true), false), (answer(ErrorCode::RevisionConflict, true), false)] {
        let label = format!("{:?} retryable={}", error.code, error.retryable);
        let mut window = Window::new();
        window.open_document("docx", 1);
        let target = workspace().join("saved.docx");
        window.app.invoke("file.save", json!({ "target_path": target.to_string_lossy() })).expect("the save is sent");
        let sent = window.sent();
        let request_id = sent.iter().find(|request| request.1 == "artifact.save").map(|request| request.2["request_id"].as_str().unwrap_or_default().to_owned()).expect("the save is sent");
        window.answer(&sent, "artifact.save", Err(error));
        let outcome = window.app.pending_save_notices.get(&request_id).map(|pending| pending.outcome);
        assert_eq!(outcome == Some(SaveRpcOutcome::Unknown), unknown, "{label}: the save is kept as maybe saved only when the outcome is unknown");
        assert_eq!(outcome.is_none(), !unknown, "{label}: a refused save is ended");
    }
}

/// Two values sent from the open cell editor with the same content (`data.edit_cell` while the editor stays open): the later
/// request finds the earlier one's mark. In every order of the answers, and whether each is refused or accepted, a refused
/// request is not left marked as maybe sent, in the window or in the file a restart reads.
#[test]
fn a_refused_request_is_not_left_marked_when_a_later_request_of_the_same_value_overlaps_it() {
    use super::test_support::{Host, Window};
    use designllm_contracts::{Intent, Operation};

    for earlier_answered_first in [true, false] {
        for (earlier_accepted, later_accepted) in [(false, false), (false, true), (true, false), (true, true)] {
            let label = format!("earlier answered first={earlier_answered_first}, earlier accepted={earlier_accepted}, later accepted={later_accepted}");
            let mut host = Host::start();
            let figure = host.create_figure();
            let table = host.import_table("value\n1\n2\n");
            host.apply(&figure, vec![Operation::PutDataset { dataset: table }], [Intent::Data].into_iter().collect()).expect("the table");
            let mut window = Window::open(&mut host, &figure);
            let root = workspace();
            std::fs::create_dir_all(&root).expect("the workspace folder");
            window.app.drafts = DraftBook::open(drafts_path(&root));
            // From here the test answers the window's requests itself, in the order of the case.
            let (rpc, replies, work) = RpcWorker::detached();
            window.app.rpc = rpc;

            let document = window.app.figure_document().expect("the figure is shown");
            let dataset = &document.datasets[0];
            let (row, column) = (&dataset.rows[0], &dataset.columns[0]);
            let original = row.cells.get(&column.id).map(|cell| cell.original.clone()).unwrap_or_default();
            let edit = DatasetCellEdit {
                dataset_id: dataset.id.clone(),
                version_hash: dataset.version_hash.clone(),
                row_id: row.id.clone(),
                column_id: column.id.clone(),
                original,
                draft: "13".into(),
            };
            let args = json!({ "dataset_id": &edit.dataset_id, "row_id": &edit.row_id, "column_id": &edit.column_id, "value": "13" });
            window.app.dataset_edit = Some(edit);
            window.app.sync_drafts();
            window.app.invoke("data.edit_cell", args.clone()).expect("the earlier value is sent");
            window.app.sync_drafts();
            window.app.invoke("data.edit_cell", args).expect("the later value is sent");
            let tickets: Vec<u64> = work
                .try_iter()
                .filter_map(|work| match work {
                    Work::Call { ticket, method, .. } if method == "figure.dataset.edit" => Some(ticket),
                    _ => None,
                })
                .collect();
            assert_eq!(tickets.len(), 2, "{label}: both values are sent");
            let marked: Vec<Draft> = DraftBook::open(drafts_path(&root)).recovered();
            assert_eq!(marked.len(), 1, "{label}: one draft holds the value");
            assert!(marked[0].sent.is_some(), "{label}: the draft is marked while both requests are on their way");

            let result = |accepted: bool| if accepted { Ok(RpcPayload::Json(json!({ "revision": 99 }))) } else { Err(DltError::invalid("refused")) };
            let mut answers = vec![(tickets[0], result(earlier_accepted)), (tickets[1], result(later_accepted))];
            if !earlier_answered_first {
                answers.reverse();
            }
            let ctx = Context::default();
            for (ticket, result) in answers {
                replies.send(RpcReply { ticket, result }).expect("the window still listens");
                window.app.process_replies(&ctx);
            }
            window.app.sync_drafts();
            window.app.drafts.flush();
            let saved = DraftBook::open(drafts_path(&root)).recovered();
            assert!(saved.iter().all(|draft| draft.sent.is_none()), "{label}: no answered request is left marked as maybe sent: {saved:?}");
            let _ = std::fs::remove_dir_all(&root);
        }
    }
}

/// A direct edit the window holds (the record `hold_direct_edit` keeps): the close dialog counts it and drops it when closed.
fn push_held_edit(h: &mut Harness) {
    h.app.held_edits.push(HeldEdit {
        id: "held-edit-1".into(),
        apply: ApplyContext {
            request_id: "request-1".into(),
            workspace_id: String::new(),
            artifact_id: String::new(),
            base_revision: 0,
            operations: Vec::new(),
            allowed_intents: std::collections::BTreeSet::new(),
            actor: "desktop".into(),
            user_edit: true,
            retryable: false,
            held_edit_id: None,
            after_action: PendingAction::Apply,
        },
        nodes: vec!["text-1".into()],
        state: HeldEditState::WaitingForJobRelease,
        message: "보류 변경".into(),
        can_retry: false,
        retry_same_request: false,
        reservations_checked: false,
        checked_revision: None,
    });
}

#[test]
fn the_close_button_and_note_name_the_held_edits_dropped_past_unsaved_drafts() {
    let mut h = Harness::new();
    push_held_edit(&mut h);
    block_draft_folder(&h.root, true);
    h.app.drafts.sync("home", Edit::Has(home_live("닫기 전 요청")), Instant::now());
    assert!(!h.app.drafts.flush(), "the draft cannot be written");
    assert!(h.app.drafts.save_failing(), "the dialog sees the unsaved drafts");
    assert_eq!(h.app.close_discard_label(true), "보류 변경을 버리고 닫기");
    assert_eq!(h.app.held_drop_note(true), Some("보류 중인 변경도 버려집니다."));
    block_draft_folder(&h.root, false);
}

#[test]
fn without_held_edits_the_unsaved_drafts_button_and_note_keep_their_wording() {
    let h = Harness::new();
    assert_eq!(h.app.close_discard_label(true), "그래도 닫기");
    assert_eq!(h.app.held_drop_note(true), None);
    assert_eq!(h.app.close_discard_label(false), "보류 변경 버리고 닫기");
    assert_eq!(h.app.held_drop_note(false), None);
}

#[test]
fn keeping_work_after_the_close_question_saves_the_board_memo_draft_again() {
    let mut h = Harness::new();
    h.app.board_memo_editor = Some(new_memo_editor("처음 메모"));
    h.settle();
    assert_eq!(h.saved().len(), 1, "the memo is in the file");
    h.app.close_confirmation_open = true;
    let ctx = h.ctx.clone();
    h.app.keep_working_after_close_request(&ctx);
    assert!(!h.app.close_confirmation_open, "the dialog is closed and the window stays open");
    h.app.board_memo_editor.as_mut().expect("the editor stays open").memo.text = "이어 쓴 메모".into();
    h.settle();
    let saved = h.saved();
    assert!(
        saved.iter().any(|draft| matches!(&draft.body, Body::Note { memo, .. } if memo.text == "이어 쓴 메모")),
        "the memo typed after 'keep working' is saved on the next quiet write"
    );
}

/// Plays the RPC worker for the `job.start` requests the window queued: the host takes each one, and its answer is returned here,
/// never given to the window (the window ends before it comes). Returns the params and the answer of each start.
fn host_takes_starts(h: &mut Harness, host: &mut Host) -> Vec<(Value, Result<Value, DltError>)> {
    let starts: Vec<Value> = h
        .work
        .try_iter()
        .filter_map(|work| match work {
            Work::Call { method, params, .. } if method == "job.start" => Some(params),
            _ => None,
        })
        .collect();
    starts.into_iter().map(|params| { let answer = host.call("job.start", params.clone()); (params, answer) }).collect()
}

/// The recovered draft of the document `artifact_id`.
fn recovered_of(h: &Harness, artifact_id: &str) -> Option<Draft> {
    h.app.drafts.recovered().into_iter().find(|draft| draft.artifact_id.as_deref() == Some(artifact_id))
}

/// R12 (48b, SPEC.md:22 "비정상 종료 복구"): the request typed in the AI tab of a document that is not a Figure, and the id of the
/// request sent from it, come back after the window ends without writing anything more (a crash). The text is restored into the
/// tab, and sending it again repeats the id with the same content, so the host answers from its receipt and makes no second job.
#[test]
fn a_board_request_and_its_request_id_come_back_after_a_crash_and_the_repeat_makes_no_second_job() {
    const PROMPT: &str = "제목을 정리해 주세요";
    let mut host = Host::start();
    let canvas = host.create("canvas", "보드");
    let revision = host.artifact(&canvas).revision;
    let board = |h: &mut Harness| {
        h.app.screen = Screen::Editor;
        h.app.active = Some(summary(&canvas, ArtifactKind::Canvas, revision));
        h.app.sync_drafts();
    };
    let mut h = Harness::new();
    board(&mut h);

    // Typed and not sent: the text comes back.
    h.app.ai_prompt = PROMPT.into();
    h.settle();
    h.crash_and_restart();
    board(&mut h);
    let typed = recovered_of(&h, &canvas).expect("the typed request is recovered");
    assert!(typed.sent.is_none());
    assert!(h.app.draft_standing(&typed).ready, "the empty tab of the same document at the same revision takes it");
    h.app.restore_draft(&typed.key);
    assert_eq!(h.app.ai_prompt, PROMPT, "the request is back in the tab");

    // Sent, and the window ends before the answer: the host has started the job.
    request_from_tab(&mut h, PROMPT);
    let first = host_takes_starts(&mut h, &mut host);
    assert_eq!(first.len(), 1, "the start went out");
    let first_job = first[0].1.clone().expect("the host starts the job");
    h.crash_and_restart();
    board(&mut h);
    let sent = recovered_of(&h, &canvas).expect("the sent request is recovered");
    assert_eq!(sent.sent.as_deref(), first[0].0["request_id"].as_str(), "its request id comes back with it");
    assert!(h.app.draft_standing(&sent).ready, "a request whose repeat carries its id can be restored");
    h.app.restore_draft(&sent.key);
    assert_eq!(h.app.ai_prompt, PROMPT, "the request is back in the tab");

    request_from_tab(&mut h, PROMPT);
    let repeat = host_takes_starts(&mut h, &mut host);
    assert_eq!(repeat.len(), 1, "the repeat went out");
    assert_eq!(repeat[0].0, first[0].0, "the start goes out again as it was, with its request id");
    let repeated_job = repeat[0].1.clone().expect("the host answers from its receipt");
    assert_eq!(repeated_job["id"], first_job["id"], "the same job");
    let jobs = host.workspace().active_jobs().expect("the host's jobs");
    assert_eq!(jobs.len(), 1, "no second job");
}

/// Shows the canvas `canvas-1` at `revision` in the editor of `app`, and brings its drafts up to its editors.
fn board_of(app: &mut DesktopApp, revision: u64) {
    app.screen = Screen::Editor;
    app.active = Some(summary("canvas-1", ArtifactKind::Canvas, revision));
    app.sync_drafts();
}

/// Sends `prompt` from the AI tab of `canvas-1` and returns the request id the start went out with; nothing answers it.
fn send_board_request(h: &mut Harness, prompt: &str) -> String {
    request_from_tab(h, prompt);
    let ids = sent_start_ids(h);
    assert_eq!(ids.len(), 1, "the start went out");
    ids.into_iter().next().unwrap_or_default()
}

/// 48b review (MAJOR 1): a request another window sent and still waits for is that window's. A window opened on the same folder
/// lists it, but neither restores it nor repeats its id.
#[test]
fn a_request_another_live_window_sent_is_not_restored_or_repeated_by_a_second_window() {
    let mut h = Harness::new();
    board_of(&mut h.app, 1);
    let first = send_board_request(&mut h, "제목 정리");

    let (mut second, _replies, work) = window_on(&h.root);
    board_of(&mut second, 1);
    let theirs = second.drafts.recovered().into_iter().find(|draft| draft.sent.as_deref() == Some(first.as_str())).expect("the other window's request is listed");
    assert!(!second.draft_standing(&theirs).ready, "the request of a window that still runs is not restored here");
    second.invoke("ai.request", json!({ "prompt": "제목 정리" })).expect("the request is sent");
    let ids: Vec<String> = work
        .try_iter()
        .filter_map(|work| match work {
            Work::Call { method, params, .. } if method == "job.start" => params["request_id"].as_str().map(str::to_owned),
            _ => None,
        })
        .collect();
    assert_eq!(ids.len(), 1);
    assert_ne!(ids[0], first, "the second window does not send the first window's request as its own");
}

/// 48b review (MAJOR 1): a request another window sent and then saw accepted leaves the file; a window opened before that
/// does not write it back with its own drafts.
#[test]
fn a_request_another_window_saw_accepted_is_not_written_back_by_a_second_window() {
    let mut h = Harness::new();
    board_of(&mut h.app, 1);
    let ticket = h.app.next_ticket;
    let first = send_board_request(&mut h, "제목 정리");
    let (mut second, _replies, _work) = window_on(&h.root);
    second.sync_drafts();

    h.answer(ticket, Ok(json!({ "id": "job-1", "attempt_id": "attempt-1", "artifact_id": "canvas-1", "status": "queued" })));
    assert!(!h.saved().iter().any(|draft| draft.sent.as_deref() == Some(first.as_str())), "the accepted request left the file");
    second.home.prompt = "다른 요청".into();
    second.sync_drafts();
    assert!(second.drafts.flush(), "the second window writes its own draft");
    let saved = h.saved();
    assert!(saved.iter().any(|draft| matches!(&draft.body, Body::Home { prompt, .. } if prompt == "다른 요청")));
    assert!(!saved.iter().any(|draft| draft.sent.as_deref() == Some(first.as_str())), "the accepted request does not come back");
    second.drafts = DraftBook::default();
}

/// 48b review (MAJOR 2): a request restored after a crash and left unsent goes back to the recovered drafts when the window
/// leaves its document, rather than out of sight until the next start.
#[test]
fn a_restored_request_left_unsent_is_listed_again_when_its_document_is_left() {
    let mut h = Harness::new();
    board_of(&mut h.app, 1);
    let first = send_board_request(&mut h, "제목 정리");
    h.crash_and_restart();
    board_of(&mut h.app, 1);
    let draft = recovered_of(&h, "canvas-1").expect("the request is recovered");
    h.app.restore_draft(&draft.key);
    assert_eq!(h.app.ai_prompt, "제목 정리", "the request is in the tab");
    h.app.leave_drafts_for_switch();
    let listed = recovered_of(&h, "canvas-1").expect("the request is listed again");
    assert_eq!(listed.sent.as_deref(), Some(first.as_str()), "with its id");
}

/// 48b review (MAJOR 3): a request whose document moved on after a crash is only copied or discarded, as the recovered list
/// says; a request typed again then is a new one, for the document as it is now.
#[test]
fn a_request_whose_document_moved_on_after_a_crash_is_not_repeated() {
    let mut h = Harness::new();
    board_of(&mut h.app, 1);
    let first = send_board_request(&mut h, "제목 정리");
    h.crash_and_restart();
    board_of(&mut h.app, 2);
    let draft = recovered_of(&h, "canvas-1").expect("the request is recovered");
    assert!(!h.app.draft_standing(&draft).ready, "a request for an older revision is not restored");
    let again = send_board_request(&mut h, "제목 정리");
    assert_ne!(again, first, "the request typed again is a new one");
}

/// 48b review (MINOR): a draft file is not written past `MAX_FILE_BYTES`, so the next open never moves the whole file aside.
#[test]
fn a_draft_file_past_the_size_limit_is_not_written() {
    let mut h = Harness::new();
    h.app.home.prompt = "가".repeat(3 * 1024 * 1024);
    h.app.sync_drafts();
    assert!(!h.app.drafts.flush(), "a file past 8 MiB is not written");
    assert!(!drafts_path(&h.root).exists(), "nothing is written");
    assert!(h.app.drafts.take_notices().iter().any(|notice| notice.contains("8 MiB")), "the user is told why");
}

/// 48b review (MINOR): the start params a draft keeps do not hold the prompt a second time; the repeat takes it from the draft.
#[test]
fn the_start_a_draft_keeps_holds_the_prompt_once() {
    let mut h = Harness::new();
    board_of(&mut h.app, 1);
    send_board_request(&mut h, "제목 정리");
    let saved = h.saved();
    let start = saved[0].start.as_ref().expect("the start is kept");
    assert!(start.get("prompt").is_none(), "{start}");
}

#[test]
fn an_unknown_answer_in_this_window_stays_copy_only_after_leaving_the_document() {
    for (id, kind) in [("figure-1", ArtifactKind::Figure), ("canvas-1", ArtifactKind::Canvas)] {
        let mut h = Harness::new();
        h.app.screen = Screen::Editor;
        h.app.active = Some(summary(id, kind, 1));
        let ticket = request_from_tab(&mut h, "제목 정리");
        h.answer(ticket, Err(DltError::new(ErrorCode::Io, "lost reply")));
        assert!(h.app.unknown_starts.contains_key(id));
        h.app.leave_drafts_for_switch();
        let draft = recovered_of(&h, id).expect("the released request is still listed");
        let standing = h.app.draft_standing(&draft);
        assert!(!standing.ready);
        assert!(standing.reason.expect("the reason").contains("결과를 알 수 없어"));
        h.app.restore_draft(&draft.key);
        assert!(h.app.ai_prompt.is_empty());
        h.app.drafts.discard(&draft.key);
        assert!(h.saved().is_empty(), "the sender can discard its unknown request");
    }
}

#[test]
fn another_windows_discard_cannot_remove_a_request_waiting_for_its_answer() {
    let mut h = Harness::new();
    let ticket = h.create_home("잃으면 안 되는 요청");
    let mut other = DraftBook::open(drafts_path(&h.root));
    let draft = other.recovered().pop().expect("the pending home request");
    other.discard(&draft.key);
    assert_eq!(h.saved(), [draft]);
    h.answer(ticket, Err(DltError::invalid("refused")));
    assert_eq!(h.app.home.prompt, "잃으면 안 되는 요청");
    assert!(h.saved().iter().all(|draft| draft.sent.is_none()));
}

#[test]
fn a_stale_discard_cannot_remove_a_claim_with_unwritten_edits() {
    let mut h = Harness::new();
    h.app.home.prompt = "처음 요청".into();
    h.settle();
    h.crash_and_restart();
    let draft = h.app.drafts.recovered().pop().expect("the original draft");
    let mut other = DraftBook::open(drafts_path(&h.root));
    h.app.restore_draft(&draft.key);
    h.app.home.prompt = "수정한 요청".into();
    h.app.sync_drafts();
    other.discard(&draft.key);
    assert_eq!(h.saved()[0].body, home_body("처음 요청"), "the stale discard did not remove the persisted row");
    assert!(h.app.drafts.flush());
    let saved = h.saved().pop().expect("the edited draft");
    assert_eq!(saved.key, draft.key);
    assert_eq!(saved.body, home_body("수정한 요청"));
}

#[test]
fn a_dirty_copy_from_an_old_session_cannot_resurrect_the_new_owners_deletion() {
    let h = Harness::new();
    let path = drafts_path(&h.root);
    let now = Instant::now();
    let mut old = DraftBook::open(path.clone());
    old.sync("home", Edit::Has(home_live("저장한 요청")), now);
    assert!(old.flush());
    old.sync("home", Edit::Has(home_live("이전 창의 수정")), now);
    old.end_session_as_crash(); // Keep the stale, dirty memory to exercise its merge after the owner has changed.

    let (mut owner, _replies, _work) = window_on(&h.root);
    let draft = owner.drafts.recovered().pop().expect("the saved request");
    owner.restore_draft(&draft.key);
    assert_eq!(owner.home.prompt, "저장한 요청");
    owner.drafts.clear("home", Edit::Empty, now);
    assert!(DraftBook::open(path.clone()).recovered().is_empty());
    assert!(old.flush());
    old.sync("home", Edit::Has(home_live("이전 창의 수정")), now);
    assert!(old.flush());
    assert!(DraftBook::open(path).recovered().is_empty());
}

#[test]
fn a_failed_recovery_claim_leaves_the_request_for_exactly_one_window() {
    let mut h = Harness::new();
    board_of(&mut h.app, 1);
    let id = send_board_request(&mut h, "복원할 요청");
    h.crash_and_restart();
    board_of(&mut h.app, 1);
    let original = recovered_of(&h, "canvas-1").expect("the pending request");
    let (mut other, _replies, _work) = window_on(&h.root);
    board_of(&mut other, 1);
    let path = drafts_path(&h.root);
    let bytes = std::fs::read(&path).expect("saved draft");

    let held = lock_drafts(&path).expect("drafts lock").expect("this filesystem locks");
    h.app.restore_draft(&original.key);
    assert!(h.app.ai_prompt.is_empty());
    assert!(h.app.notices.current(Instant::now()).expect("lock contention is reported").text.contains("다른 창"));
    drop(held);
    let failure = FailingAside::new(AsideKind::Replace);
    h.app.restore_draft(&original.key);
    drop(failure);
    assert!(h.app.notices.current(Instant::now()).expect("failed save is reported").text.contains("복원하지 못했습니다"));
    assert!(h.app.ai_prompt.is_empty());
    assert_eq!(h.app.unknown_starts.len(), 0);
    assert_eq!(recovered_of(&h, "canvas-1"), Some(original.clone()));
    assert_eq!(std::fs::read(&path).expect("original survives"), bytes);

    other.restore_draft(&original.key);
    assert_eq!(other.ai_prompt, "복원할 요청");
    let claimed = h.saved().pop().expect("durable claim");
    assert!(claimed.owner.is_some() && claimed.owner != original.owner);
    assert_eq!(claimed.sent.as_deref(), Some(id.as_str()));
    h.app.restore_draft(&original.key);
    assert!(h.app.ai_prompt.is_empty(), "the stale second claimant gets no editor content");
    assert_eq!(h.app.unknown_starts.len(), 0);
    h.app.drafts.discard(&original.key);
    assert_eq!(h.saved(), [claimed]);
}

#[test]
fn an_already_open_window_can_claim_an_unchanged_draft_after_its_owner_crashes() {
    let mut h = Harness::new();
    board_of(&mut h.app, 1);
    let id = send_board_request(&mut h, "계속 열려 있던 창");
    let (mut other, _replies, _work) = window_on(&h.root);
    board_of(&mut other, 1);
    let draft = other.drafts.recovered().pop().expect("the pending request");
    assert!(!other.draft_standing(&draft).ready);
    h.app.drafts.end_session_as_crash();
    assert!(other.draft_standing(&draft).ready, "the file's contents did not change");
    other.restore_draft(&draft.key);
    assert_eq!(other.ai_prompt, "계속 열려 있던 창");
    assert_eq!(h.saved()[0].sent.as_deref(), Some(id.as_str()));
}

#[test]
fn opening_the_workspace_cleans_an_unreferenced_crashed_session_but_keeps_live_locks() {
    let mut h = Harness::new();
    let ticket = h.create_home("이미 처리한 요청");
    let first = h.saved()[0].owner.clone().expect("the sender");
    h.app.drafts.answered(ticket, Outcome::Accepted, None, Instant::now());
    let dir = h.root.join(".gui").join("drafts-sessions");
    let orphan = dir.join(format!("{first}.lock"));
    let mut live = DraftBook::open(drafts_path(&h.root));
    live.begin_send(1, "home", home_live("다른 요청"), "r2", false, Instant::now()).expect("the second sender");
    let live_owner = h.saved()[0].owner.clone().expect("the other session");
    h.app.drafts.end_session_as_crash();
    assert!(orphan.is_file());
    let _ = DraftBook::open(drafts_path(&h.root));
    assert!(!orphan.exists());
    assert!(dir.join(format!("{live_owner}.lock")).is_file());
}

#[test]
fn a_recovered_request_keeps_its_target_ids_and_waits_if_a_target_is_missing() {
    let mut h = Harness::new();
    board_of(&mut h.app, 1);
    h.app.ai_prompt = "선택한 항목 정리".into();
    let Edit::Has(live) = h.app.ai_tab_edit() else { panic!("the typed request") };
    let start = json!({
        "request_id": "selected-request", "artifact_id": "canvas-1", "prompt": "선택한 항목 정리",
        "target_ids": ["node-b", "node-a"]
    });
    h.app.drafts.begin_send_start(1, "ai:canvas-1", live, "selected-request", Some(start), true, Instant::now()).unwrap();
    h.crash_and_restart();
    board_of(&mut h.app, 1);
    let draft = recovered_of(&h, "canvas-1").expect("the saved request");
    h.app.restore_draft(&draft.key);
    assert_eq!(h.app.ai_target_ids(), ["node-b", "node-a"]);
    h.app.start_ai_job();
    assert!(h.app.ai_selection_refusal().is_some(), "a missing target cannot widen the scope to the whole board");
    assert!(sent_start_ids(&mut h).is_empty());
}

#[test]
fn a_late_acceptance_can_remove_only_its_matching_mark_after_a_claim() {
    for resend in [false, true] {
        let mut h = Harness::new();
        board_of(&mut h.app, 1);
        let ticket = h.app.next_ticket;
        send_board_request(&mut h, "첫 요청");
        h.app.drafts.end_session_as_crash();
        let (mut next, _replies, _work) = window_on(&h.root);
        board_of(&mut next, 1);
        let draft = next.drafts.recovered().pop().expect("the saved request");
        next.restore_draft(&draft.key);
        assert_eq!(next.ai_prompt, "첫 요청");
        if resend {
            next.allow_data = !next.allow_data;
            next.start_ai_job();
            assert_ne!(h.saved()[0].sent, draft.sent);
        }
        h.app.answer_sent_outcome(ticket, Outcome::Accepted);
        next.drafts.sync("other", Edit::Has(home_live("별도 초안")), Instant::now());
        assert!(next.drafts.flush());
        assert_eq!(h.saved().iter().any(|saved| saved.key == draft.key), resend);
    }
}

#[test]
fn an_unreadable_recovery_file_is_reported_without_claiming_the_cached_request() {
    let mut h = Harness::new();
    board_of(&mut h.app, 1);
    send_board_request(&mut h, "저장된 요청");
    h.crash_and_restart();
    board_of(&mut h.app, 1);
    let draft = recovered_of(&h, "canvas-1").expect("the cached request");
    let path = drafts_path(&h.root);
    std::fs::write(&path, b"broken").unwrap();
    h.app.restore_draft(&draft.key);
    assert!(h.app.ai_prompt.is_empty());
    assert_eq!(h.app.unknown_starts.len(), 0);
    assert!(h.app.notices.current(Instant::now()).expect("the read error").text.contains("읽을 수 없습니다"));
    assert_eq!(std::fs::read(path).unwrap().as_slice(), b"broken");
}

#[test]
fn restoring_at_the_unknown_start_limit_names_the_request_that_was_dropped() {
    let mut h = Harness::new();
    board_of(&mut h.app, 1);
    send_board_request(&mut h, "복원할 요청");
    h.crash_and_restart();
    board_of(&mut h.app, 1);
    for i in 0..UNKNOWN_STARTS_MAX {
        let params = json!({ "artifact_id": "canvas-1", "request_id": format!("old-{i}"), "prompt": format!("이전 요청 {i}") });
        let sent = SentStart::of(&params).unwrap();
        h.app.settle_job_start(sent, &Err(DltError::new(ErrorCode::Io, "lost reply")));
    }
    let draft = recovered_of(&h, "canvas-1").expect("the recovered request");
    h.app.restore_draft(&draft.key);
    assert_eq!(h.app.ai_prompt, "복원할 요청");
    assert_eq!(h.app.unknown_starts.len(), UNKNOWN_STARTS_MAX);
    assert!(h.app.notices.current(Instant::now()).expect("the eviction warning").text.contains("이전 요청 0"));
}
