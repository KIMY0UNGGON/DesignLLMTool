//! Tests for the editor AI tab intent segmented control (D10.2), runner/model/effort dropdown (R11),
//! and Figure export menu formats (R07.9).

use std::{path::{Path, PathBuf}, sync::{mpsc::Receiver, Arc}, time::{Duration, Instant}};

use designllm_contracts::{
    new_id, Artifact, ArtifactBody, ArtifactKind, ArtifactSummary, AssetStore, ChangeSet, DltError, DltResult, EncodedOutput,
    EngineAdapter, Editability, FigureImage, FigurePanel, Intent, Job, JobStatus, Operation, PanelBody, RenderScene, RpcRequest,
    PROTOCOL_VERSION,
};
use designllm_figures::FigureEngine;
use eframe::egui::{self, epaint::Shape, Context, Event, Pos2, RawInput, Rect};
use serde_json::json;

use super::super::{chrome::export_formats, DesktopApp, PendingAction, Screen};
use crate::app::figure_tools::{panel_additions, panel_label};
use crate::app::{
    jobs::SELECTION_GONE,
    link_flow_tests::canvas_with_three_boxes,
    test_support::{change_set_arguments, content_of, drawn_text, primary_button, screen, Harness, Host, HostBackend, Mcp, Window},
};
use crate::{
    notice::Severity,
    rpc::{RpcWorker, Work},
    theme,
};

struct AiTabHarness {
    app: DesktopApp,
    work: Receiver<Work>,
    ctx: Context,
}

impl AiTabHarness {
    fn new(kind: ArtifactKind) -> Self {
        let (rpc, _replies, work) = RpcWorker::detached();
        let mut app = DesktopApp::new(rpc, "AI 탭 테스트".into(), None, None);
        while work.try_recv().is_ok() {}
        app.screen = Screen::Editor;
        app.active = Some(ArtifactSummary {
            id: "doc-1".into(),
            kind,
            title: "테스트 문서".into(),
            revision: 1,
            engine_id: match kind {
                ArtifactKind::Figure => "figure",
                _ => "canvas",
            }
            .into(),
            dirty: false,
            modified_at_ms: None,
            active_jobs: Vec::new(),
        });
        // The app's fonts and theme, as the window installs them at start (the other headless harnesses do the same).
        let ctx = Context::default();
        theme::install(&ctx);
        Self { app, work, ctx }
    }

    fn sent(&mut self) -> Vec<(u64, String, serde_json::Value)> {
        let mut batch = Vec::new();
        while let Ok(item) = self.work.try_recv() {
            if let Work::Call { ticket, method, params, .. } = item {
                batch.push((ticket, method, params));
            }
        }
        batch
    }

    fn draw_ai_tab(&mut self, input: RawInput) -> egui::FullOutput {
        let ctx = self.ctx.clone();
        ctx.run(input, |ctx| {
            draw_in_sidebar(ctx, "test-ai-sidebar", theme::SIDEBAR_WIDTH, &mut self.app);
        })
    }

    /// The click's frame, then the idle frame after it: a click changes the state during its frame, and the next frame draws it.
    fn click_then_settle(&mut self, pos: Pos2) -> egui::FullOutput {
        let _ = self.draw_ai_tab(click_at(pos));
        self.draw_ai_tab(screen(1.0, 1000.0, 800.0))
    }

    fn draw_export_menu(&mut self, input: RawInput) -> egui::FullOutput {
        let ctx = self.ctx.clone();
        ctx.run(input, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                let palette = theme::palette(ctx);
                self.app.export_menu(ui, &palette);
            });
        })
    }
}

/// Draws the AI tab in a left panel `width` wide as the window's sidebar does (`chrome.rs`): a panel without a margin, the
/// zero item spacing its tab strip sets, and the tab's vertical scroll area. Returns the panel's column.
fn draw_in_sidebar(ctx: &Context, id: &str, width: f32, app: &mut DesktopApp) -> Rect {
    let mut column = Rect::NOTHING;
    egui::SidePanel::left(id.to_owned()).exact_width(width).frame(egui::Frame::new()).show(ctx, |ui| {
        let palette = theme::palette(ctx);
        ui.spacing_mut().item_spacing = egui::Vec2::ZERO;
        column = ui.max_rect();
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| app.draw_ai_tab(ui, &palette));
    });
    column
}

fn click_at(pos: Pos2) -> RawInput {
    let mut input = screen(1.0, 1000.0, 800.0);
    input.events = vec![Event::PointerMoved(pos), primary_button(pos, true), primary_button(pos, false)];
    input
}

fn find_text_center(output: &egui::FullOutput, expected: &str) -> Pos2 {
    fn visit(shape: &Shape, expected: &str, found: &mut Option<Pos2>) {
        match shape {
            Shape::Text(text) if text.galley.text() == expected => *found = Some(text.pos + text.galley.size() / 2.0),
            Shape::Vec(shapes) => shapes.iter().for_each(|shape| visit(shape, expected, found)),
            _ => {}
        }
    }
    let mut found = None;
    for clipped in &output.shapes {
        visit(&clipped.shape, expected, &mut found);
    }
    found.unwrap_or_else(|| panic!("missing drawn text {expected:?}; got {:?}", drawn_text(output)))
}

/// The last drawn text with this content (the request button's label comes after the field's label).
fn find_last_text_center(output: &egui::FullOutput, expected: &str) -> Pos2 {
    fn visit(shape: &Shape, expected: &str, found: &mut Vec<Pos2>) {
        match shape {
            Shape::Text(text) if text.galley.text() == expected => found.push(text.pos + text.galley.size() / 2.0),
            Shape::Vec(shapes) => shapes.iter().for_each(|shape| visit(shape, expected, found)),
            _ => {}
        }
    }
    let mut found = Vec::new();
    for clipped in &output.shapes {
        visit(&clipped.shape, expected, &mut found);
    }
    *found.last().unwrap_or_else(|| panic!("missing drawn text {expected:?}; got {:?}", drawn_text(output)))
}

/// A job of an artifact in the given status (the host's snake_case name).
fn job_of(artifact_id: &str, status: &str) -> Job {
    serde_json::from_value(json!({
        "id": format!("job-{artifact_id}"),
        "attempt_id": "attempt-1",
        "artifact_id": artifact_id,
        "base_revision": 1,
        "status": status,
        "allowed_intents": [],
        "model": "mock-model",
        "effort": "high",
        "error": null,
    }))
    .expect("a job in the host's shape")
}

/// Regression (backlog desktop 29): the job's label was the request box's text now, so a job of another client, or one sent
/// before the user typed on, showed whatever the box held. The label is the request the window started the job with, and
/// the general label for a job it did not start.
#[test]
fn the_job_label_is_the_request_the_job_was_started_with_not_the_text_in_the_request_box() {
    let mut h = Harness::new();
    theme::install(&h.ctx);
    let _ = h.open_document("canvas", 1);
    h.app.ai_model = "mock-model".into();
    h.app.invoke_ui("ai.request", json!({ "prompt": "레이아웃 정리" }));
    let requests = h.sent();
    assert!(requests.iter().any(|request| request.1 == "job.start"), "the request starts a job: {:?}", requests.iter().map(|r| &r.1).collect::<Vec<_>>());
    let started = serde_json::to_value(Job { id: "job-mine".into(), ..job_of("a1", "running") }).expect("a job");
    h.answer(&requests, "job.start", Ok(started));
    h.app.ai_prompt = "새로 입력한 문장".into();
    let draw = |h: &mut Harness| {
        let ctx = h.ctx.clone();
        let output = ctx.run(screen(1.0, 1000.0, 800.0), |ctx| {
            draw_in_sidebar(ctx, "test-ai-sidebar", theme::SIDEBAR_WIDTH, &mut h.app);
        });
        drawn_text(&output)
    };
    let shown = draw(&mut h);
    assert!(shown.iter().any(|text| text == "레이아웃 정리"), "the job's own request is its label: {shown:?}");

    // A job another client started: the window does not know its request.
    h.app.job = Some(Job { id: "job-other".into(), ..job_of("a1", "running") });
    let shown = draw(&mut h);
    assert!(shown.iter().any(|text| text == super::super::sidebar::JOB_LABEL), "the general label: {shown:?}");
    assert!(!shown.iter().any(|text| text == "레이아웃 정리"), "not another job's request: {shown:?}");
}

fn find_raised_segment_label(output: &egui::FullOutput, palette: &theme::Palette, candidates: &[&str]) -> Option<String> {
    let mut raised = Vec::new();
    let mut texts = Vec::new();
    fn walk(shape: &Shape, raised_fill: egui::Color32, raised: &mut Vec<Rect>, texts: &mut Vec<(Pos2, String)>) {
        match shape {
            Shape::Rect(rect) if rect.fill == raised_fill => raised.push(rect.rect),
            Shape::Text(text) => texts.push((text.pos + text.galley.size() / 2.0, text.galley.text().to_owned())),
            Shape::Vec(shapes) => shapes.iter().for_each(|shape| walk(shape, raised_fill, raised, texts)),
            _ => {}
        }
    }
    for clipped in &output.shapes {
        walk(&clipped.shape, palette.surface, &mut raised, &mut texts);
    }
    // The panel behind the segments is surface-coloured too, so the raised segment is the candidate inside the smallest one.
    texts
        .into_iter()
        .filter(|(_, label)| candidates.contains(&label.as_str()))
        .filter_map(|(center, label)| {
            let smallest = raised.iter().filter(|rect| rect.contains(center)).map(|rect| rect.width() * rect.height()).reduce(f32::min)?;
            Some((smallest, label))
        })
        .min_by(|a, b| a.0.total_cmp(&b.0))
        .map(|(_, label)| label)
}

#[test]
fn d10_2_intent_segmented_control_expresses_all_three_modes_and_never_both_false() {
    let mut harness = AiTabHarness::new(ArtifactKind::Canvas);
    assert!(harness.app.allow_content && harness.app.allow_design && !harness.app.allow_data);
    assert_eq!(harness.app.ai_intents(), [Intent::Content, Intent::Design].into_iter().collect());

    let palette = theme::palette(&harness.ctx);
    let labels = ["내용+디자인", "디자인만", "내용만"];

    let initial = harness.draw_ai_tab(screen(1.0, 1000.0, 800.0));
    assert_eq!(find_raised_segment_label(&initial, &palette, &labels).as_deref(), Some("내용+디자인"));

    // Click "디자인만" -> design only (allow_content=false, allow_design=true).
    let design_pos = find_text_center(&initial, "디자인만");
    let after_design = harness.click_then_settle(design_pos);
    assert!(!harness.app.allow_content && harness.app.allow_design);
    assert_eq!(harness.app.ai_intents(), [Intent::Design].into_iter().collect());
    assert_eq!(find_raised_segment_label(&after_design, &palette, &labels).as_deref(), Some("디자인만"));

    // Clicking "디자인만" again must never turn both false.
    let after_repeat = harness.click_then_settle(design_pos);
    assert!(!harness.app.allow_content && harness.app.allow_design);
    assert_eq!(find_raised_segment_label(&after_repeat, &palette, &labels).as_deref(), Some("디자인만"));

    // Click "내용만" -> content only (allow_content=true, allow_design=false).
    let content_pos = find_text_center(&after_repeat, "내용만");
    let after_content = harness.click_then_settle(content_pos);
    assert!(harness.app.allow_content && !harness.app.allow_design);
    assert_eq!(harness.app.ai_intents(), [Intent::Content].into_iter().collect());
    assert_eq!(find_raised_segment_label(&after_content, &palette, &labels).as_deref(), Some("내용만"));

    // Click "내용+디자인" -> both true again.
    let both_pos = find_text_center(&after_content, "내용+디자인");
    let after_both = harness.click_then_settle(both_pos);
    assert!(harness.app.allow_content && harness.app.allow_design);
    assert_eq!(harness.app.ai_intents(), [Intent::Content, Intent::Design].into_iter().collect());
    assert_eq!(find_raised_segment_label(&after_both, &palette, &labels).as_deref(), Some("내용+디자인"));
}

#[test]
fn d10_2_direct_toggle_intent_commands_never_leave_both_content_and_design_false() {
    let mut harness = AiTabHarness::new(ArtifactKind::Canvas);
    harness.app.ai_prompt = "제목 수정".into();
    harness.app.ai_model = "mock-model".into();

    // From (true, true), toggling "design" flips to content-only (true, false).
    harness.app.invoke_ui("ai.toggle_intent", json!({ "intent": "design" }));
    assert!(harness.app.allow_content && !harness.app.allow_design);

    // Trying to toggle "content" off while "design" is already false must keep allow_content true.
    harness.app.invoke_ui("ai.toggle_intent", json!({ "intent": "content" }));
    assert!(harness.app.allow_content && !harness.app.allow_design);
    assert!(harness.app.can_start_ai_job());

    // Re-enable design, then toggle content off -> design-only (false, true).
    harness.app.invoke_ui("ai.toggle_intent", json!({ "intent": "design" }));
    assert!(harness.app.allow_content && harness.app.allow_design);
    harness.app.invoke_ui("ai.toggle_intent", json!({ "intent": "content" }));
    assert!(!harness.app.allow_content && harness.app.allow_design);

    // Trying to toggle "design" off while "content" is already false must keep allow_design true.
    harness.app.invoke_ui("ai.toggle_intent", json!({ "intent": "design" }));
    assert!(!harness.app.allow_content && harness.app.allow_design);
    assert!(harness.app.can_start_ai_job());
}

#[test]
fn r11_editor_ai_tab_runner_dropdown_opens_and_dispatches_home_runner() {
    let mut harness = AiTabHarness::new(ArtifactKind::Canvas);
    let first = harness.draw_ai_tab(screen(1.0, 1000.0, 800.0));
    let trigger = find_text_center(&first, "Claude · 모델 선택 · 높음");

    // Click the dropdown to open the runner/model/effort popover.
    let opened = harness.click_then_settle(trigger);
    let popup_texts = drawn_text(&opened);
    for expected in ["Claude", "Codex", "낮음", "보통", "높음", "최대"] {
        assert!(popup_texts.iter().any(|text| text == expected), "missing {expected:?} in {popup_texts:?}");
    }

    // Choose Codex in the open popup.
    let codex_pos = find_text_center(&opened, "Codex");
    let _ = harness.click_then_settle(codex_pos);
    assert_eq!(harness.app.ai_runtime, "codex");

    // Reopen and choose "최대" effort.
    let after_codex = harness.draw_ai_tab(screen(1.0, 1000.0, 800.0));
    let reopened = harness.click_then_settle(find_text_center(&after_codex, "Codex · 모델 선택 · 높음"));
    let max_pos = find_text_center(&reopened, "최대");
    let _ = harness.click_then_settle(max_pos);
    assert_eq!(harness.app.ai_effort, "max");

    // Update model via home.runner and verify the dropdown label + job.start payload reflect it.
    harness.app.invoke_ui("home.runner", json!({ "model": "gpt-5.4" }));
    assert_eq!(harness.app.ai_model, "gpt-5.4");
    let updated = harness.draw_ai_tab(screen(1.0, 1000.0, 800.0));
    assert!(drawn_text(&updated).iter().any(|text| text == "Codex · gpt-5.4 · 최대"));

    harness.sent();
    harness.app.ai_prompt = "레이아웃 정리".into();
    harness.app.invoke_ui("ai.request", json!({ "prompt": "레이아웃 정리" }));
    let requests = harness.sent();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].1, "job.start");
    assert_eq!(requests[0].2["runtime"], json!("codex"));
    assert_eq!(requests[0].2["model"], json!("gpt-5.4"));
    assert_eq!(requests[0].2["effort"], json!("max"));
}

/// The editor's runner names the runner in the home row's words, and its model field is a draft as on the home screen: a
/// keystroke saves nothing, and the typed model is saved once when the menu closes.
#[test]
fn the_editor_runner_shows_the_home_words_and_saves_the_typed_model_once_when_the_menu_closes() {
    let mut harness = AiTabHarness::new(ArtifactKind::Canvas);
    assert_eq!(harness.app.runner_labels(), ("Claude", "모델 선택 · 높음".to_owned()));
    type_runner_model(&mut harness, "gpt");
    // A click outside closes the menu; the draft is saved then.
    let _ = harness.click_then_settle(Pos2::new(900.0, 700.0));
    assert_eq!(harness.app.ai_model, "gpt");
    let shown = harness.draw_ai_tab(screen(1.0, 1000.0, 800.0));
    assert!(drawn_text(&shown).iter().any(|text| text == "Claude · gpt · 높음"), "{:?}", drawn_text(&shown));
}

/// Opens the editor's runner menu and types `model` into its model field, one key per frame. No keystroke saves anything.
fn type_runner_model(harness: &mut AiTabHarness, model: &str) {
    let first = harness.draw_ai_tab(screen(1.0, 1000.0, 800.0));
    let opened = harness.click_then_settle(find_text_center(&first, "Claude · 모델 선택 · 높음"));
    let _ = harness.click_then_settle(find_text_center(&opened, "직접 입력…"));
    for key in model.chars() {
        let mut input = screen(1.0, 1000.0, 800.0);
        input.events = vec![Event::Text(key.to_string())];
        let _ = harness.draw_ai_tab(input);
        assert_eq!(harness.app.ai_model, "", "a keystroke ({key}) saves nothing");
    }
}

/// A frame of the window that draws no runner menu (another tab or screen): only the start of the frame runs.
fn frame_without_the_menu(harness: &mut AiTabHarness, input: RawInput) {
    let ctx = harness.ctx.clone();
    let _ = ctx.run(input, |ctx| harness.app.settle_runner_drafts(ctx));
}

/// A typed model whose open menu stops being drawn (a tab or screen change) is saved at the start of a later frame, not lost.
#[test]
fn a_typed_model_is_saved_when_its_open_menu_stops_being_drawn() {
    let mut harness = AiTabHarness::new(ArtifactKind::Canvas);
    type_runner_model(&mut harness, "gpt");
    frame_without_the_menu(&mut harness, screen(1.0, 1000.0, 800.0));
    frame_without_the_menu(&mut harness, screen(1.0, 1000.0, 800.0));
    assert_eq!(harness.app.ai_model, "gpt");
}

/// A typed model is saved when the window is asked to close, even while its menu is still drawn.
#[test]
fn a_typed_model_is_saved_when_the_window_is_asked_to_close() {
    let mut harness = AiTabHarness::new(ArtifactKind::Canvas);
    type_runner_model(&mut harness, "gpt");
    let mut input = screen(1.0, 1000.0, 800.0);
    input.viewports.get_mut(&egui::ViewportId::ROOT).expect("root viewport").events.push(egui::ViewportEvent::Close);
    frame_without_the_menu(&mut harness, input);
    assert_eq!(harness.app.ai_model, "gpt");
}

/// A typed model whose menu was drawn in the last frame is saved at the app's exit too (`on_exit`): the frame that closes the
/// window may carry the keystroke after that frame's start had saved the drafts.
#[test]
fn a_typed_model_is_saved_when_the_app_exits() {
    let mut harness = AiTabHarness::new(ArtifactKind::Canvas);
    type_runner_model(&mut harness, "gpt");
    harness.app.exit_ctx = Some(harness.ctx.clone());
    eframe::App::on_exit(&mut harness.app, None);
    assert_eq!(harness.app.ai_model, "gpt");
}

#[test]
fn r07_9_figure_export_offers_svg_png_pdf_pptx_and_excludes_html() {
    let keys: Vec<&str> = export_formats(ArtifactKind::Figure).iter().map(|(key, _, _)| *key).collect();
    assert_eq!(keys, ["svg", "png", "pdf", "pptx"]);

    let mut harness = AiTabHarness::new(ArtifactKind::Figure);
    harness.app.export_format = "html".into();

    for format in ["svg", "png", "pdf", "pptx"] {
        assert!(harness.app.export_format_supported(format), "{format} must be supported for Figure");
    }
    assert!(!harness.app.export_format_supported("html"), "html must not be supported for Figure");

    let pptx_request = harness
        .app
        .export_request("C:/out/figure.pptx".into(), Some(&json!({ "format": "pptx" })))
        .expect("Figure PPTX export request should succeed");
    assert_eq!(pptx_request.options.format, "pptx");
    // Regression: the floor was Objects, which a raster image panel can never reach (the engine lowers it to Vector), so the
    // export failed with editability_below_required and no allowed loss could pass it.
    assert_eq!(pptx_request.options.required_editability, Editability::Vector);
    assert!(
        harness.app.export_request("C:/out/figure.html".into(), Some(&json!({ "format": "html" }))).is_none(),
        "Figure HTML export request must be rejected"
    );

    let menu = harness.draw_export_menu(screen(1.0, 1000.0, 800.0));
    let labels = drawn_text(&menu);
    for expected in ["SVG", "PNG", "PDF", "PPTX"] {
        assert!(labels.iter().any(|text| text == expected), "missing {expected:?} in Figure export menu: {labels:?}");
    }
    assert!(!labels.iter().any(|text| text == "HTML"), "HTML must not appear in Figure export menu: {labels:?}");
    assert_eq!(harness.app.export_format, "svg", "unsupported prior format resets to first Figure format");
}

fn figure_harness_with_request() -> AiTabHarness {
    let mut harness = AiTabHarness::new(ArtifactKind::Figure);
    harness.app.ai_prompt = "범례 정리".into();
    harness.app.ai_model = "mock-model".into();
    harness
}

#[test]
fn figure_job_running_disables_the_send_control_and_refuses_every_request_path() {
    let mut harness = figure_harness_with_request();
    harness.app.job = Some(job_of("doc-1", "running"));

    let shown = harness.draw_ai_tab(screen(1.0, 1000.0, 800.0));
    let reason = harness.app.figure_job_wait().expect("a running job of this figure holds the next request");
    assert!(drawn_text(&shown).iter().any(|text| text == reason), "the reason is drawn: {:?}", drawn_text(&shown));

    // A click on the request button sends nothing and is not refused either: the control is disabled.
    let idle = harness.click_then_settle(find_last_text_center(&shown, "요청"));
    assert!(drawn_text(&idle).iter().any(|text| text == reason));
    assert!(harness.sent().iter().all(|request| request.1 != "job.start"), "a disabled control starts no job");
    assert_ne!(harness.app.notices.current(Instant::now()).map(|notice| notice.severity), Some(Severity::Error));

    // The shortcut and the retry refuse with the same words, and nothing is sent.
    assert_eq!(harness.app.invoke("ai.request", json!({ "prompt": "범례 정리" })), Err(reason.into()));
    assert_eq!(harness.app.invoke("ai.retry", json!({})), Err(reason.into()));
    assert!(harness.sent().iter().all(|request| request.1 != "job.start"));

    // When the job has ended, the control shows no reason and the request starts.
    harness.app.job = Some(job_of("doc-1", "completed"));
    let ended = harness.draw_ai_tab(screen(1.0, 1000.0, 800.0));
    assert!(harness.app.figure_job_wait().is_none());
    assert!(!drawn_text(&ended).iter().any(|text| text == reason));
    let _ = harness.click_then_settle(find_last_text_center(&ended, "요청"));
    assert!(harness.sent().iter().any(|request| request.1 == "job.start"), "a finished job lets the next request start");
}

#[test]
fn figure_job_kept_aside_blocks_only_its_own_figure() {
    let mut harness = figure_harness_with_request();
    harness.app.parked_jobs = vec![job_of("other-figure", "running")];
    assert!(harness.app.figure_job_wait().is_none(), "a running job of another document does not block this figure");
    harness.app.parked_jobs = vec![job_of("doc-1", "running")];
    assert!(harness.app.figure_job_wait().is_some(), "a kept-aside job of this figure blocks it");
}

#[test]
fn figure_job_start_not_yet_answered_blocks_a_second_start() {
    let mut harness = figure_harness_with_request();
    assert!(harness.app.call("job.start", json!({ "artifact_id": "doc-1" }), PendingAction::StartJob).is_some());
    assert!(harness.app.figure_job_wait().is_some(), "a start the host has not answered is a job on its way");
    assert!(harness.app.invoke("ai.request", json!({ "prompt": "범례 정리" })).is_err());
    assert_eq!(harness.sent().iter().filter(|request| request.1 == "job.start").count(), 1, "only the first start is sent");
}

#[test]
fn figure_job_running_stops_an_attachment_request_before_its_imports() {
    let mut harness = figure_harness_with_request();
    // The repository's README is an evidence file (.md): the attach check reads it and nothing is changed.
    let evidence = PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../README.md"));
    harness.app.add_figure_ai_attachment(evidence).expect("a Markdown evidence file attaches");

    // Without a job the request starts by importing the file.
    harness.app.start_figure_ai_request();
    assert!(harness.sent().iter().any(|request| request.1 == "asset.import"));
    harness.app.cancel_figure_ai_request().expect("the import can be cancelled before any job");

    // With a job of this figure running, the request starts no import and no job.
    harness.app.job = Some(job_of("doc-1", "running"));
    harness.app.start_figure_ai_request();
    assert!(harness.sent().iter().all(|request| request.1 != "asset.import" && request.1 != "job.start"));
    assert!(harness.app.figure_ai_request.is_none());
    assert_eq!(harness.app.notices.current(Instant::now()).map(|notice| notice.severity), Some(Severity::Error));
}

#[test]
fn a_figure_pptx_export_asks_the_engine_for_no_more_than_vector() {
    // The request the window sends for a Figure's PPTX carries the Vector floor (`export_request`).
    let mut harness = AiTabHarness::new(ArtifactKind::Figure);
    assert!(harness.app.invoke("file.export", json!({ "format": "pptx", "target_path": "C:/out/figure.pptx" })).is_ok());
    let sent = harness.sent();
    let export = sent.iter().find(|request| request.1 == "artifact.export").expect("the export is sent");
    assert_eq!(export.2["options"]["required_editability"], serde_json::to_value(Editability::Vector).unwrap());
}

#[test]
fn a_canvas_request_control_says_why_it_waits_for_its_running_job() {
    // Regression: the control of a document that is not a Figure stayed on, and a click did nothing without a word.
    let mut harness = AiTabHarness::new(ArtifactKind::Canvas);
    harness.app.ai_prompt = "제목 수정".into();
    harness.app.ai_model = "mock-model".into();
    harness.app.job = Some(job_of("doc-1", "running"));
    let reason = "이 작업물의 AI 작업이 진행 중입니다. 끝나면 다시 요청할 수 있습니다";
    assert_eq!(harness.app.ai_request_block("제목 수정"), Some(reason));

    let shown = harness.draw_ai_tab(screen(1.0, 1000.0, 800.0));
    assert!(drawn_text(&shown).iter().any(|text| text == reason), "the reason is drawn: {:?}", drawn_text(&shown));
    let _ = harness.click_then_settle(find_last_text_center(&shown, "요청"));
    assert!(harness.sent().iter().all(|request| request.1 != "job.start"), "a disabled control starts no job");
    assert_eq!(harness.app.invoke("ai.request", json!({ "prompt": "제목 수정" })), Err(reason.into()));
    assert_eq!(harness.app.invoke("ai.retry", json!({})), Err(reason.into()));
}

#[test]
fn a_request_without_a_model_or_a_prompt_is_refused_with_its_reason() {
    let mut harness = AiTabHarness::new(ArtifactKind::Canvas);
    harness.app.ai_prompt = "제목 수정".into();
    harness.app.ai_model.clear();
    assert_eq!(harness.app.ai_request_block("제목 수정"), Some("실행기 메뉴에서 모델을 고르세요"));
    assert!(harness.app.invoke("ai.request", json!({ "prompt": "제목 수정" })).is_err(), "no model, no request");
    assert!(harness.sent().is_empty());

    harness.app.ai_model = "mock-model".into();
    assert_eq!(harness.app.ai_request_block(" "), Some("요청 내용을 적어 주세요"));
    assert!(harness.app.invoke("ai.request", json!({ "prompt": " " })).is_err(), "an empty prompt is refused");
    assert!(harness.sent().is_empty());
}

#[test]
fn the_runner_menu_stays_shut_while_a_job_runs_and_the_runner_is_kept() {
    // Regression: the runner could be changed while its job ran, and a model could be emptied under the running job.
    let mut harness = figure_harness_with_request();
    harness.app.job = Some(job_of("doc-1", "running"));
    let shown = harness.draw_ai_tab(screen(1.0, 1000.0, 800.0));
    // The runner label is cut to the field's width ("…"), as the window draws it at this width.
    let trigger = find_text_center(&shown, "Claude · mock-model · …");
    let opened = harness.click_then_settle(trigger);
    assert!(!drawn_text(&opened).iter().any(|text| text == "Codex"), "the runner menu does not open while the job runs");
    assert_eq!(harness.app.invoke("home.runner", json!({ "runtime": "codex" })), Err("AI 작업이 진행 중이라 실행기를 바꿀 수 없습니다".into()));
    assert_eq!(harness.app.ai_runtime, "claude", "the runner is unchanged");
}

#[test]
fn an_intent_mode_names_the_whole_set_and_an_agent_data_intent_is_not_hidden() {
    let mut harness = AiTabHarness::new(ArtifactKind::Canvas);
    let palette = theme::palette(&harness.ctx);
    let labels = ["내용+디자인", "디자인만", "내용만"];
    harness.app.invoke_ui("ai.toggle_intent", json!({ "intent": "data" }));
    assert!(harness.app.allow_data);
    assert!(harness.app.ai_intents().contains(&Intent::Data));
    let _ = harness.draw_ai_tab(screen(1.0, 1000.0, 800.0));

    harness.app.invoke_ui("ai.toggle_intent", json!({ "mode": "design_only" }));
    assert!(!harness.app.allow_data, "a segment turns the data intent off");
    assert_eq!(harness.app.ai_intents(), [Intent::Design].into_iter().collect());
    let after = harness.draw_ai_tab(screen(1.0, 1000.0, 800.0));
    assert_eq!(find_raised_segment_label(&after, &palette, &labels).as_deref(), Some("디자인만"));
}

#[test]
fn an_unknown_or_null_intent_mode_is_refused_and_a_mode_alone_is_enough() {
    let mut harness = AiTabHarness::new(ArtifactKind::Canvas);
    assert!(harness.app.invoke("ai.toggle_intent", json!({ "mode": "all" })).is_err());
    assert!(harness.app.invoke("ai.toggle_intent", json!({ "mode": null })).is_err());
    assert!(harness.app.invoke("ai.toggle_intent", json!({})).is_err());
    assert!(harness.app.allow_content && harness.app.allow_design && !harness.app.allow_data, "refused values change nothing");

    assert!(harness.app.invoke("ai.toggle_intent", json!({ "mode": "content_only" })).is_ok());
    assert!(harness.app.allow_content && !harness.app.allow_design);
}

/// The PPTX writer the product's figure engine uses, as a stand-in that claims the most it could: Objects.
fn pptx_stand_in(_scene: &RenderScene, _assets: &dyn AssetStore, title: &str) -> DltResult<EncodedOutput> {
    Ok(EncodedOutput {
        bytes: format!("PPTX {title}").into_bytes(),
        mime: "application/vnd.openxmlformats-officedocument.presentationml.presentation".into(),
        editability: Editability::Objects,
        diagnostics: Vec::new(),
    })
}

/// One call to the workspace, as the host makes it: its result, or its error.
fn host_call(workspace: &mut designllm_core::Workspace, method: &str, params: serde_json::Value) -> Result<serde_json::Value, DltError> {
    let response = workspace.dispatch(RpcRequest { protocol_version: PROTOCOL_VERSION, id: new_id(), method: method.into(), params });
    match (response.result, response.error) {
        (_, Some(error)) => Err(error),
        (Some(result), None) => Ok(result),
        (None, None) => Ok(serde_json::Value::Null),
    }
}

#[test]
fn a_figure_with_an_image_panel_exports_to_pptx_through_the_real_workspace() {
    // Regression: the window asked for Objects, the engine can give Vector to a figure with a raster panel, and the core refused
    // the export (editability_below_required) for good. Here the window's own request goes to the real workspace.
    let root = std::env::temp_dir().join(format!("designllm-pptx-{}", new_id()));
    std::fs::create_dir_all(&root).expect("a folder for the workspace");
    let fonts = Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().nth(2).expect("the repository root").join("assets/fonts");
    let renderer = Arc::new(designllm_render::Renderer::new(&fonts).expect("bundled fonts"));
    let engines: Vec<Arc<dyn EngineAdapter>> = vec![Arc::new(FigureEngine::default().with_pptx_exporter(pptx_stand_in))];
    let mut workspace = designllm_core::Workspace::open(&root.join("workspace"), engines, renderer).expect("the workspace opens");
    let workspace_id = workspace.id().to_owned();

    let summary = host_call(
        &mut workspace,
        "artifact.create",
        json!({ "request_id": new_id(), "actor": "desktop", "kind": "figure", "title": "그림", "size": null, "template": null }),
    )
    .expect("the figure is created");
    let summary: ArtifactSummary = serde_json::from_value(summary).expect("a summary");

    // A raster panel of 4 by 4 pixels, as the window adds a picture.
    let picture = root.join("picture.png");
    image::RgbaImage::new(4, 4).save(&picture).expect("the picture is written");
    let asset = host_call(&mut workspace, "asset.import", json!({ "path": picture.to_string_lossy() })).expect("the picture is imported");
    let artifact: Artifact = serde_json::from_value(
        host_call(&mut workspace, "artifact.inspect", json!({ "artifact_id": summary.id })).expect("the figure is read"),
    )
    .expect("an artifact");
    let ArtifactBody::Figure(document) = &artifact.body else { panic!("the figure is a figure") };
    let panel = FigurePanel {
        id: new_id(),
        bounds: Default::default(),
        label: panel_label(document.panels.len()),
        body: PanelBody::Image(FigureImage::new(serde_json::from_value(asset).expect("the asset"), "experimental_original", 4, 4)),
        style: Default::default(),
    };
    let change = ChangeSet {
        request_id: new_id(),
        actor: "desktop".into(),
        workspace_id,
        artifact_id: summary.id.clone(),
        base_revision: artifact.revision,
        allowed_intents: [Intent::Content, Intent::Design].into_iter().collect(),
        job_ref: None,
        read_dependencies: Vec::new(),
        operations: panel_additions(document, panel),
    };
    host_call(&mut workspace, "changes.apply", serde_json::to_value(change).expect("a change set")).expect("the panel is added");
    let revision = artifact.revision + 1;

    // The window's own export request for the figure at that revision.
    let mut harness = AiTabHarness::new(ArtifactKind::Figure);
    harness.app.active = Some(ArtifactSummary { id: summary.id.clone(), revision, ..summary });
    let target = root.join("figure.pptx");
    assert!(harness.app.invoke("file.export", json!({ "format": "pptx", "target_path": target.to_string_lossy() })).is_ok());
    let sent = harness.sent();
    let export = sent.iter().find(|request| request.1 == "artifact.export").expect("the window sends the export");
    let result = host_call(&mut workspace, "artifact.export", export.2.clone());
    assert!(result.is_ok(), "the figure with a picture exports to PPTX: {result:?}");
    assert!(target.is_file(), "the PPTX is written");
}

#[test]
fn a_content_or_design_intent_turns_off_while_the_data_intent_is_on() {
    // Regression: the design and content toggles did nothing while the data intent was on and the other intent was on too.
    let mut harness = AiTabHarness::new(ArtifactKind::Figure);
    harness.app.ai_prompt = "범례 정리".into();
    harness.app.ai_model = "mock-model".into();
    harness.app.invoke_ui("ai.toggle_intent", json!({ "mode": "design_only" }));
    harness.app.invoke_ui("ai.toggle_intent", json!({ "intent": "data" }));
    assert!(!harness.app.allow_content && harness.app.allow_design && harness.app.allow_data);
    harness.app.invoke_ui("ai.toggle_intent", json!({ "intent": "design" }));
    assert!(!harness.app.allow_design && harness.app.allow_data, "design goes off and data stays on");

    harness.app.invoke_ui("ai.toggle_intent", json!({ "mode": "content_only" }));
    harness.app.invoke_ui("ai.toggle_intent", json!({ "intent": "data" }));
    assert!(harness.app.allow_content && !harness.app.allow_design && harness.app.allow_data);
    harness.app.invoke_ui("ai.toggle_intent", json!({ "intent": "content" }));
    assert!(!harness.app.allow_content && harness.app.allow_data, "content goes off and data stays on");
    assert!(harness.app.can_start_ai_job(), "one intent stays on, so a request may start");
}

#[test]
fn the_home_runner_changes_on_the_home_screen_while_a_job_of_the_editor_runs() {
    // Regression: the runner was refused on the home screen too while a job of the open document ran.
    let mut harness = figure_harness_with_request();
    harness.app.job = Some(job_of("doc-1", "running"));
    harness.app.screen = Screen::Home;
    assert_eq!(harness.app.invoke("home.runner", json!({ "runtime": "codex" })), Ok(()));
    assert_eq!(harness.app.ai_runtime, "codex");

    harness.app.screen = Screen::Editor;
    assert_eq!(harness.app.invoke("home.runner", json!({ "runtime": "claude" })), Err("AI 작업이 진행 중이라 실행기를 바꿀 수 없습니다".into()));
    assert_eq!(harness.app.ai_runtime, "codex", "the editor keeps its runner while the job runs");
}

#[test]
fn a_job_start_answered_after_the_window_left_its_document_leaves_the_open_one_as_it_was() {
    // A04: the answer of a request that the window has left is kept for its own document; the open one does not change.
    let mut host = Host::start();
    let first = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
    let second = host.create("canvas", "다른 작업물");
    let second_before = content_of(&host.artifact(&second));
    let mut window = Window::open(&mut host, &first);
    window.app.ai_model = "mock-model".into();

    // The request goes out, and the window moves to the other document (`nav.open_project`) before the host answers it.
    window.app.invoke("ai.request", json!({ "prompt": "정리" })).expect("the request is sent");
    window.step(&mut host, "nav.open_project", json!({ "artifact_id": second })).expect("the other document opens");

    assert_eq!(window.app.active.as_ref().map(|active| active.id.as_str()), Some(second.as_str()));
    assert!(window.app.screen_is_editor());
    assert!(window.app.job.is_none(), "the job of the document left is not shown in the one that is open");
    assert_eq!(window.app.parked_jobs.len(), 1, "the job is kept for its own document");
    assert_eq!(window.app.parked_jobs[0].artifact_id, first);
    assert_eq!(content_of(&host.artifact(&second)), second_before, "the open document keeps its content");

    // The job is cancelled on the host; the poll that follows brings no job back into the window.
    let job = window.app.parked_jobs[0].clone();
    host.call("job.cancel", json!({ "job_id": job.id, "attempt_id": job.attempt_id })).expect("the job is cancelled");
    window.app.request_parked_job_status();
    window.settle(&mut host);
    assert!(window.app.parked_jobs.is_empty(), "a cancelled job is not kept");
    assert!(window.app.job.is_none(), "a cancelled job is not shown");
    assert_eq!(window.app.active.as_ref().map(|active| active.id.as_str()), Some(second.as_str()));
}

/// The text the AI tab draws for the window's open document, in one frame.
fn drawn_ai_tab(app: &mut DesktopApp) -> Vec<String> {
    let ctx = Context::default();
    theme::install(&ctx);
    let output = ctx.run(screen(1.0, 1000.0, 800.0), |ctx| {
        draw_in_sidebar(ctx, "test-ai-sidebar-of-window", theme::SIDEBAR_WIDTH, app);
    });
    drawn_text(&output)
}

#[test]
fn the_ai_tab_names_the_scope_of_the_request() {
    // The request carries the whole artifact, or the selected targets (`ai_target_ids`), and the tab says which.
    let mut host = Host::start();
    let canvas = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
    let mut window = Window::open(&mut host, &canvas);
    let shown = drawn_ai_tab(&mut window.app);
    assert!(shown.iter().any(|text| text == "작업물 전체"), "nothing is selected: {shown:?}");

    window.step(&mut host, "select.many", json!({ "node_ids": ["a", "c"] })).expect("two boxes are selected");
    let shown = drawn_ai_tab(&mut window.app);
    assert!(shown.iter().any(|text| text == "선택한 항목 2개"), "two boxes are selected: {shown:?}");
    assert!(!shown.iter().any(|text| text == "작업물 전체"), "a selection that targets the request is named as such: {shown:?}");
}

#[test]
fn a_job_that_failed_on_a_change_outside_its_targets_shows_why() {
    let mut host = Host::start();
    let canvas = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
    let mut window = Window::open(&mut host, &canvas);
    window.app.ai_model = "mock-model".into();
    window.step(&mut host, "select.node", json!({ "node_id": "a" })).expect("the box is selected");
    window.step(&mut host, "ai.request", json!({ "prompt": "첫 상자를 고쳐 주세요" })).expect("the request is sent");
    let job = window.app.job.clone().expect("the job is shown");

    // The job's change to a box outside its target is refused by the host; the runtime then reports the job as failed with that refusal.
    let revision = host.artifact(&canvas).revision;
    let operations = vec![Operation::SetText { node_id: "c".into(), text: "고침".into() }];
    let mut change = change_set_arguments(&host.workspace_id, &canvas, revision, &[Intent::Content], operations);
    change["job_ref"] = json!({ "job_id": job.id, "attempt_id": job.attempt_id });
    let refusal = host.call("changes.apply", change).expect_err("a change outside the target is refused");
    host.set_job_status_for_test(&job.id, &job.attempt_id, JobStatus::Failed, Some(refusal.clone())).expect("the job is failed");

    // The window reads the job's state; the tab shows the reason in the words the window uses for errors.
    window.app.request_job_status();
    window.settle(&mut host);
    assert_eq!(window.app.job.as_ref().map(|job| job.status), Some(JobStatus::Failed));
    let shown = drawn_ai_tab(&mut window.app);
    let reason = crate::app::user_error(&refusal);
    assert!(shown.iter().any(|text| *text == reason), "the reason is shown: {shown:?}");
}

/// The window reads the changes that came from elsewhere, as its frames do: the events are polled and the document is reloaded.
fn read_changes_from_elsewhere(backend: &Arc<HostBackend>, window: &mut Window) {
    window.app.last_event_poll = Instant::now() - Duration::from_secs(3);
    window.app.poll_events();
    backend.with(|host| window.settle(host));
}

/// An agent removes the text box `node` of `canvas` over MCP (`changes_apply`). The window has not read that change yet.
fn agent_removes_box_unread(backend: &Arc<HostBackend>, canvas: &str, node: &str) {
    let mcp = Mcp::connect(backend.clone());
    let (workspace, revision) = backend.with(|host| (host.workspace_id.clone(), host.artifact(canvas).revision));
    let operations = vec![Operation::RemoveNode { node_id: node.into() }];
    mcp.call("changes_apply", change_set_arguments(&workspace, canvas, revision, &[Intent::Content], operations));
}

/// An agent removes the text box `node` of `canvas` over MCP (`changes_apply`), and the window reads that change.
fn agent_removes_box(backend: &Arc<HostBackend>, window: &mut Window, canvas: &str, node: &str) {
    agent_removes_box_unread(backend, canvas, node);
    read_changes_from_elsewhere(backend, window);
}

#[test]
fn a_selection_an_agent_took_away_refuses_the_request_until_the_user_chooses_again() {
    // The agent deletes the box the user selected: the request is refused, the AI tab says why, and no job starts. Choosing another
    // box ends the refusal, and the request then names that box.
    let mut host = Host::start();
    let canvas = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
    let mut window = Window::open(&mut host, &canvas);
    window.app.ai_model = "mock-model".into();
    window.step(&mut host, "select.node", json!({ "node_id": "a" })).expect("the box is selected");

    let backend = HostBackend::new(host);
    agent_removes_box(&backend, &mut window, &canvas, "a");
    assert!(window.app.selected_nodes.is_empty(), "the selection went with its box");
    assert_eq!(window.app.ai_selection_refusal(), Some(SELECTION_GONE), "the agent took the selection away");
    let shown = drawn_ai_tab(&mut window.app);
    assert!(shown.iter().any(|text| text == SELECTION_GONE), "the tab says why: {shown:?}");

    assert_eq!(window.app.invoke("ai.request", json!({ "prompt": "정리" })), Err(SELECTION_GONE.into()));
    backend.with(|host| window.settle(host));
    assert!(window.app.job.is_none(), "a refused request starts no job");

    backend.with(|host| window.step(host, "select.node", json!({ "node_id": "b" }))).expect("the user chooses another box");
    assert_eq!(window.app.ai_selection_refusal(), None, "a selection the user made ends the refusal");
    backend.with(|host| window.step(host, "ai.request", json!({ "prompt": "정리" }))).expect("the request is sent");
    let job = window.app.job.clone().expect("the job is shown");
    assert_eq!(job.target_ids, ["b"], "the request names the box the user chose");
}

#[test]
fn a_selection_the_user_clears_after_an_agent_took_it_away_sends_the_whole_artifact() {
    let mut host = Host::start();
    let canvas = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
    let mut window = Window::open(&mut host, &canvas);
    window.app.ai_model = "mock-model".into();
    window.step(&mut host, "select.node", json!({ "node_id": "a" })).expect("the box is selected");

    let backend = HostBackend::new(host);
    agent_removes_box(&backend, &mut window, &canvas, "a");
    backend.with(|host| window.step(host, "select.clear", json!({}))).expect("the user clears the selection");
    assert_eq!(window.app.ai_selection_refusal(), None, "a selection the user cleared is the user's choice");
    backend.with(|host| window.step(host, "ai.request", json!({ "prompt": "정리" }))).expect("the request is sent");
    let job = window.app.job.clone().expect("the job is shown");
    assert!(job.target_ids.is_empty(), "nothing is selected: the request covers the whole artifact");
}

#[test]
fn a_box_the_user_deletes_does_not_refuse_the_request() {
    // The window's own deletion is the user's choice, not an agent's: the request is not refused. `edit.delete` clears the selection
    // before the change (set_selection), so the reload has no selection to restore and reports nothing.
    let mut host = Host::start();
    let canvas = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
    let mut window = Window::open(&mut host, &canvas);
    window.app.ai_model = "mock-model".into();
    window.step(&mut host, "select.node", json!({ "node_id": "a" })).expect("the box is selected");

    let backend = HostBackend::new(host);
    backend.with(|host| window.step(host, "edit.delete", json!({}))).expect("the user deletes the box");
    read_changes_from_elsewhere(&backend, &mut window);
    assert_eq!(window.app.ai_selection_refusal(), None, "the user's own deletion is not an agent's");
    backend.with(|host| window.step(host, "ai.request", json!({ "prompt": "정리" }))).expect("the request is sent");
    let job = window.app.job.clone().expect("the job is shown");
    assert!(job.target_ids.is_empty(), "nothing is selected: the whole artifact");
}

#[test]
fn an_agent_removal_stays_refused_when_the_desktop_commits_again_before_the_window_reads_it() {
    // The agent removes the selected box (revision R), and the desktop commits again (R+1) before the window has read the agent's
    // change. The window's one read covers both commits, and the refusal does not depend on which commit it reads last.
    let mut host = Host::start();
    let canvas = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
    let mut window = Window::open(&mut host, &canvas);
    window.app.ai_model = "mock-model".into();
    window.step(&mut host, "select.node", json!({ "node_id": "a" })).expect("the box is selected");

    let backend = HostBackend::new(host);
    agent_removes_box_unread(&backend, &canvas, "a");
    backend
        .with(|host| host.apply(&canvas, vec![Operation::SetText { node_id: "c".into(), text: "고침".into() }], [Intent::Content].into_iter().collect()))
        .expect("the desktop commits again");
    read_changes_from_elsewhere(&backend, &mut window);

    assert_eq!(window.app.ai_selection_refusal(), Some(SELECTION_GONE), "the box the agent removed is still what the selection names");
    assert_eq!(window.app.invoke("ai.request", json!({ "prompt": "정리" })), Err(SELECTION_GONE.into()));
    backend.with(|host| window.settle(host));
    assert!(window.app.job.is_none(), "a refused request starts no job");
}

#[test]
fn an_agent_removal_stays_refused_when_the_window_reloads_before_it_polls_the_events() {
    // A resync the host asks for reloads the window before its event poll delivers the agent's change, so the reload alone has to
    // keep the refusal. The poll that follows does not end it.
    let mut host = Host::start();
    let canvas = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
    let mut window = Window::open(&mut host, &canvas);
    window.app.ai_model = "mock-model".into();
    window.step(&mut host, "select.node", json!({ "node_id": "a" })).expect("the box is selected");

    let backend = HostBackend::new(host);
    agent_removes_box_unread(&backend, &canvas, "a");
    backend.with(|host| {
        window.app.handle_events(json!({ "events": [], "resync_required": true }));
        window.settle(host);
    });
    assert_eq!(window.app.ai_selection_refusal(), Some(SELECTION_GONE), "the reload found the box gone");
    read_changes_from_elsewhere(&backend, &mut window);
    assert_eq!(window.app.ai_selection_refusal(), Some(SELECTION_GONE), "the later poll does not end the refusal");
    assert_eq!(window.app.invoke("ai.request", json!({ "prompt": "정리" })), Err(SELECTION_GONE.into()));
    backend.with(|host| window.settle(host));
    assert!(window.app.job.is_none(), "a refused request starts no job");
}

#[test]
fn a_selection_that_loses_one_box_to_an_agent_is_refused_even_though_another_box_stays() {
    // The request must not go on naming only the box that stayed: the user has to choose again.
    let mut host = Host::start();
    let canvas = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
    let mut window = Window::open(&mut host, &canvas);
    window.app.ai_model = "mock-model".into();
    window.step(&mut host, "select.many", json!({ "node_ids": ["a", "c"] })).expect("two boxes are selected");

    let backend = HostBackend::new(host);
    agent_removes_box(&backend, &mut window, &canvas, "a");
    assert_eq!(window.app.ai_selection_refusal(), Some(SELECTION_GONE), "one of the two boxes is gone");
    assert_eq!(window.app.invoke("ai.request", json!({ "prompt": "정리" })), Err(SELECTION_GONE.into()));
    backend.with(|host| window.settle(host));
    assert!(window.app.job.is_none(), "a refused request starts no job");
}

#[test]
fn a_selection_whose_boxes_all_stay_is_not_refused_after_an_edit_and_the_request_names_both() {
    // A reload keeps one element of a multi-selection on the window's screen. The request still names both boxes the user chose.
    let mut host = Host::start();
    let canvas = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
    let mut window = Window::open(&mut host, &canvas);
    window.app.ai_model = "mock-model".into();
    window.step(&mut host, "select.many", json!({ "node_ids": ["a", "c"] })).expect("two boxes are selected");

    window.app.change(vec![Operation::SetText { node_id: "b".into(), text: "고침".into() }], &[Intent::Content]).expect("the edit is sent");
    window.settle(&mut host);
    assert_eq!(window.app.ai_selection_refusal(), None, "the boxes the selection names are still there");
    window.step(&mut host, "ai.request", json!({ "prompt": "정리" })).expect("the request is sent");
    let job = window.app.job.clone().expect("the job is shown");
    assert_eq!(job.target_ids, ["a", "c"], "the request names the boxes the user chose, not only the one the screen keeps");
}

#[test]
fn a_request_sent_before_the_window_reads_an_agent_removal_is_refused_by_the_host() {
    // The window reads only the context after the agent's removal, so its document still has the box and its own check passes.
    // The request names the box, and the host checks the targets again: the job does not start.
    let mut host = Host::start();
    let canvas = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
    let mut window = Window::open(&mut host, &canvas);
    window.app.ai_model = "mock-model".into();
    window.step(&mut host, "select.node", json!({ "node_id": "a" })).expect("the box is selected");

    let backend = HostBackend::new(host);
    agent_removes_box_unread(&backend, &canvas, "a");
    backend.with(|host| {
        window.app.fetch_context();
        window.settle(host);
    });
    assert_eq!(window.app.ai_selection_refusal(), None, "the window's document still has the box");
    backend.with(|host| window.step(host, "ai.request", json!({ "prompt": "정리" }))).expect("the request is sent");
    assert_eq!(window.app.notices.current(Instant::now()).map(|notice| notice.severity), Some(Severity::Error), "the host answered with a refusal");
    assert!(window.app.job.is_none(), "the host refused the job: the box it names is gone");
}

#[test]
fn a_pair_that_lost_one_box_to_an_agent_stays_refused_through_a_later_reload() {
    // The first reload keeps the other box selected on the window's screen. A commit and a second reload re-apply that selection
    // again. The choice the user made still names the removed box, so the request stays refused.
    let mut host = Host::start();
    let canvas = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
    let mut window = Window::open(&mut host, &canvas);
    window.app.ai_model = "mock-model".into();
    window.step(&mut host, "select.many", json!({ "node_ids": ["a", "c"] })).expect("two boxes are selected");

    let backend = HostBackend::new(host);
    agent_removes_box_unread(&backend, &canvas, "a");
    read_changes_from_elsewhere(&backend, &mut window);
    assert_eq!(window.app.ai_selection_refusal(), Some(SELECTION_GONE), "the first reload");
    backend
        .with(|host| host.apply(&canvas, vec![Operation::SetText { node_id: "b".into(), text: "고침".into() }], [Intent::Content].into_iter().collect()))
        .expect("the desktop commits again");
    read_changes_from_elsewhere(&backend, &mut window);
    assert_eq!(window.app.ai_selection_refusal(), Some(SELECTION_GONE), "the second reload does not end the refusal");
    assert!(drawn_ai_tab(&mut window.app).iter().any(|text| text == SELECTION_GONE), "the tab says why");
    assert_eq!(window.app.invoke("ai.request", json!({ "prompt": "정리" })), Err(SELECTION_GONE.into()));
    backend.with(|host| window.settle(host));
    assert!(window.app.job.is_none(), "a refused request starts no job");
}

#[test]
fn the_ai_tab_names_the_refusal_where_it_would_name_the_whole_artifact() {
    // The selection names an item the document does not hold: the scope the tab shows is the refusal, and the request is refused.
    let mut host = Host::start();
    let canvas = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
    let mut window = Window::open(&mut host, &canvas);
    let (view, artifact) = (window.app.view_id.clone(), canvas.clone());
    window.app.ui.user_selection = Some((view, artifact, vec!["gone".into()]));
    let shown = drawn_ai_tab(&mut window.app);
    assert!(shown.iter().any(|text| text == SELECTION_GONE), "the refusal is the scope: {shown:?}");
    assert!(!shown.iter().any(|text| text == "작업물 전체"), "a refused request is not the whole artifact: {shown:?}");
    assert_eq!(window.app.ai_request_block("정리"), Some(SELECTION_GONE));
}

/// The ids of the top-level nodes of the first page of the canvas `canvas`.
fn canvas_node_ids(host: &mut Host, canvas: &str) -> Vec<String> {
    match &host.artifact(canvas).body {
        ArtifactBody::Canvas(document) => document.pages[0].nodes.iter().map(|node| node.id.clone()).collect(),
        _ => panic!("a canvas"),
    }
}

/// What the window keeps as the user's choice, the items an AI request would name, and the scope the AI tab shows for them.
fn user_choice_of(window: &Window) -> (Option<(String, String, Vec<String>)>, Vec<String>, String) {
    (window.app.ui.user_selection.clone(), window.app.ai_target_ids(), window.app.ai_scope_label())
}

#[test]
fn an_agent_run_moves_the_screen_selection_and_leaves_the_users_choice() {
    // The user chose box `a`. The agent selects, clears, deletes and adds through the UI channel: the screen follows the agent, the
    // choice does not, and the user's next request still names `a`.
    let mut host = Host::start();
    let canvas = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
    let mut window = Window::open(&mut host, &canvas);
    window.app.ai_model = "mock-model".into();
    window.step(&mut host, "select.node", json!({ "node_id": "a" })).expect("the user chooses the box");
    let before = user_choice_of(&window);
    assert_eq!(before.1, ["a"], "the user's choice names the box");

    let run = window.agent_run(&mut host, "select.node", json!({ "node_id": "b" }));
    assert_eq!(run["status"], "completed", "{run}");
    assert_eq!(window.app.selected_nodes, ["b"], "the screen follows the agent");
    assert_eq!(user_choice_of(&window), before, "the agent's selection is not the user's choice");

    let run = window.agent_run(&mut host, "select.clear", json!({}));
    assert_eq!(run["status"], "completed", "{run}");
    assert!(window.app.selected_nodes.is_empty(), "the screen follows the agent");
    assert_eq!(user_choice_of(&window), before, "a selection the agent cleared is not the user's choice");

    let run = window.agent_run(&mut host, "select.node", json!({ "node_id": "c" }));
    assert_eq!(run["status"], "completed", "{run}");
    let run = window.agent_run(&mut host, "edit.delete", json!({}));
    assert_eq!(run["status"], "completed", "{run}");
    assert!(!canvas_node_ids(&mut host, &canvas).contains(&"c".to_owned()), "the agent deleted box c");
    assert_eq!(user_choice_of(&window), before, "a deletion of the agent's selection leaves the choice");

    let run = window.agent_run(&mut host, "canvas.table.add", json!({ "rows": 2, "columns": 2 }));
    assert_eq!(run["status"], "completed", "{run}");
    let table = canvas_node_ids(&mut host, &canvas).into_iter().find(|id| !matches!(id.as_str(), "a" | "b")).expect("the table is in the canvas");
    assert_eq!(window.app.selected_nodes, [table], "the screen moves to the table the agent added");
    assert_eq!(user_choice_of(&window), before, "a table the agent adds is not the user's choice");

    window.step(&mut host, "ai.request", json!({ "prompt": "정리" })).expect("the request is sent");
    let job = window.app.job.clone().expect("the job is shown");
    assert_eq!(job.target_ids, ["a"], "the request still names the box the user chose");
}

#[test]
fn an_agent_that_deletes_the_box_the_user_chose_refuses_the_users_next_request() {
    let mut host = Host::start();
    let canvas = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
    let mut window = Window::open(&mut host, &canvas);
    window.app.ai_model = "mock-model".into();
    window.step(&mut host, "select.node", json!({ "node_id": "a" })).expect("the user chooses the box");

    let run = window.agent_run(&mut host, "select.node", json!({ "node_id": "a" }));
    assert_eq!(run["status"], "completed", "{run}");
    let run = window.agent_run(&mut host, "edit.delete", json!({}));
    assert_eq!(run["status"], "completed", "{run}");
    assert!(!canvas_node_ids(&mut host, &canvas).contains(&"a".to_owned()), "the agent deleted the box the user chose");
    assert_eq!(window.app.ai_selection_refusal(), Some(SELECTION_GONE), "the choice names a box that is gone");
    assert_eq!(window.app.invoke("ai.request", json!({ "prompt": "정리" })), Err(SELECTION_GONE.into()));
    window.settle(&mut host);
    assert!(window.app.job.is_none(), "a refused request starts no job");
}

#[test]
fn a_selection_the_user_makes_after_an_agent_run_is_the_users_choice() {
    let mut host = Host::start();
    let canvas = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
    let mut window = Window::open(&mut host, &canvas);
    window.app.ai_model = "mock-model".into();
    let run = window.agent_run(&mut host, "select.node", json!({ "node_id": "b" }));
    assert_eq!(run["status"], "completed", "{run}");

    window.step(&mut host, "select.many", json!({ "node_ids": ["a", "c"] })).expect("the user chooses two boxes");
    assert_eq!(window.app.ai_target_ids(), ["a", "c"], "the user's choice is recorded");
    window.step(&mut host, "ai.request", json!({ "prompt": "정리" })).expect("the request is sent");
    let job = window.app.job.clone().expect("the job is shown");
    assert_eq!(job.target_ids, ["a", "c"], "the request names the boxes the user chose");
}

#[test]
fn a_link_answer_does_not_take_back_a_selection_the_user_changed_while_it_was_on_its_way() {
    // The link change goes to box `a`, which the user chose. The user picks `b` before the host answers the change, and the answer
    // must not select `a` again. The answers come in the order given here, so the host's own selection checks do not interfere.
    let mut host = Host::start();
    let canvas = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
    let mut artifact = host.artifact(&canvas);
    artifact.id = "a1".into();
    artifact.revision = 1;
    let mut harness = Harness::new();
    harness.open_document("canvas", 1);
    harness.app.artifact = Some(artifact);
    harness.app.invoke("select.node", json!({ "node_id": "a" })).expect("the user chooses the box");
    harness.app.invoke("canvas.open_url", json!({ "url": "https://example.test/" })).expect("the link change is sent");
    harness.app.invoke("select.node", json!({ "node_id": "b" })).expect("the user picks another box");
    let sent = harness.sent();
    harness.answer(&sent, "changes.apply", Ok(json!({ "revision": 2 })));

    assert!(harness.app.select_after_commit.is_none(), "the answer selects nothing: the user's selection is newer");
    assert_eq!(harness.app.selected_nodes, ["b"], "the selection the user made stays on the screen");
    assert_eq!(harness.app.ai_target_ids(), ["b"], "and it is the user's choice");
}

#[test]
fn the_restoring_mark_goes_back_even_when_the_change_under_it_panics() {
    let mut host = Host::start();
    let canvas = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
    let mut window = Window::open(&mut host, &canvas);
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let restored = crate::app::events::NotUserChoice::enter(&mut window.app, true);
        assert!(restored.ui.restoring_selection, "the change runs as the window's own");
        panic!("the change fails");
    }));
    assert!(outcome.is_err(), "the panic reached the test");
    assert!(!window.app.ui.restoring_selection, "the mark goes back when the change panics");
}

#[test]
fn the_users_choice_is_dropped_when_the_window_shows_another_document() {
    let mut host = Host::start();
    let first = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
    let second = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
    let mut window = Window::open(&mut host, &first);
    window.step(&mut host, "select.node", json!({ "node_id": "a" })).expect("the user chooses a box");
    assert!(window.app.ui.user_selection.is_some(), "the choice is kept on its document");

    window.step(&mut host, "nav.open_project", json!({ "artifact_id": second })).expect("the other document opens");
    window.app.publish_ui_if_due();
    assert!(window.app.ui.user_selection.is_none(), "a choice made on the first document is dropped");
    assert!(window.app.ai_target_ids().is_empty(), "the other document has no choice");
}

/// The texts the AI tab draws in one frame at sidebar width `width` with their rects (a cut text keeps its whole text and
/// has its cut size), the panel's column, and the frame's house controls (`ui_record`).
fn ai_tab_texts(app: &mut DesktopApp, width: f32) -> (Vec<(String, Rect)>, Rect, Vec<crate::ui_record::Control>) {
    fn walk(shape: &Shape, found: &mut Vec<(String, Rect)>) {
        match shape {
            Shape::Text(text) => found.push((text.galley.text().to_owned(), Rect::from_min_size(text.pos, text.galley.size()))),
            Shape::Vec(shapes) => shapes.iter().for_each(|shape| walk(shape, found)),
            _ => {}
        }
    }
    let ctx = Context::default();
    theme::install(&ctx);
    let mut column = Rect::NOTHING;
    let mut controls = Vec::new();
    let output = ctx.run(screen(1.0, 1000.0, 800.0), |ctx| {
        crate::ui_record::begin();
        column = draw_in_sidebar(ctx, "test-ai-tab-column", width, app);
        controls = crate::ui_record::finish();
    });
    let mut found = Vec::new();
    for clipped in &output.shapes {
        walk(&clipped.shape, &mut found);
    }
    (found, column, controls)
}

fn text_rect(texts: &[(String, Rect)], text: &str) -> Rect {
    texts.iter().find(|(drawn, _)| drawn == text).unwrap_or_else(|| panic!("{text:?} is drawn: {texts:?}")).1
}

#[test]
fn the_ai_tab_starts_at_the_layers_tab_margin() {
    // The first element of the tab starts `TREE_PADDING` in from the sidebar's column, as the layers tree does.
    let mut host = Host::start();
    let canvas = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
    let mut window = Window::open(&mut host, &canvas);
    let (texts, column, _) = ai_tab_texts(&mut window.app, theme::SIDEBAR_WIDTH);
    let scope = text_rect(&texts, "작업물 전체");
    assert_eq!(scope.left() - column.left(), theme::TREE_PADDING, "the AI tab's left margin");
}

/// A window whose job was started by `prompt`, with `client` as the job's runner.
fn window_with_job(host: &mut Host, prompt: &str, client: &str) -> Window {
    let canvas = canvas_with_three_boxes(host, [0, 1_000_000, 3_000_000]);
    let mut window = Window::open(host, &canvas);
    window.app.ai_model = "mock-model".into();
    window.step(host, "ai.request", json!({ "prompt": prompt })).expect("the request is sent");
    let mut job = window.app.job.clone().expect("the job is shown");
    job.client_name = Some(client.into());
    window.app.job = Some(job);
    window
}

#[test]
fn the_job_title_and_its_runner_name_are_drawn_apart() {
    // The title and the runner's name are two texts with a gap between them, not one word. The sidebar draws its tab
    // strip with zero item spacing, and the tab is drawn under it.
    let mut host = Host::start();
    let mut window = window_with_job(&mut host, "모바일 앱 로그인 화면", "claude-code");
    let (texts, _, _) = ai_tab_texts(&mut window.app, theme::SIDEBAR_WIDTH);
    let title = text_rect(&texts, "모바일 앱 로그인 화면");
    let runner = text_rect(&texts, "claude-code");
    let gap = runner.left() - title.right();
    assert!(gap >= 5.0, "the gap between the title and the runner's name: {gap}");
}

#[test]
fn a_long_job_title_is_cut_before_the_runner_name_at_the_default_and_least_width() {
    // A long request is cut to the room the runner's name leaves; the name stays inside the tab's column.
    let prompt = "모바일 앱 로그인 화면과 회원가입 화면, 비밀번호 찾기 화면까지 한 번에 정리해 주세요";
    let mut host = Host::start();
    let mut window = window_with_job(&mut host, prompt, "claude-code");
    for width in [theme::SIDEBAR_WIDTH, theme::SIDEBAR_MIN] {
        let (texts, column, _) = ai_tab_texts(&mut window.app, width);
        let inner_right = column.right() - theme::TREE_PADDING;
        let title = text_rect(&texts, prompt);
        let runner = text_rect(&texts, "claude-code");
        assert!(title.right() < runner.left(), "at {width}: the title ends before the runner's name, {title:?} {runner:?}");
        assert!(runner.right() <= inner_right + 0.5, "at {width}: the runner's name {runner:?} is inside {column:?}");
    }
}

#[test]
fn the_undo_icon_stays_in_the_column_at_the_default_and_least_width() {
    // The undo row names its actor (a job by the first 8 characters of its id) cut before the icon; the icon stays inside
    // the tab's column, also for a long actor.
    let job_actor = "job:fac9ff74-450c-44b6-bad9-1d9758840efe:agent";
    // An actor of no known form is shown as it is, cut.
    let long_actor = format!("outside:{}{}{}", new_id(), new_id(), new_id());
    let mut host = Host::start();
    let canvas = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
    let mut window = Window::open(&mut host, &canvas);
    for actor in [job_actor, long_actor.as_str()] {
        window.app.agent_batches.push(crate::app::state::AgentBatch { request_id: new_id(), actor: actor.into() });
    }
    for width in [theme::SIDEBAR_WIDTH, theme::SIDEBAR_MIN] {
        let (texts, column, controls) = ai_tab_texts(&mut window.app, width);
        let inner_right = column.right() - theme::TREE_PADDING;
        let icons: Vec<Rect> = controls.iter().filter(|control| control.label == "되돌리기").map(|control| control.rect).collect();
        assert_eq!(icons.len(), 2, "at {width}: one undo icon per batch");
        for icon in &icons {
            assert!(icon.left() >= column.left() && icon.right() <= inner_right + 0.5, "at {width}: the undo icon {icon:?} is inside {column:?}");
        }
        // Newest first: the long actor's row, then the job's.
        let name = text_rect(&texts, "AI 작업 fac9ff74");
        assert!(name.right() <= icons[1].left(), "at {width}: the job's name {name:?} ends before its icon {:?}", icons[1]);
        assert!(!texts.iter().any(|(text, _)| text == job_actor), "at {width}: the row does not show the raw actor");
    }
}

#[test]
fn the_undo_row_names_agent_actors() {
    use super::agent_actor_name;
    assert_eq!(agent_actor_name("job:fac9ff74-450c-44b6-bad9-1d9758840efe:agent"), "AI 작업 fac9ff74");
    assert_eq!(agent_actor_name("job:j1:desktop"), "AI 작업 j1");
    assert_eq!(agent_actor_name("mcp:0123456789abcdef:agent"), "에이전트 01234567");
    assert_eq!(agent_actor_name("cli:agent"), "명령줄");
    assert_eq!(agent_actor_name("someone"), "someone");
}
