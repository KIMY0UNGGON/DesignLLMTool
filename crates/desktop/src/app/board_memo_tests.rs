use designllm_contracts::{
    new_id, Artifact, ArtifactBody, ArtifactKind, ArtifactSummary, CanvasBoardMemo, CanvasBoardMemoKind, CanvasDocument,
    ContextSnapshot, DltError, ErrorCode, Rect, Selection, Size,
};
use eframe::egui::{Context, Pos2, Rect as ScreenRect, Vec2};

use super::{
    memo_copy_payload, observe_gesture_transform, remove_page_memo, replace_page_memo,
    screen_to_page_emu, BoardMemoEditor, BoardMemoGesture, BoardMemoMutation, DesktopApp,
};
use super::super::{board::Tool, drafts::{drafts_path, DraftBook}};
use crate::rpc::RpcWorker;

fn memo(id: &str, page_id: &str, text: &str, color: &str, bounds: Rect) -> CanvasBoardMemo {
    CanvasBoardMemo {
        kind: CanvasBoardMemoKind::Memo,
        id: id.into(),
        page_id: page_id.into(),
        bounds,
        text: text.into(),
        color: color.into(),
    }
}

#[test]
fn page_memo_crud_changes_only_the_requested_page_list() {
    let first = memo("m1", "p1", "first", "#FFF2A8", Rect { x: 1, y: 2, width: 3, height: 4 });
    let second = memo("m2", "p1", "second", "#E0F2FE", Rect { x: 5, y: 6, width: 7, height: 8 });
    let page_two = memo("m3", "p2", "other page", "#FFFFFF", Rect { x: 9, y: 10, width: 11, height: 12 });

    let created = memo("m4", "p1", "new", "#FFF2A8", Rect { x: 13, y: 14, width: 15, height: 16 });
    let after_create = replace_page_memo(&[first.clone(), second.clone()], &created, true).expect("create");
    assert_eq!(after_create, [first.clone(), second.clone(), created.clone()]);

    let updated = memo("m2", "p1", "updated", "#A7F3D0", Rect { x: 20, y: 21, width: 22, height: 23 });
    let after_update = replace_page_memo(&[first.clone(), second.clone()], &updated, false).expect("update");
    assert_eq!(after_update, [first.clone(), updated]);

    let after_delete = remove_page_memo(&[first.clone(), second.clone()], "m2").expect("delete");
    assert_eq!(after_delete, [first.clone()]);
    assert_eq!(page_two.page_id, "p2", "page-scoped edits do not include other pages");
}

#[test]
fn screen_coordinates_round_trip_outside_the_page() {
    let page_rect = ScreenRect::from_min_size(Pos2::new(100.0, 200.0), eframe::egui::vec2(400.0, 200.0));
    let point = Pos2::new(50.0, 80.0);
    let page = Size { width: 4_000, height: 1_000 };
    let (x, y) = screen_to_page_emu(point, page_rect, page).expect("valid transform");
    assert_eq!((x, y), (-500, -600));
    let screen = super::page_to_screen(Rect { x, y, width: 0, height: 0 }, page, page_rect);
    assert!((screen.min.x - point.x).abs() < 0.01);
    assert!((screen.min.y - point.y).abs() < 0.01);
}

#[test]
fn pointer_up_after_page_disappears_preserves_drag_for_copy_or_discard() {
    let (rpc, _replies, _work) = RpcWorker::detached();
    let mut app = DesktopApp::new(rpc, "workspace".into(), None, None);
    // The active Canvas has lost the gesture's page while another editor commits a page removal.
    let mut artifact = Artifact::new(
        ArtifactKind::Canvas,
        "Canvas",
        "engine",
        ArtifactBody::Canvas(CanvasDocument::default()),
    );
    artifact.id = "a1".into();
    artifact.revision = 2;
    app.active = Some(ArtifactSummary {
        id: "a1".into(),
        kind: ArtifactKind::Canvas,
        title: "Canvas".into(),
        revision: 2,
        engine_id: "engine".into(),
        dirty: true,
        modified_at_ms: None,
        active_jobs: Vec::new(),
    });
    app.artifact = Some(artifact);
    app.context = Some(ContextSnapshot {
        workspace_id: "w1".into(),
        artifact_id: "a1".into(),
        revision: 2,
        view_id: "v1".into(),
        selection: Selection { node_ids: Vec::new(), text_range: None },
        visible_page: 0,
        selected_properties: serde_json::Value::Null,
        capabilities: Default::default(),
        capability_extensions: Default::default(),
        render_snapshot: None,
        view_transform: None,
    });
    let memo = memo("m1", "deleted-page", "keep this note", "#ABCDEF", Rect { x: 50, y: 80, width: 400, height: 300 });
    let page_rect = ScreenRect::from_min_size(Pos2::ZERO, eframe::egui::vec2(200.0, 200.0));
    app.board_memo_gesture = Some(BoardMemoGesture {
        artifact_id: "a1".into(),
        revision: 1,
        page_id: "deleted-page".into(),
        page_index: 0,
        start: Pos2::new(100.0, 100.0),
        page_rect,
        page_size: Size { width: 2_000, height: 2_000 },
        tool_at_start: Tool::Select,
        space_down_at_start: false,
        zoom_at_start: 1.0,
        pan_at_start: Vec2::ZERO,
        transform_id_at_start: None,
        invalidated: false,
        memo_id: Some(memo.id.clone()),
        original_bounds: Some(memo.bounds),
        expected: vec![memo.clone()],
        memo: memo.clone(),
    });

    let ctx = Context::default();
    app.observe_board_memo_gesture(&ctx);
    app.finish_board_memo_gesture_at(Some(Pos2::new(80.0, 70.0)));

    let mutation = app.board_memo_mutation.as_ref().expect("stale move remains visible");
    assert!(!mutation.pending);
    assert!(mutation.message.as_deref().is_some_and(|message| message.contains("보존")));
    let draft = mutation.editor.as_ref().expect("memo is recoverable");
    assert_eq!(draft.memo.text, memo.text);
    assert_eq!(draft.memo.color, memo.color);
    assert_eq!(draft.memo.page_id, memo.page_id);
    assert_eq!(draft.memo.bounds, Rect { x: -150, y: -220, width: 400, height: 300 });
    assert!(app.has_close_work(), "the uncommitted move is protected by the close guard");
}

#[test]
fn any_space_or_view_transform_transition_invalidates_the_whole_drag() {
    let memo = memo("m1", "p1", "text", "#ABCDEF", Rect { x: 1, y: 2, width: 300, height: 400 });
    let page_rect = ScreenRect::from_min_size(Pos2::ZERO, eframe::egui::vec2(200.0, 200.0));
    let mut gesture = BoardMemoGesture {
        artifact_id: "a1".into(),
        revision: 1,
        page_id: "p1".into(),
        page_index: 0,
        start: Pos2::new(20.0, 20.0),
        page_rect,
        page_size: Size { width: 2_000, height: 2_000 },
        tool_at_start: Tool::Select,
        space_down_at_start: false,
        zoom_at_start: 1.0,
        pan_at_start: Vec2::ZERO,
        transform_id_at_start: Some("transform-1".into()),
        invalidated: false,
        memo_id: Some("m1".into()),
        original_bounds: Some(memo.bounds),
        expected: vec![memo.clone()],
        memo,
    };

    observe_gesture_transform(
        &mut gesture,
        Tool::Hand,
        true,
        true,
        1.25,
        Vec2::new(10.0, 0.0),
        true,
        Some(page_rect.translate(Vec2::new(10.0, 0.0))),
        Some("transform-2"),
    );
    assert!(gesture.invalidated);

    observe_gesture_transform(
        &mut gesture,
        Tool::Select,
        false,
        false,
        1.0,
        Vec2::ZERO,
        true,
        Some(page_rect),
        Some("transform-1"),
    );
    assert!(gesture.invalidated, "returning to the old transform must not revive the drag");
}

#[test]
fn stale_memo_copy_contains_text_page_bounds_and_color() {
    let draft = BoardMemoEditor {
        artifact_id: "a1".into(),
        revision: 3,
        page_id: "p1".into(),
        page_index: 1,
        expected: Vec::new(),
        memo: memo("m1", "p1", "full note text", "#ABCDEF", Rect { x: -10, y: 20, width: 300, height: 400 }),
        is_new: false,
        message: Some("stale".into()),
    };
    let copied = memo_copy_payload(&draft);
    assert!(copied.contains("full note text"));
    assert!(copied.contains("페이지 ID: p1"));
    assert!(copied.contains("위치 EMU: -10, 20"));
    assert!(copied.contains("크기 EMU: 300 × 400"));
    assert!(copied.contains("색상: #ABCDEF"));
}

#[test]
fn board_title_kind_and_text_color_survive_page_list_replacement() {
    let title = CanvasBoardMemo {
        kind: CanvasBoardMemoKind::Title,
        id: "title-1".into(),
        page_id: "p1".into(),
        bounds: Rect { x: 100, y: 200, width: 3_000, height: 400 },
        text: "Board title".into(),
        color: "#18212F".into(),
    };
    let desired = replace_page_memo(&[], &title, true).expect("title is a board-memo variant");
    assert_eq!(desired, [title.clone()]);
    let copied = memo_copy_payload(&BoardMemoEditor {
        artifact_id: "a1".into(),
        revision: 1,
        page_id: "p1".into(),
        page_index: 0,
        expected: Vec::new(),
        memo: desired[0].clone(),
        is_new: true,
        message: Some("stale".into()),
    });
    assert!(copied.contains("종류: 보드 제목"));
    assert!(copied.contains("색상: #18212F"));
}

#[test]
fn unknown_save_outcome_is_not_retryable_and_close_warning_says_it_may_have_committed() {
    let (rpc, _replies, _work) = RpcWorker::detached();
    let mut app = DesktopApp::new(rpc, "workspace".into(), None, None);
    let memo = memo("m1", "p1", "keep this note", "#ABCDEF", Rect { x: 10, y: 20, width: 300, height: 400 });
    let editor = BoardMemoEditor {
        artifact_id: "a1".into(),
        revision: 1,
        page_id: "p1".into(),
        page_index: 0,
        expected: Vec::new(),
        memo: memo.clone(),
        is_new: true,
        message: None,
    };
    app.board_memo_mutation = Some(BoardMemoMutation {
        token: "t1".into(),
        artifact_id: "a1".into(),
        revision: 1,
        page_id: "p1".into(),
        page_index: 0,
        expected: Vec::new(),
        desired: vec![memo],
        selected_memo_id: Some("m1".into()),
        editor: Some(editor),
        message: None,
        pending: true,
        retry_allowed: false,
        outcome_uncertain: false,
        held_edit_id: None,
    });

    app.fail_board_memo_change("t1", &DltError::new(ErrorCode::Unavailable, "connection lost"));

    let mutation = app.board_memo_mutation.as_ref().expect("draft remains");
    assert!(mutation.outcome_uncertain);
    assert!(!mutation.retry_allowed);
    assert!(mutation.editor.is_some());
    assert!(app.held_edit_close_warning().is_some_and(|warning| warning.contains("반영됐을 수")));
}

/// A change of a page's memos that holds the new memo `id` as its editor's draft.
fn memo_change(token: &str, id: &str, pending: bool) -> BoardMemoMutation {
    let draft = memo(id, "p1", "버릴 메모", "#FFF2A8", Rect { x: 10, y: 20, width: 300, height: 200 });
    BoardMemoMutation {
        token: token.into(),
        artifact_id: "a1".into(),
        revision: 1,
        page_id: "p1".into(),
        page_index: 0,
        expected: Vec::new(),
        desired: vec![draft.clone()],
        selected_memo_id: Some(id.into()),
        editor: Some(BoardMemoEditor {
            artifact_id: "a1".into(),
            revision: 1,
            page_id: "p1".into(),
            page_index: 0,
            expected: Vec::new(),
            memo: draft,
            is_new: true,
            message: None,
        }),
        message: None,
        pending,
        retry_allowed: false,
        outcome_uncertain: false,
        held_edit_id: None,
    }
}

#[test]
fn discarding_a_memo_change_takes_its_draft_out_of_the_file_and_a_change_on_its_way_stays() {
    let root = std::env::temp_dir().join(format!("designllm-memo-discard-{}", new_id()));
    let path = drafts_path(&root);
    let (rpc, _replies, _work) = RpcWorker::detached();
    let mut app = DesktopApp::new(rpc, "workspace".into(), None, None);
    app.drafts = DraftBook::open(path.clone());
    app.board_memo_mutation = Some(memo_change("t1", "m1", false));
    app.sync_drafts();
    app.drafts.flush();
    assert_eq!(DraftBook::open(path.clone()).recovered().len(), 1, "the memo's draft is in the file");

    app.discard_board_memo_mutation("t1");
    assert!(app.board_memo_mutation.is_none(), "the change is discarded");
    assert!(DraftBook::open(path.clone()).recovered().is_empty(), "the discarded memo's draft leaves the file");

    app.board_memo_mutation = Some(memo_change("t2", "m2", true));
    app.discard_board_memo_mutation("t2");
    assert!(app.board_memo_mutation.is_some(), "a change that is being saved is not discarded");
    let _ = std::fs::remove_dir_all(&root);
}
