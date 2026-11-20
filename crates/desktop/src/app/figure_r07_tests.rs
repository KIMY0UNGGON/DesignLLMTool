//! The R07 figure commands (`figure_r07.rs`): each command sends the operation it names, with the values the figure holds now as
//! `expected`; input the engine would refuse, or a change that changes nothing, sends nothing; a stale approval shows its words.
//! The window is played with a detached RPC worker. No window opens.

use std::sync::mpsc::{Receiver, Sender};

use designllm_contracts::{
    Artifact, ArtifactBody, ArtifactKind, ArtifactSummary, AssetRef, CommonLegend, CommonLegendItem, CommonLegendPlacement, ContextSnapshot,
    Diagnostic, Diagram, Edge, Evidence, FigureDocument, FigureImage, FigurePanel, Node, NodeKind, PanelBody, PixelRect, Rect, Size, Style,
    VisualReview, VisualReviewState,
};
use serde_json::{json, Value};
use std::collections::BTreeMap;

use super::{
    figure_r07::{draft_id, millidegrees, review_status_message, CalibrationDraft, LegendDraft, ProvenanceDraft, ReviewDraft, TransformDraft},
    messages::diagnostic_message,
    test_support::screen,
    DesktopApp, Screen,
};
use crate::{
    commands,
    rpc::{RpcReply, RpcWorker, Work},
    theme,
};
use eframe::egui;

const FIGURE: &str = "figure-1";
const REVISION: u64 = 7;
const DIAGRAM: &str = "diagram";
const IMAGE: &str = "image";
const ASSET: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const STALE_WORDS: &str = "검토 후 그림의 내용·자료·디자인이 바뀌었습니다 — 다시 검토 필요";

struct Harness {
    app: DesktopApp,
    work: Receiver<Work>,
    // Kept alive: the window reads replies from the worker, which needs this end open.
    _replies: Sender<RpcReply>,
}

impl Harness {
    /// An open figure at `REVISION` with a diagram and the given image panel, with `selected` selected.
    fn new(image: FigurePanel, selected: Option<&str>) -> Self {
        Self::with_figure(figure(image), selected)
    }

    fn with_figure(figure: FigureDocument, selected: Option<&str>) -> Self {
        let (rpc, replies, work) = RpcWorker::detached();
        let mut app = DesktopApp::new(rpc, "그림".into(), None, None);
        while work.try_recv().is_ok() {}
        let mut artifact = Artifact::new(ArtifactKind::Figure, "그림", "figure", ArtifactBody::Figure(figure));
        artifact.id = FIGURE.into();
        artifact.revision = REVISION;
        app.screen = Screen::Editor;
        app.active = Some(ArtifactSummary {
            id: FIGURE.into(),
            kind: ArtifactKind::Figure,
            title: "그림".into(),
            revision: REVISION,
            engine_id: "figure".into(),
            dirty: false,
            modified_at_ms: None,
            active_jobs: Vec::new(),
        });
        app.context = Some(context(REVISION, &app.view_id));
        app.artifact = Some(artifact);
        app.selected_node = selected.map(str::to_owned);
        Self { app, work, _replies: replies }
    }

    /// The `changes.apply` parameters sent since the last call, in order.
    fn sent_changes(&mut self) -> Vec<Value> {
        let mut changes = Vec::new();
        while let Ok(work) = self.work.try_recv() {
            if let Work::Call { method, params, .. } = work {
                if method == "changes.apply" {
                    changes.push(params);
                }
            }
        }
        changes
    }
}

fn context(revision: u64, view_id: &str) -> ContextSnapshot {
    serde_json::from_value(json!({
        "workspace_id": "w1",
        "artifact_id": FIGURE,
        "revision": revision,
        "view_id": view_id,
        "selection": { "node_ids": [], "text_range": null },
        "visible_page": 0,
        "selected_properties": { "nodes": null, "stale_selection": null, "render_unavailable": null },
        "capabilities": {},
        "render_snapshot": null,
        "view_transform": null,
    }))
    .expect("a context snapshot")
}

fn evidence(id: &str, text: &str) -> Evidence {
    serde_json::from_value(json!({
        "id": id,
        "source_hash": designllm_contracts::sha256(text.as_bytes()),
        "origin": "user_statement",
        "physical_page_index": null,
        "region": null,
        "pdf_geometry": null,
        "text": text,
        "review_state": "user_provided",
    }))
    .expect("evidence")
}

fn node(id: &str) -> Node {
    Node {
        id: id.into(),
        kind: NodeKind::Rectangle,
        bounds: Rect { x: 0, y: 0, width: 1_000_000, height: 500_000 },
        text: id.into(),
        style: Style::default(),
        asset: None,
        children: Vec::new(),
        layout: None,
        interactions: Vec::new(),
        variants: Default::default(),
        ext: Default::default(),
    }
}

/// A diagram: `node-a` has origin `user_authored` and no evidence; `node-b` has no origin and evidence `ev-1`; `edge-1` is a
/// connector of origin `model_proposal`.
fn diagram_panel() -> FigurePanel {
    let mut node_origins = BTreeMap::new();
    node_origins.insert("node-a".to_owned(), "user_authored".to_owned());
    let mut evidence_ids = BTreeMap::new();
    evidence_ids.insert("node-b".to_owned(), vec!["ev-1".to_owned()]);
    FigurePanel {
        id: DIAGRAM.into(),
        bounds: Rect { x: 0, y: 0, width: 4_000_000, height: 3_000_000 },
        label: "(a)".into(),
        body: PanelBody::Diagram(Diagram {
            nodes: vec![node("node-a"), node("node-b")],
            edges: vec![Edge {
                id: "edge-1".into(),
                from: "node-a".into(),
                to: "node-b".into(),
                label: String::new(),
                evidence_ids: Vec::new(),
                origin: "model_proposal".into(),
                style: Style::default(),
            }],
            evidence_ids,
            node_origins,
        }),
        style: Style::default(),
    }
}

fn image_panel(crop: Option<PixelRect>, rotation: i32, calibration: Option<designllm_contracts::Calibration>, origin: &str) -> FigurePanel {
    FigurePanel {
        id: IMAGE.into(),
        bounds: Rect { x: 4_000_000, y: 0, width: 4_000_000, height: 3_000_000 },
        label: "(b)".into(),
        body: PanelBody::Image(FigureImage {
            crop_pixel_rect: crop,
            rotation_millidegrees: rotation,
            calibration,
            ..FigureImage::new(AssetRef { sha256: ASSET.into(), mime: "image/png".into(), bytes: 1 }, origin, 400, 300)
        }),
        style: Style::default(),
    }
}

fn figure(image: FigurePanel) -> FigureDocument {
    FigureDocument {
        panels: vec![diagram_panel(), image],
        evidence: vec![evidence("ev-1", "세 번 반복 측정했다"), evidence("ev-2", "대조군과 차이가 없었다")],
        ..FigureDocument::new(Size::a4())
    }
}

fn diagnostic(code: &str) -> Diagnostic {
    Diagnostic { code: code.into(), message: String::new(), node_id: None, blocking: false, params: Default::default() }
}

fn calibration_args(length: &str, span: &str, unit: &str, evidence: Option<&str>) -> Value {
    json!({ "panel_id": IMAGE, "calibration": { "length": length, "unit": unit, "pixel_span": span, "evidence_id": evidence } })
}

#[test]
fn provenance_sends_the_operation_with_the_values_the_element_holds_as_expected() {
    let mut h = Harness::new(image_panel(None, 0, None, "experimental_original"), Some("node-b"));
    h.app
        .invoke("figure.diagram_provenance", json!({ "element_id": "node-b", "origin": "paper_evidence", "evidence_ids": ["ev-2"] }))
        .expect("the change is sent");
    let changes = h.sent_changes();
    assert_eq!(changes.len(), 1);
    assert_eq!(
        changes[0]["operations"],
        json!([{
            "op": "set_diagram_provenance",
            "panel_id": DIAGRAM,
            "element_id": "node-b",
            "expected_origin": null,
            "expected_evidence_ids": ["ev-1"],
            "origin": "paper_evidence",
            "evidence_ids": ["ev-2"]
        }])
    );
    assert_eq!(changes[0]["allowed_intents"], json!(["content"]));
}

#[test]
fn a_provenance_the_engine_would_refuse_names_its_reason_and_sends_nothing() {
    let mut h = Harness::new(image_panel(None, 0, None, "experimental_original"), None);
    let refused = [
        ("edge-1", json!(null), json!([])),
        ("node-a", json!("paper_evidence"), json!([])),
        ("node-a", json!("user_authored"), json!(["ev-9"])),
        ("node-a", json!("guess"), json!([])),
        ("missing", json!("user_authored"), json!([])),
    ];
    for (element, origin, evidence_ids) in refused {
        let args = json!({ "element_id": element, "origin": origin, "evidence_ids": evidence_ids });
        assert!(h.app.invoke("figure.diagram_provenance", args).is_err(), "{element} {origin}");
    }
    assert!(h.sent_changes().is_empty());
}

#[test]
fn a_provenance_that_changes_nothing_sends_nothing() {
    let mut h = Harness::new(image_panel(None, 0, None, "experimental_original"), None);
    let refusal = h
        .app
        .invoke("figure.diagram_provenance", json!({ "element_id": "node-a", "origin": "user_authored", "evidence_ids": [] }))
        .expect_err("the node already has this origin");
    assert_eq!(refusal, "바뀐 내용이 없습니다");
    assert!(h.sent_changes().is_empty());
}

#[test]
fn the_common_legend_sends_the_whole_legend_and_a_blank_title_is_no_title() {
    let mut h = Harness::new(image_panel(None, 0, None, "experimental_original"), None);
    h.app
        .invoke(
            "figure.common_legend",
            json!({ "legend": { "items": [{ "label": "대조군", "color": "#1F4E79" }], "placement": "right", "title": "  " } }),
        )
        .expect("the legend is sent");
    let changes = h.sent_changes();
    assert_eq!(
        changes[0]["operations"],
        json!([{
            "op": "set_common_legend",
            "expected": null,
            "legend": { "items": [{ "label": "대조군", "color": "#1F4E79" }], "placement": "right" }
        }])
    );
    assert_eq!(changes[0]["allowed_intents"], json!(["content"]));
}

#[test]
fn removing_the_common_legend_names_the_legend_it_replaces() {
    let mut figure = figure(image_panel(None, 0, None, "experimental_original"));
    figure.common_legend = Some(CommonLegend {
        items: vec![CommonLegendItem { label: "대조군".into(), color: "#1F4E79".into() }],
        placement: CommonLegendPlacement::Bottom,
        title: None,
    });
    let mut h = Harness::with_figure(figure, None);
    h.app.invoke("figure.common_legend", json!({ "legend": null })).expect("the legend is removed");
    let changes = h.sent_changes();
    assert_eq!(
        changes[0]["operations"],
        json!([{
            "op": "set_common_legend",
            "expected": { "items": [{ "label": "대조군", "color": "#1F4E79" }], "placement": "bottom" },
            "legend": null
        }])
    );
}

#[test]
fn an_invalid_common_legend_sends_nothing() {
    let mut h = Harness::new(image_panel(None, 0, None, "experimental_original"), None);
    let invalid = [
        json!({ "items": [{ "label": "  ", "color": "#1F4E79" }] }),
        json!({ "items": [{ "label": "대조군", "color": "1F4E79" }] }),
        json!({ "items": [{ "label": "대조군", "color": "#GGGGGG" }] }),
        json!({ "items": [{ "label": "대조군", "color": "#1F4E7" }] }),
        json!({ "items": [] }),
    ];
    for legend in invalid {
        assert!(h.app.invoke("figure.common_legend", json!({ "legend": legend })).is_err(), "{legend}");
    }
    assert!(h.sent_changes().is_empty());
}

#[test]
fn the_visual_review_sends_its_state_and_note_and_never_a_digest() {
    let mut h = Harness::new(image_panel(None, 0, None, "experimental_original"), None);
    h.app.invoke("figure.visual_review", json!({ "review": { "state": "approved", "note": "  " } })).expect("the review is sent");
    h.app.invoke("figure.visual_review", json!({ "review": { "state": "needs_revision", "note": "축 단위" } })).expect("the review is sent");
    let changes = h.sent_changes();
    assert_eq!(changes.len(), 2);
    assert_eq!(
        changes[0]["operations"],
        json!([{ "op": "set_figure_visual_review", "expected": null, "review": { "state": "approved" } }])
    );
    // Nothing the host answers is applied in this harness, so the second change names the review the figure still holds.
    assert_eq!(
        changes[1]["operations"],
        json!([{
            "op": "set_figure_visual_review",
            "expected": null,
            "review": { "state": "needs_revision", "note": "축 단위" }
        }])
    );
}

#[test]
fn a_stale_approval_says_it_must_be_reviewed_again_and_only_an_approval_can_be_stale() {
    let approved = VisualReview { state: VisualReviewState::Approved, note: None, approved_digest: Some("digest".into()) };
    let stale = [diagnostic("VISUAL_REVIEW_STALE")];
    assert_eq!(review_status_message(Some(&approved), &stale), Some(STALE_WORDS));
    assert_eq!(review_status_message(Some(&approved), &[]), None, "a current approval shows no words");
    let pending = VisualReview { state: VisualReviewState::Pending, ..approved.clone() };
    assert_eq!(review_status_message(Some(&pending), &stale), None, "only an approval can be stale");
    assert_eq!(diagnostic_message("VISUAL_REVIEW_STALE"), Some(STALE_WORDS), "the same words where the code is refused");
}

#[test]
fn the_image_transform_sends_the_crop_and_rotation_with_the_current_values_as_expected() {
    let current = Some(PixelRect { x: 0, y: 0, width: 200, height: 100 });
    let mut h = Harness::new(image_panel(current, 1_000, None, "experimental_original"), Some(IMAGE));
    h.app
        .invoke("figure.image_transform", json!({ "panel_id": IMAGE, "crop": null, "rotation_millidegrees": 0 }))
        .expect("the crop is removed");
    h.app
        .invoke("figure.image_transform", json!({ "panel_id": IMAGE, "crop": { "x": 10, "y": 20, "width": 100, "height": 50 }, "rotation_millidegrees": -90000 }))
        .expect("the crop and rotation are sent");
    let changes = h.sent_changes();
    assert_eq!(changes.len(), 2);
    assert_eq!(
        changes[0]["operations"],
        json!([{
            "op": "set_figure_image_transform",
            "panel_id": IMAGE,
            "expected_asset_sha256": ASSET,
            "expected_crop_pixel_rect": { "x": 0, "y": 0, "width": 200, "height": 100 },
            "expected_rotation_millidegrees": 1000,
            "crop_pixel_rect": null,
            "rotation_millidegrees": 0
        }])
    );
    assert_eq!(
        changes[1]["operations"],
        json!([{
            "op": "set_figure_image_transform",
            "panel_id": IMAGE,
            "expected_asset_sha256": ASSET,
            "expected_crop_pixel_rect": { "x": 0, "y": 0, "width": 200, "height": 100 },
            "expected_rotation_millidegrees": 1000,
            "crop_pixel_rect": { "x": 10, "y": 20, "width": 100, "height": 50 },
            "rotation_millidegrees": -90000
        }])
    );
    assert_eq!(changes[0]["allowed_intents"], json!(["design"]));
}

#[test]
fn a_crop_outside_the_image_an_empty_crop_or_a_full_turn_sends_nothing() {
    let mut h = Harness::new(image_panel(None, 0, None, "experimental_original"), Some(IMAGE));
    let refused = [
        json!({ "panel_id": IMAGE, "crop": { "x": 390, "y": 0, "width": 20, "height": 10 }, "rotation_millidegrees": 0 }),
        json!({ "panel_id": IMAGE, "crop": { "x": 0, "y": 280, "width": 10, "height": 21 }, "rotation_millidegrees": 0 }),
        json!({ "panel_id": IMAGE, "crop": { "x": 0, "y": 0, "width": 0, "height": 10 }, "rotation_millidegrees": 0 }),
        json!({ "panel_id": IMAGE, "crop": null, "rotation_millidegrees": 360000 }),
        json!({ "panel_id": IMAGE, "crop": null, "rotation_millidegrees": 0 }),
    ];
    for args in refused {
        assert!(h.app.invoke("figure.image_transform", args.clone()).is_err(), "{args}");
    }
    assert!(h.sent_changes().is_empty());
}

#[test]
fn a_typed_rotation_is_millidegrees_within_the_engine_bound() {
    assert_eq!(millidegrees("12.5"), Ok(12_500));
    assert_eq!(millidegrees("-0.5"), Ok(-500));
    assert_eq!(millidegrees("+3"), Ok(3_000));
    assert_eq!(millidegrees("359.999"), Ok(359_999));
    assert_eq!(millidegrees("-359.999"), Ok(-359_999));
    for text in ["360", "360.001", "1.2345", "", "abc", "1,5", "--1"] {
        assert!(millidegrees(text).is_err(), "{text:?}");
    }
}

#[test]
fn the_calibration_sends_its_length_span_unit_and_evidence_as_data() {
    let mut h = Harness::new(image_panel(None, 0, None, "experimental_original"), Some(IMAGE));
    h.app.invoke("figure.image_calibration", calibration_args(" 10.0 ", "250", "mm", Some("ev-2"))).expect("the calibration is sent");
    let changes = h.sent_changes();
    assert_eq!(
        changes[0]["operations"],
        json!([{
            "op": "set_figure_image_calibration",
            "panel_id": IMAGE,
            "expected_asset_sha256": ASSET,
            "expected_calibration": null,
            "calibration": { "length": "10.0", "unit": "mm", "pixel_span": "250", "evidence_id": "ev-2" }
        }])
    );
    assert_eq!(changes[0]["allowed_intents"], json!(["data"]));
}

#[test]
fn an_invalid_calibration_sends_nothing() {
    let mut generated = Harness::new(image_panel(None, 0, None, "generated_illustration"), Some(IMAGE));
    assert!(generated.app.invoke("figure.image_calibration", calibration_args("10", "250", "mm", None)).is_err());
    assert!(generated.sent_changes().is_empty(), "a generated illustration takes no calibration");

    let mut h = Harness::new(image_panel(None, 0, None, "experimental_original"), Some(IMAGE));
    let refused = [
        calibration_args("0", "250", "mm", None),
        calibration_args("0.0", "250", "mm", None),
        calibration_args("-5", "250", "mm", None),
        calibration_args("5e3", "250", "mm", None),
        calibration_args("10", "250", "  ", None),
        calibration_args("10", "250", "mm", Some("ev-9")),
    ];
    for args in refused {
        assert!(h.app.invoke("figure.image_calibration", args.clone()).is_err(), "{args}");
    }
    assert!(h.sent_changes().is_empty());
}

#[test]
fn an_omitted_value_key_is_refused_by_the_command_check_and_sends_nothing() {
    // 49zf: the value keys are required; only an explicit null clears.
    let calibrated = designllm_contracts::Calibration { length: "10".into(), unit: "mm".into(), pixel_span: "250".into(), evidence_id: None };
    let mut h = Harness::new(image_panel(None, 0, Some(calibrated), "experimental_original"), Some(IMAGE));
    let error = h.app.invoke("figure.image_calibration", json!({ "panel_id": IMAGE })).expect_err("no calibration key");
    assert!(error.contains("missing `calibration`"), "{error}");
    assert!(h.app.invoke("figure.image_transform", json!({ "panel_id": IMAGE, "rotation_millidegrees": 0 })).is_err(), "no crop key");
    assert!(h.app.invoke("figure.common_legend", json!({})).is_err(), "no legend key");
    assert!(h.sent_changes().is_empty());
    h.app.invoke("figure.image_calibration", json!({ "panel_id": IMAGE, "calibration": null })).expect("null clears the calibration");
    let changes = h.sent_changes();
    assert_eq!(changes[0]["operations"][0]["calibration"], Value::Null);
    assert_eq!(changes[0]["operations"][0]["expected_calibration"]["length"], json!("10"));
}

#[test]
fn every_figure_r07_command_is_wired_and_declares_the_change_it_sends() {
    for (id, call) in [
        ("figure.diagram_provenance", "changes.apply:set_diagram_provenance"),
        ("figure.common_legend", "changes.apply:set_common_legend"),
        ("figure.visual_review", "changes.apply:set_figure_visual_review"),
        ("figure.image_transform", "changes.apply:set_figure_image_transform"),
        ("figure.image_calibration", "changes.apply:set_figure_image_calibration"),
    ] {
        assert!(commands::available(id), "{id} is wired");
        assert_eq!(commands::core_calls(id), Some(&[call][..]), "{id}");
    }
}

fn draw_r07(ctx: &egui::Context, app: &mut DesktopApp, figure: &FigureDocument) {
    let _ = ctx.run(screen(1.0, 640.0, 480.0), |ctx| {
        egui::CentralPanel::default().show(ctx, |ui| app.r07_draw_figure(ui, figure));
    });
}

fn stored_legend(ctx: &egui::Context, figure_id: &str) -> LegendDraft {
    ctx.data(|data| data.get_temp::<LegendDraft>(draft_id("r07-legend", figure_id, ()))).expect("the legend section keeps a draft")
}

fn stored_review(ctx: &egui::Context, figure_id: &str) -> ReviewDraft {
    ctx.data(|data| data.get_temp::<ReviewDraft>(draft_id("r07-review", figure_id, ()))).expect("the review section keeps a draft")
}

// The legend and the review keep what the user typed but did not apply. Two figures without a legend or a review have equal
// bases, so a draft keyed by the section alone would be shown on the second figure.
#[test]
fn an_unapplied_legend_or_review_stays_with_the_figure_it_was_typed_in() {
    let mut h = Harness::new(image_panel(None, 0, None, "experimental_original"), None);
    let ctx = egui::Context::default();
    theme::install(&ctx);
    let mut legend = LegendDraft::of(None);
    legend.items.push(("대조군".into(), "#1F4E79".into()));
    let mut review = ReviewDraft::of(None);
    review.state = VisualReviewState::Approved;
    ctx.data_mut(|data| {
        data.insert_temp(draft_id("r07-legend", FIGURE, ()), legend);
        data.insert_temp(draft_id("r07-review", FIGURE, ()), review);
    });

    let typed_on = figure(image_panel(None, 0, None, "experimental_original"));
    draw_r07(&ctx, &mut h.app, &typed_on);
    assert_eq!(stored_legend(&ctx, FIGURE).items.len(), 1, "the figure it was typed on keeps the draft");

    // Another figure, with its own artifact id, opens in the window. It has no legend and no review, like the first one.
    let other = "figure-2";
    h.app.artifact.as_mut().expect("a figure is open").id = other.into();
    h.app.active.as_mut().expect("a figure is open").id = other.into();
    let second = figure(image_panel(None, 0, None, "experimental_original"));
    draw_r07(&ctx, &mut h.app, &second);
    assert!(stored_legend(&ctx, other).items.is_empty(), "the other figure shows no legend item typed on the first");
    assert!(stored_review(&ctx, other).state == VisualReviewState::Pending, "the other figure shows no review typed on the first");
}

/// Shows `figure_id` in the window, with `selected` selected, and draws its R07 sections for `figure`.
fn show(ctx: &egui::Context, h: &mut Harness, figure_id: &str, selected: &str, figure: &FigureDocument) {
    h.app.artifact.as_mut().expect("a figure is open").id = figure_id.into();
    h.app.active.as_mut().expect("a figure is open").id = figure_id.into();
    h.app.selected_node = Some(selected.into());
    draw_r07(ctx, &mut h.app, figure);
}

fn stored<T: Clone + Send + Sync + 'static>(ctx: &egui::Context, section: &str, figure_id: &str, part: &str) -> T {
    ctx.data(|data| data.get_temp::<T>(draft_id(section, figure_id, part))).unwrap_or_else(|| panic!("{section} keeps a draft for {figure_id}/{part}"))
}

// The provenance, transform and calibration drafts are keyed by the figure and the element: another figure shows its own, the
// first figure gets its typed values back when it is shown again, and a panel order change keeps them with their panel.
#[test]
fn the_provenance_transform_and_calibration_drafts_stay_with_their_figure_and_element() {
    let mut h = Harness::new(image_panel(None, 0, None, "experimental_original"), Some(IMAGE));
    let ctx = egui::Context::default();
    theme::install(&ctx);
    let first = figure(image_panel(None, 0, None, "experimental_original"));

    show(&ctx, &mut h, FIGURE, IMAGE, &first);
    let mut transform: TransformDraft = stored(&ctx, "r07-transform", FIGURE, IMAGE);
    transform.rotation = "15".into();
    let mut calibration: CalibrationDraft = stored(&ctx, "r07-calibration", FIGURE, IMAGE);
    calibration.length = "10".into();
    show(&ctx, &mut h, FIGURE, "node-a", &first);
    let mut provenance: ProvenanceDraft = stored(&ctx, "r07-provenance", FIGURE, "node-a");
    provenance.origin = Some("paper_evidence".into());
    ctx.data_mut(|data| {
        data.insert_temp(draft_id("r07-transform", FIGURE, IMAGE), transform);
        data.insert_temp(draft_id("r07-calibration", FIGURE, IMAGE), calibration);
        data.insert_temp(draft_id("r07-provenance", FIGURE, "node-a"), provenance);
    });

    // Another figure with the same panel and element ids: its sections start from its own values.
    let other = "figure-2";
    show(&ctx, &mut h, other, IMAGE, &first);
    assert_ne!(stored::<TransformDraft>(&ctx, "r07-transform", other, IMAGE).rotation, "15", "the other figure has no typed rotation");
    assert_ne!(stored::<CalibrationDraft>(&ctx, "r07-calibration", other, IMAGE).length, "10", "the other figure has no typed length");
    show(&ctx, &mut h, other, "node-a", &first);
    assert_eq!(stored::<ProvenanceDraft>(&ctx, "r07-provenance", other, "node-a").origin.as_deref(), Some("user_authored"));

    // The first figure again, with its panels in another order: the typed values are back, with their panel and element.
    let mut reordered = first.clone();
    reordered.panels.reverse();
    show(&ctx, &mut h, FIGURE, IMAGE, &reordered);
    assert_eq!(stored::<TransformDraft>(&ctx, "r07-transform", FIGURE, IMAGE).rotation, "15");
    assert_eq!(stored::<CalibrationDraft>(&ctx, "r07-calibration", FIGURE, IMAGE).length, "10");
    show(&ctx, &mut h, FIGURE, "node-a", &reordered);
    assert_eq!(stored::<ProvenanceDraft>(&ctx, "r07-provenance", FIGURE, "node-a").origin.as_deref(), Some("paper_evidence"));
}
