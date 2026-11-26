//! Task 68d: every dialog and board notice in the house style, driven through `App::raw_input_hook` and `App::update` with a
//! test `eframe::Frame` over a real host. Each one is opened from the state that opens it, and the tests read where its
//! controls were drawn (`ui.layout`, the UI-test recording) and where egui placed it (its area), press its buttons and keys
//! as a person does, and check what the buttons did, which buttons stay a person's (task 69) and which keys answer it.

use std::time::{Duration, Instant};

use designllm_contracts::{ArtifactBody, CanvasBoardMemo, CanvasBoardMemoKind, CanvasPageSection, Rect as EmuRect, UiLayoutControl};
use eframe::egui::{self, Event, Key, Modifiers, Pos2, Rect, ViewportCommand, ViewportId};
use serde_json::{json, Value};

use super::super::{
    drafts::{Body, Live},
    page_sections::SectionEditor,
    test_support::{canvas_with_page, primary_button, screen, Host, Window},
    ui_test_input::UiTestInput,
    BoardMemoEditor, BoardMemoMutation, CsvImportDraft, ExportConfirmation, FigureForm, Screen,
};

const WIDTH: f32 = 1280.0;
const HEIGHT: f32 = 800.0;

/// A window on a host document, its frames run as eframe runs them, recording its controls for `ui.layout`.
struct Bench {
    host: Host,
    window: Window,
    artifact: String,
    clock: f64,
}

impl Bench {
    fn canvas() -> Self {
        let mut host = Host::start();
        let canvas = canvas_with_page(&mut host, "68d 대화상자");
        Self::open(host, canvas)
    }

    fn figure() -> Self {
        let mut host = Host::start();
        let figure = host.create_figure();
        Self::open(host, figure)
    }

    fn open(mut host: Host, artifact: String) -> Self {
        let mut window = Window::open(&mut host, &artifact);
        window.app.screen = Screen::Editor;
        window.app.ui_test = Some(UiTestInput::default());
        let mut bench = Self { host, window, artifact, clock: 1.0 };
        bench.frames(4);
        bench
    }

    /// One frame: the raw input hook, then `App::update`; then the host answers what the frame sent.
    fn frame_with(&mut self, events: Vec<Event>) -> egui::FullOutput {
        self.clock += 0.05;
        let mut input = screen(1.0, WIDTH, HEIGHT);
        input.time = Some(self.clock);
        input.events = events;
        let ctx = self.window.ctx.clone();
        eframe::App::raw_input_hook(&mut self.window.app, &ctx, &mut input);
        let mut frame = eframe::Frame::_new_kittest();
        let output = ctx.run(input, |ctx| eframe::App::update(&mut self.window.app, ctx, &mut frame));
        self.window.settle(&mut self.host);
        output
    }

    fn frames(&mut self, count: usize) {
        for _ in 0..count {
            self.frame_with(Vec::new());
        }
    }

    /// A person's key press, its release in the next frame, then a frame without input.
    fn key(&mut self, key: Key) -> egui::FullOutput {
        let event = |pressed| Event::Key { key, physical_key: Some(key), pressed, repeat: false, modifiers: Modifiers::NONE };
        let output = self.frame_with(vec![event(true)]);
        self.frame_with(vec![event(false)]);
        self.frames(1);
        output
    }

    /// A person's click at `at` (pressed and released in one frame), then a frame without input.
    fn click_at(&mut self, at: Pos2) -> egui::FullOutput {
        let output = self.frame_with(vec![Event::PointerMoved(at), primary_button(at, true), primary_button(at, false)]);
        self.frames(1);
        output
    }

    /// The control labelled `label` the last frame drew (the last one of that label).
    fn control(&self, label: &str) -> UiLayoutControl {
        self.window
            .app
            .ui_layout()
            .controls
            .into_iter()
            .filter(|control| control.label == label && control.visible)
            .last()
            .unwrap_or_else(|| panic!("the last frame drew {label}"))
    }

    fn drawn(&self, label: &str) -> bool {
        self.window.app.ui_layout().controls.iter().any(|control| control.label == label && control.visible)
    }

    fn press(&mut self, label: &str) -> egui::FullOutput {
        let at = centre(&self.control(label));
        self.click_at(at)
    }

    /// Where egui placed the dialog of `id` (its area).
    fn area(&self, id: impl std::hash::Hash) -> Rect {
        self.window.ctx.memory(|memory| memory.area_rect(egui::Id::new(id))).expect("the dialog's area is shown")
    }
}

fn centre(control: &UiLayoutControl) -> Pos2 {
    Pos2::new(control.rect.x + control.rect.width / 2.0, control.rect.y + control.rect.height / 2.0)
}

fn rect_of(control: &UiLayoutControl) -> Rect {
    Rect::from_min_size(Pos2::new(control.rect.x, control.rect.y), egui::vec2(control.rect.width, control.rect.height))
}

/// The dialog sits in the middle of the window, and the buttons `labels` (one line) in the middle of the dialog.
fn assert_centred(bench: &Bench, id: impl std::hash::Hash, labels: &[&str]) {
    let area = bench.area(id);
    assert!((area.center().x - WIDTH / 2.0).abs() < 1.5 && (area.center().y - HEIGHT / 2.0).abs() < 1.5, "the dialog {area:?} is centred");
    let row = labels.iter().map(|label| rect_of(&bench.control(label))).reduce(Rect::union).expect("buttons");
    assert!((row.center().x - area.center().x).abs() < 1.5, "the buttons {row:?} are centred in {area:?}");
    assert!(area.contains_rect(row), "the buttons stay inside the dialog");
}

fn closes(output: &egui::FullOutput) -> bool {
    output.viewport_output.get(&ViewportId::ROOT).is_some_and(|viewport| viewport.commands.contains(&ViewportCommand::Close))
}

fn section_editor(bench: &mut Bench) -> SectionEditor {
    let artifact = bench.host.artifact(&bench.artifact);
    let ArtifactBody::Canvas(document) = &artifact.body else { panic!("a canvas") };
    SectionEditor {
        artifact_id: bench.artifact.clone(),
        revision: artifact.revision,
        expected: document.page_sections.clone(),
        section: CanvasPageSection { id: "section-1".into(), name: "1장".into(), start_page_id: document.pages[0].id.clone() },
        page_index: 0,
        is_new: true,
        message: None,
    }
}

#[test]
fn the_close_question_is_a_centred_modal_whose_safe_choice_takes_enter_escape_and_the_backdrop() {
    let mut bench = Bench::canvas();
    // An unsaved section editor is work the close question names; its own dialog is drawn under the modal.
    let editor = section_editor(&mut bench);
    bench.window.app.page_section_editor = Some(editor);
    bench.window.app.close_confirmation_open = true;
    bench.frames(3);
    let discard = bench.window.app.close_discard_label(false);
    assert_centred(&bench, "held-edit-close-confirmation", &["계속 작업", discard]);
    assert!(!bench.control("계속 작업").human_only && bench.control(discard).human_only, "the discard stays a person's");

    bench.key(Key::Enter);
    assert!(!bench.window.app.close_confirmation_open, "Enter keeps working");
    assert!(bench.window.app.page_section_editor.is_some(), "the key did not reach the section dialog under the modal");
    bench.window.app.close_confirmation_open = true;
    bench.frames(2);
    bench.key(Key::Escape);
    assert!(!bench.window.app.close_confirmation_open, "Esc keeps working");
    assert!(bench.window.app.page_section_editor.is_some(), "Esc did not cancel the section dialog under the modal");
    bench.window.app.close_confirmation_open = true;
    bench.frames(2);
    bench.click_at(Pos2::new(12.0, HEIGHT - 12.0));
    assert!(!bench.window.app.close_confirmation_open, "a press on the backdrop keeps working");

    bench.window.app.close_confirmation_open = true;
    bench.frames(2);
    let output = bench.press(discard);
    assert!(closes(&output), "the discard closes the window");
    assert!(bench.window.app.page_section_editor.is_none(), "the section draft is dropped by the discard");
}

#[test]
fn the_figure_switch_question_keeps_working_on_enter_and_escape_and_switches_only_on_its_button() {
    let mut bench = Bench::figure();
    let draft = CsvImportDraft::to_declare(bench.host.import_table("x,y\n1,2\n3,4\n"));
    let summary = bench.window.app.active.clone().expect("the figure is open");
    for key in [Key::Enter, Key::Escape] {
        bench.window.app.csv_draft = Some(draft.clone());
        bench.window.app.pending_switch = Some(summary.clone());
        bench.frames(3);
        assert_centred(&bench, "figure-switch-confirmation", &["계속 작업", "전환"]);
        assert!(bench.control("계속 작업").human_only && bench.control("전환").human_only, "both answers are a person's");
        bench.key(key);
        assert!(bench.window.app.pending_switch.is_none(), "{key:?} answers the question");
        assert!(bench.window.app.csv_draft.is_some(), "{key:?} keeps working: the CSV draft stays");
    }
    // The frame the question opens in takes no Enter: the key that led to it is not its answer.
    bench.window.app.csv_draft = Some(draft);
    bench.window.app.pending_switch = Some(summary);
    bench.key(Key::Enter);
    assert!(bench.window.app.pending_switch.is_some(), "the opening frame's Enter does not answer");
    bench.frames(1);
    bench.press("전환");
    assert!(bench.window.app.pending_switch.is_none());
    assert!(bench.window.app.csv_draft.is_none(), "전환 opens the document again without the draft");
}

#[test]
fn the_lost_host_modal_has_no_safe_key_and_closes_only_by_its_button() {
    let mut bench = Bench::canvas();
    bench.window.app.host_link.lost = true;
    bench.frames(3);
    assert_centred(&bench, "host-link-lost", &[super::super::host_link::RECONNECT, super::super::host_link::QUIT]);
    let reconnect = bench.control(super::super::host_link::RECONNECT);
    assert!(reconnect.human_only && !reconnect.enabled, "without a starter the reconnect is shown off");
    assert!(bench.control(super::super::host_link::QUIT).human_only);
    for key in [Key::Enter, Key::Escape] {
        let output = bench.key(key);
        assert!(bench.window.app.host_link.lost && !closes(&output), "{key:?} neither reconnects nor closes");
    }
    let output = bench.click_at(Pos2::new(12.0, HEIGHT - 12.0));
    assert!(bench.window.app.host_link.lost && !closes(&output), "the backdrop does nothing either");
    let output = bench.press(super::super::host_link::QUIT);
    assert!(closes(&output), "종료 closes the window");
}

/// Queues `home.create` as an agent would and draws until the window's approval is on screen; returns the run id.
fn ask_for_a_model_run(bench: &mut Bench, prompt: &str) -> String {
    bench.window.app.ai_model = "claude-opus-5-5".into();
    bench.window.step(&mut bench.host, "nav.home", json!({})).expect("home");
    bench.frames(3);
    bench.window.app.publish_ui_if_due();
    bench.window.settle(&mut bench.host);
    let view_id = bench.window.app.ui.view_id.clone();
    let queued = bench
        .host
        .call(
            "ui.command.run",
            json!({
                "view_id": view_id, "command_id": "home.create", "actor": "agent", "permission": "agent_start",
                "allowed_intents": [], "args": { "tile": "blank", "prompt": prompt }, "timeout_ms": 60_000,
            }),
        )
        .expect("the agent's run is queued");
    bench.window.app.last_event_poll = Instant::now() - Duration::from_secs(3);
    for _ in 0..8 {
        bench.frames(1);
        if bench.window.app.has_home_confirmation() {
            break;
        }
    }
    assert!(bench.window.app.has_home_confirmation(), "the approval is on screen");
    bench.frames(2);
    queued["run_id"].as_str().expect("a run id").to_owned()
}

fn run_record(bench: &mut Bench, run_id: &str) -> Value {
    bench.host.call("ui.command.get", json!({ "run_id": run_id })).expect("the run's record")
}

#[test]
fn the_model_run_approval_refuses_on_escape_and_on_the_backdrop_and_runs_on_enter() {
    let mut bench = Bench::canvas();
    for refuse in ["Escape", "backdrop"] {
        let run_id = ask_for_a_model_run(&mut bench, "모델 실행 요청");
        assert_centred(&bench, "agent-home-create", &["거절", "모델 실행"]);
        assert!(bench.control("거절").human_only && bench.control("모델 실행").human_only);
        match refuse {
            "Escape" => {
                bench.key(Key::Escape);
            }
            _ => {
                bench.click_at(Pos2::new(12.0, HEIGHT - 12.0));
            }
        }
        assert!(!bench.window.app.has_home_confirmation(), "{refuse} answers the approval");
        let run = run_record(&mut bench, &run_id);
        assert!(run["error"]["message"].as_str().is_some_and(|message| message.contains("승인하지 않아")), "{refuse} is 거절: {run}");
    }
    let run_id = ask_for_a_model_run(&mut bench, "모델 실행 요청");
    bench.key(Key::Enter);
    assert!(!bench.window.app.has_home_confirmation(), "Enter answers the approval");
    let run = run_record(&mut bench, &run_id);
    assert!(!run.to_string().contains("승인하지 않아"), "Enter is 모델 실행, not 거절: {run}");
}

/// The approvals stay a person's with the new keys (task 69): a synthetic Enter or Esc during the close question or the export
/// losses ends the run with HUMAN_ONLY before the window sees it, and the question stays. (The model-run approval:
/// `ui_test_input_tests::real_approvals_refuse_synthetic_presses_and_keys`.)
#[test]
fn synthetic_enter_and_escape_never_answer_an_approval() {
    let mut bench = Bench::canvas();
    let editor = section_editor(&mut bench);
    bench.window.app.page_section_editor = Some(editor);
    bench.window.app.close_confirmation_open = true;
    bench.frames(3);
    for key in ["Enter", "Escape"] {
        let run_id = queue_input(&mut bench, json!([{ "type": "key", "key": key }]));
        let run = finish(&mut bench, &run_id);
        assert_eq!(run["error"]["diagnostics"][0]["code"], "HUMAN_ONLY", "{key}: {run}");
        assert!(bench.window.app.close_confirmation_open, "{key}: the close question waits for a person");
    }
    bench.window.app.close_confirmation_open = false;
    bench.window.app.page_section_editor = None;
    let confirmation = export_confirmation(&mut bench);
    bench.window.app.export_confirmation = Some(confirmation);
    bench.frames(3);
    for key in ["Enter", "Escape"] {
        let run_id = queue_input(&mut bench, json!([{ "type": "key", "key": key }]));
        let run = finish(&mut bench, &run_id);
        assert_eq!(run["error"]["diagnostics"][0]["code"], "HUMAN_ONLY", "{key}: {run}");
        assert!(bench.window.app.export_confirmation.is_some(), "{key}: the export losses wait for a person");
    }
}

fn queue_input(bench: &mut Bench, events: Value) -> String {
    bench.window.app.publish_ui_if_due();
    bench.window.settle(&mut bench.host);
    let view_id = bench.window.app.ui.view_id.clone();
    let queued = bench
        .host
        .call(
            "ui.command.run",
            json!({
                "view_id": view_id, "command_id": designllm_contracts::UI_INPUT_COMMAND, "actor": "agent", "permission": "agent_start",
                "allowed_intents": ["content", "design", "data"], "args": { "events": events }, "timeout_ms": 60_000,
            }),
        )
        .expect("the run is queued");
    bench.window.app.last_event_poll = Instant::now() - Duration::from_secs(3);
    queued["run_id"].as_str().expect("a run id").to_owned()
}

fn finish(bench: &mut Bench, run_id: &str) -> Value {
    for _ in 0..200 {
        bench.frames(1);
        let run = run_record(bench, run_id);
        if !matches!(run["status"].as_str(), Some("queued" | "delivered")) {
            return run;
        }
    }
    panic!("the run {run_id} did not end");
}

fn export_confirmation(bench: &mut Bench) -> ExportConfirmation {
    let revision = bench.host.artifact(&bench.artifact).revision;
    let diagnostic = serde_json::from_value(json!({ "code": "FONT_SUBSTITUTED", "message": "글꼴 대체", "node_id": null, "blocking": true }))
        .expect("a diagnostic");
    ExportConfirmation {
        request: designllm_contracts::ExportFileRequest {
            request_id: designllm_contracts::new_id(),
            options: designllm_contracts::ExportRequest::new(bench.artifact.clone(), revision, "pdf", designllm_contracts::Editability::None),
            target_path: std::env::temp_dir().join(format!("designllm-68d-{}.pdf", designllm_contracts::new_id())).to_string_lossy().into_owned(),
            overwrite: false,
            expected_target_hash: None,
            dry_run: None,
        },
        diagnostics: vec![diagnostic],
        allowed: Default::default(),
    }
}

#[test]
fn the_export_losses_take_house_check_boxes_and_enter_never_accepts_a_loss() {
    let mut bench = Bench::canvas();
    let confirmation = export_confirmation(&mut bench);
    bench.window.app.export_confirmation = Some(confirmation.clone());
    bench.frames(3);
    assert_centred(&bench, "내보내기 확인", &["취소", "이 손실을 허용하고 내보내기"]);
    let accept = bench.control("이 손실을 허용하고 내보내기");
    assert!(accept.human_only && !accept.enabled, "nothing allowed yet: the accept button is off");
    bench.key(Key::Enter);
    assert!(bench.window.app.export_confirmation.is_some(), "Enter does not accept a loss");
    bench.press("글꼴 대체");
    let allowed = bench.window.app.export_confirmation.as_ref().map(|confirmation| confirmation.allowed.clone()).unwrap_or_default();
    assert!(allowed.contains("FONT_SUBSTITUTED"), "the check box allows the loss: {allowed:?}");
    assert!(bench.control("이 손실을 허용하고 내보내기").enabled, "one loss allowed: the accept button is on");
    bench.key(Key::Enter);
    assert!(bench.window.app.export_confirmation.is_some(), "Enter does not accept a loss, even with the button on");
    bench.key(Key::Escape);
    assert!(bench.window.app.export_confirmation.is_none(), "Esc is 취소");
    bench.window.app.export_confirmation = Some(confirmation);
    bench.frames(3);
    bench.press("취소");
    assert!(bench.window.app.export_confirmation.is_none(), "취소 closes it");
}

#[test]
fn the_evidence_form_is_centred_with_house_fields_and_closes_on_escape_and_cancel() {
    let mut bench = Bench::figure();
    for close in ["Escape", "취소"] {
        bench.window.app.figure_form = Some(FigureForm::Evidence);
        bench.frames(3);
        assert_centred(&bench, "근거 추가", &["취소", "근거 추가"]);
        assert!(bench.drawn("PDF 파일 경로") && bench.drawn("쪽"), "the house text fields");
        assert!(!bench.control("근거 추가").enabled, "no path yet");
        bench.key(Key::Enter);
        assert!(bench.window.app.figure_form.is_some(), "Enter does nothing while the main button is off");
        match close {
            "Escape" => {
                bench.key(Key::Escape);
            }
            label => {
                bench.press(label);
            }
        }
        assert!(bench.window.app.figure_form.is_none(), "{close} closes the form");
    }
}

#[test]
fn the_chart_form_offers_its_types_in_house_menus() {
    let mut bench = Bench::figure();
    // The path step first, as a person goes: the columns step that follows grows the dialog to its rows and buttons.
    bench.window.app.figure_form = Some(FigureForm::Chart);
    bench.frames(3);
    assert_centred(&bench, "차트 추가", &["취소", "가져오기"]);
    bench.window.app.csv_draft = Some(CsvImportDraft::to_declare(bench.host.import_table("이름,값\na,1\nb,2\n")));
    bench.frames(3);
    assert_centred(&bench, "차트 추가", &["취소", "확인"]);
    let types = |bench: &Bench| bench.window.app.csv_draft.as_ref().map(|draft| draft.columns[1].data_type.clone()).unwrap_or_default();
    let before = types(&bench);
    // The second column's type menu: its value is the type's word, and its menu rows are the four types.
    let shown = match before.as_str() {
        "numeric" => "숫자",
        "category" => "범주",
        "count" => "개수",
        _ => "텍스트",
    };
    let menus: Vec<UiLayoutControl> = bench.window.app.ui_layout().controls.into_iter().filter(|control| control.label == shown).collect();
    let menu = menus.last().cloned().expect("the type menu is a house dropdown");
    bench.click_at(centre(&menu));
    let other = if before == "category" { "숫자" } else { "범주" };
    bench.press(other);
    assert_ne!(types(&bench), before, "the menu row sets the column's type");
}

#[test]
fn the_page_section_editor_saves_on_enter_and_cancels_on_escape() {
    let mut bench = Bench::canvas();
    let editor = section_editor(&mut bench);
    bench.window.app.page_section_editor = Some(editor.clone());
    bench.frames(3);
    assert_centred(&bench, ("page-section-editor", bench.artifact.as_str(), "section-1"), &["저장", "취소"]);
    assert!(bench.drawn("섹션 이름"), "the name is a house text field");
    bench.key(Key::Escape);
    assert!(bench.window.app.page_section_editor.is_none(), "Esc is 취소");
    bench.window.app.page_section_editor = Some(editor);
    bench.frames(3);
    bench.key(Key::Enter);
    bench.frames(3);
    let artifact = bench.host.artifact(&bench.artifact);
    let ArtifactBody::Canvas(document) = &artifact.body else { panic!("a canvas") };
    assert_eq!(document.page_sections.iter().map(|section| section.name.as_str()).collect::<Vec<_>>(), ["1장"], "Enter saves the section");
}

fn memo_mutation(bench: &mut Bench) -> BoardMemoMutation {
    let artifact = bench.host.artifact(&bench.artifact);
    let ArtifactBody::Canvas(document) = &artifact.body else { panic!("a canvas") };
    let page_id = document.pages[0].id.clone();
    let memo = CanvasBoardMemo {
        kind: CanvasBoardMemoKind::Memo,
        id: "memo-1".into(),
        page_id: page_id.clone(),
        bounds: EmuRect { x: 0, y: 0, width: 2_000_000, height: 1_000_000 },
        text: "보존한 메모".into(),
        color: "#FFF2A8".into(),
    };
    let editor = BoardMemoEditor {
        artifact_id: bench.artifact.clone(),
        revision: artifact.revision,
        page_id: page_id.clone(),
        page_index: 0,
        expected: Vec::new(),
        memo: memo.clone(),
        is_new: true,
        message: None,
    };
    BoardMemoMutation {
        token: "token-1".into(),
        artifact_id: bench.artifact.clone(),
        revision: artifact.revision,
        page_id,
        page_index: 0,
        expected: Vec::new(),
        desired: vec![memo],
        selected_memo_id: Some("memo-1".into()),
        editor: Some(editor),
        message: Some("작업물이 바뀌어 메모를 적용하지 않았습니다. 초안을 보존했습니다.".into()),
        pending: false,
        retry_allowed: false,
        outcome_uncertain: false,
        held_edit_id: None,
    }
}

#[test]
fn board_notices_hang_from_the_board_top_middle_stack_without_overlap_and_leave_the_keys_to_the_board() {
    let mut bench = Bench::canvas();
    let live = Live { artifact_id: Some(bench.artifact.clone()), revision: Some(1), body: Body::Ai { prompt: "보내지 않은 요청".into(), figure_refs: Vec::new() } };
    bench.window.app.drafts.keep_recovered(live, Instant::now());
    let mutation = memo_mutation(&mut bench);
    bench.window.app.board_memo_mutation = Some(mutation);
    bench.frames(4);
    let board = bench.window.app.board_rect.expect("the board is drawn");
    let memo = bench.area("board-memo-conflict");
    let drafts = bench.area("복구할 초안");
    assert!((memo.top() - (board.top() + 16.0)).abs() < 1.5, "the first notice hangs 16 under the board's top: {memo:?} in {board:?}");
    assert!(drafts.top() >= memo.bottom() + 7.5, "the second stacks under the first: {memo:?} {drafts:?}");
    for notice in [memo, drafts] {
        assert!((notice.center().x - board.center().x).abs() < 1.5, "centred on the board: {notice:?} in {board:?}");
        assert!(board.contains_rect(notice), "inside the board");
    }
    assert!(bench.control("복원").human_only && bench.control("버리기").human_only, "the draft's restore and discard stay a person's");
    for key in [Key::Enter, Key::Escape] {
        bench.key(key);
        assert!(bench.window.app.board_memo_mutation.is_some() && !bench.window.app.drafts.recovered().is_empty(), "{key:?} answers no notice");
    }
    bench.press("초안 버리기");
    assert!(bench.window.app.board_memo_mutation.is_none(), "초안 버리기 discards the memo change");
    bench.frames(2);
    let alone = bench.area("복구할 초안");
    assert!((alone.top() - (board.top() + 16.0)).abs() < 1.5, "the draft notice moves up once alone: {alone:?}");
}

/// The editor's top bar has no agent mark: the left panel's AI tab is the one way to the AI panel.
#[test]
fn the_editor_top_bar_has_no_agent_mark() {
    let mut bench = Bench::canvas();
    let output = bench.frame_with(Vec::new());
    assert!(top_bar_letter(&output).is_none(), "the top bar draws no runtime letter next to the save button");
}

/// The centre of the `C`/`X` letter drawn in the top bar (44 high), if any.
fn top_bar_letter(output: &egui::FullOutput) -> Option<Pos2> {
    fn walk(shape: &egui::epaint::Shape, found: &mut Option<Pos2>) {
        match shape {
            egui::epaint::Shape::Text(text) if matches!(text.galley.text(), "C" | "X") => {
                let rect = text.visual_bounding_rect();
                if rect.center().y < 44.0 {
                    *found = Some(rect.center());
                }
            }
            egui::epaint::Shape::Vec(shapes) => shapes.iter().for_each(|shape| walk(shape, found)),
            _ => {}
        }
    }
    let mut found = None;
    output.shapes.iter().for_each(|clipped| walk(&clipped.shape, &mut found));
    found
}

/// The home top bar: no mark without a connected agent or a job; with the window's job running, its runtime's mark, which
/// opens the job's document.
#[test]
fn the_home_agent_mark_shows_only_with_a_job_and_opens_its_document() {
    let mut bench = Bench::canvas();
    bench.window.app.screen = Screen::Home;
    let output = bench.frame_with(Vec::new());
    assert!(top_bar_letter(&output).is_none(), "no mark without a job or an agent");
    let job = serde_json::from_value(json!({
        "id": "job-68d", "attempt_id": "attempt-1", "artifact_id": bench.artifact, "base_revision": 1, "status": "running",
        "allowed_intents": [], "model": "mock-model", "effort": "low", "error": null, "runtime": "claude",
    }))
    .expect("a job");
    bench.window.app.job = Some(job);
    // Not polled in these frames: the host knows no such job.
    bench.window.app.last_job_poll = Instant::now();
    bench.window.app.active = None;
    let output = bench.frame_with(Vec::new());
    let letter = top_bar_letter(&output).expect("the job's runtime mark is shown");
    bench.click_at(letter);
    bench.frames(2);
    assert_eq!(bench.window.app.screen, Screen::Editor, "the mark opens the job's document");
    assert_eq!(bench.window.app.active.as_ref().map(|active| active.id.clone()), Some(bench.artifact.clone()));
}

// Round 3: the keys of the open dialog are its own (the board and the shortcuts wait), an IME composition keeps them, only the
// top dialog takes them, a held key does not answer, a long body leaves the buttons in view, a finished job is not marked.

fn key_event(key: Key, pressed: bool, repeat: bool) -> Event {
    Event::Key { key, physical_key: Some(key), pressed, repeat, modifiers: Modifiers::NONE }
}

/// A canvas with the text node `t1`, selected.
fn canvas_with_a_selected_text() -> Bench {
    let mut host = Host::start();
    let canvas = canvas_with_page(&mut host, "68d 선택");
    let node = serde_json::from_value(json!({
        "id": "t1", "kind": "text", "text": "가", "bounds": { "x": 1_000_000, "y": 1_000_000, "width": 3_000_000, "height": 600_000 },
    }))
    .expect("a node");
    let intents = [designllm_contracts::Intent::Content, designllm_contracts::Intent::Design].into_iter().collect();
    host.apply(&canvas, vec![designllm_contracts::Operation::AddNode { page_id: "page-1".into(), parent_id: None, node }], intents)
        .expect("the text");
    let mut bench = Bench::open(host, canvas);
    bench.window.app.set_selected_ids(vec!["t1".into()]);
    bench.frames(3);
    assert_eq!(bench.window.app.selected_nodes, ["t1"], "t1 is selected");
    bench
}

#[test]
fn with_a_node_selected_the_dialog_takes_enter_and_escape_and_the_board_does_not() {
    let mut bench = canvas_with_a_selected_text();
    // Enter answers the section editor: it saves, and the selected text does not start an inline edit.
    let editor = section_editor(&mut bench);
    bench.window.app.page_section_editor = Some(editor);
    bench.frames(3);
    bench.key(Key::Enter);
    bench.frames(3);
    assert!(bench.window.app.page_section_editor.is_none(), "Enter saved the section");
    assert!(bench.window.app.inline_text_edit.is_none(), "the board did not start an inline edit on the same Enter");
    // The saved change reads the canvas again; the test selects the text again for the next key.
    bench.window.app.set_selected_ids(vec!["t1".into()]);
    bench.frames(2);
    assert_eq!(bench.window.app.selected_nodes, ["t1"]);
    // Esc answers the close question: it keeps working, and `select.clear` does not clear the selection.
    // A second section, against the canvas as the save left it (a draft against an older revision takes no keys).
    let mut second = section_editor(&mut bench);
    second.section.id = "section-2".into();
    second.section.name = "2장".into();
    bench.window.app.page_section_editor = Some(second);
    bench.window.app.close_confirmation_open = true;
    bench.frames(3);
    bench.key(Key::Escape);
    assert!(!bench.window.app.close_confirmation_open, "Esc kept working");
    assert!(bench.window.app.page_section_editor.is_some(), "the section editor under the modal is not cancelled");
    assert_eq!(bench.window.app.selected_nodes, ["t1"], "Esc did not clear the selection");
    bench.key(Key::Escape);
    assert!(bench.window.app.page_section_editor.is_none(), "the next Esc cancels the editor, now the top dialog");
    assert_eq!(bench.window.app.selected_nodes, ["t1"], "still selected");
    // With no dialog open the board has its keys again: Enter starts the inline edit of the selected text.
    bench.frames(2);
    bench.key(Key::Enter);
    assert!(bench.window.app.inline_text_edit.is_some(), "Enter edits the selected text once no dialog is open");
}

#[test]
fn an_ime_composition_keeps_the_dialog_open_on_escape_and_enter() {
    let mut bench = Bench::figure();
    bench.window.app.figure_form = Some(FigureForm::Evidence);
    bench.frames(3);
    // Esc while the IME composes cancels the composition, not the form.
    bench.frame_with(vec![Event::Ime(egui::ImeEvent::Enabled), Event::Ime(egui::ImeEvent::Preedit("ㄱ".into()))]);
    bench.frame_with(vec![key_event(Key::Escape, true, false)]);
    bench.frame_with(vec![key_event(Key::Escape, false, false)]);
    assert!(bench.window.app.figure_form.is_some(), "Esc during a composition keeps the form");
    // Enter that commits the composition comes with the commit: it does not answer the form either.
    bench.frame_with(vec![Event::Ime(egui::ImeEvent::Commit("가".into())), key_event(Key::Enter, true, false)]);
    bench.frame_with(vec![key_event(Key::Enter, false, false)]);
    bench.frames(1);
    assert!(bench.window.app.figure_form.is_some(), "the commit's Enter does not answer");
    bench.key(Key::Escape);
    assert!(bench.window.app.figure_form.is_none(), "after the composition Esc closes the form");
}

#[test]
fn only_the_dialog_opened_last_takes_the_keys() {
    let mut bench = Bench::canvas();
    bench.window.app.figure_form = Some(FigureForm::Evidence);
    bench.frames(3);
    let confirmation = export_confirmation(&mut bench);
    bench.window.app.export_confirmation = Some(confirmation);
    bench.frames(3);
    bench.key(Key::Escape);
    assert!(bench.window.app.export_confirmation.is_none(), "Esc closes the export losses, opened last");
    assert!(bench.window.app.figure_form.is_some(), "the form under it stays");
    bench.key(Key::Escape);
    assert!(bench.window.app.figure_form.is_none(), "the next Esc closes the form");
}

#[test]
fn a_held_key_does_not_answer_a_dialog() {
    let mut bench = Bench::figure();
    let draft = CsvImportDraft::to_declare(bench.host.import_table("x,y\n1,2\n"));
    let summary = bench.window.app.active.clone().expect("the figure is open");
    bench.window.app.csv_draft = Some(draft);
    // Enter goes down before the question opens and stays down: the frames after it bring only its repeats.
    bench.frame_with(vec![key_event(Key::Enter, true, false)]);
    bench.window.app.pending_switch = Some(summary);
    for _ in 0..3 {
        bench.frame_with(vec![key_event(Key::Enter, true, true)]);
    }
    for _ in 0..4 {
        bench.frame_with(vec![key_event(Key::Enter, true, true)]);
    }
    bench.frame_with(vec![key_event(Key::Enter, false, false)]);
    assert!(bench.window.app.pending_switch.is_some(), "the repeats of a held Enter do not answer the question");
    bench.key(Key::Enter);
    assert!(bench.window.app.pending_switch.is_none(), "a new press does");
}

#[test]
fn a_long_request_leaves_the_approval_buttons_in_view() {
    let mut bench = Bench::canvas();
    let prompt = (1..=200).map(|line| format!("요청 {line}번째 줄")).collect::<Vec<_>>().join("\n");
    let _run = ask_for_a_model_run(&mut bench, &prompt);
    bench.frames(2);
    for label in ["거절", "모델 실행"] {
        let button = rect_of(&bench.control(label));
        assert!(button.bottom() <= HEIGHT && button.top() >= 0.0, "{label} is on screen: {button:?}");
    }
    assert_centred(&bench, "agent-home-create", &["거절", "모델 실행"]);
    let area = bench.area("agent-home-create");
    assert!(area.height() <= HEIGHT - 2.0 * 24.0 + 1.0, "the dialog keeps to the window and its body scrolls: {area:?}");
}

#[test]
fn a_finished_job_has_no_home_mark() {
    let mut bench = Bench::canvas();
    bench.window.app.screen = Screen::Home;
    for status in ["completed", "failed", "canceled"] {
        let job = serde_json::from_value(json!({
            "id": "job-68d", "attempt_id": "attempt-1", "artifact_id": bench.artifact, "base_revision": 1, "status": status,
            "allowed_intents": [], "model": "mock-model", "effort": "low", "error": null, "runtime": "claude",
        }))
        .expect("a job");
        bench.window.app.job = Some(job);
        bench.window.app.last_job_poll = Instant::now();
        let output = bench.frame_with(Vec::new());
        assert!(top_bar_letter(&output).is_none(), "a {status} job is not marked");
    }
}
