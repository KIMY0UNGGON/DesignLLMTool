//! The error-key switch of a plot panel (`plot_display_change` and `set_plot_display` in figure_tools.rs): the change names the
//! mode the window shows, goes against the revision the window shows, and is not sent while the figure is stale or busy.
//! The window is played by the test with detached RPC; egui runs headless and no window opens.

use std::sync::mpsc::{Receiver, Sender};

use designllm_contracts::{
    Artifact, ArtifactBody, ArtifactKind, ArtifactSummary, AxisSpec, ContextSnapshot, DatasetRef, ErrorSpec, ErrorValues,
    FigureDocument, FigurePanel, Operation, PanelBody, PlotDisplayMode, PlotErrorKind, PlotSpec, Rect, Size, Style,
};
use serde_json::{json, Value};

use super::{
    figure_tools::plot_display_change, kind_commands::ERROR_ON_AXIS, operation_commands::PLOT_DISPLAY_NO_ERROR, DatasetCellEdit, DesktopApp, PendingAction, Screen,
};
use crate::rpc::{RpcReply, RpcWorker, Work};

const FIGURE: &str = "figure-1";
const PANEL: &str = "panel-1";
const REVISION: u64 = 5;

type Request = (u64, String, Value);

struct Harness {
    app: DesktopApp,
    work: Receiver<Work>,
    // Kept alive: the window reads replies from the worker, which needs this end open.
    _replies: Sender<RpcReply>,
}

impl Harness {
    /// An open Figure at `REVISION` with one plot panel. The host allows plot edits.
    fn new(plot: PlotSpec) -> Self {
        let (rpc, replies, work) = RpcWorker::detached();
        let mut app = DesktopApp::new(rpc, "그림".into(), None, None);
        while work.try_recv().is_ok() {}
        let panel = FigurePanel {
            id: PANEL.into(),
            bounds: Rect { x: 0, y: 0, width: 4_000_000, height: 3_000_000 },
            label: "(a)".into(),
            body: PanelBody::Plot(plot),
            style: Style::default(),
        };
        let document = FigureDocument { panels: vec![panel], ..FigureDocument::new(Size::a4()) };
        let mut artifact = Artifact::new(ArtifactKind::Figure, "그림", "figure", ArtifactBody::Figure(document));
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
        app.selected_node = Some(PANEL.into());
        Self { app, work, _replies: replies }
    }

    fn sent(&mut self) -> Vec<Request> {
        let mut batch = Vec::new();
        while let Ok(work) = self.work.try_recv() {
            if let Work::Call { ticket, method, params, .. } = work {
                batch.push((ticket, method, params));
            }
        }
        batch
    }

    fn figure(&self) -> FigureDocument {
        self.app.figure_document().expect("the figure is open")
    }
}

fn context(revision: u64, view_id: &str) -> ContextSnapshot {
    serde_json::from_value(json!({
        "workspace_id": "w1",
        "artifact_id": FIGURE,
        "revision": revision,
        "view_id": view_id,
        "selection": { "node_ids": [PANEL], "text_range": null },
        "visible_page": 0,
        "selected_properties": { "nodes": null, "stale_selection": null, "render_unavailable": null },
        "capabilities": {
            "plot": { "read": "supported", "render": "supported", "edit": "supported", "preserve": "supported", "write": "supported", "reason": "" }
        },
        "render_snapshot": null,
        "view_transform": null,
    }))
    .expect("a context snapshot")
}

fn axis(label: &str) -> AxisSpec {
    AxisSpec { label: label.into(), unit: None, scale: "linear".into(), minimum: None, maximum: None, tick_step: None }
}

/// A line plot with error bars when `error` is set, showing the error key in the mode `display`.
fn plot(display: Option<PlotDisplayMode>, error: bool) -> PlotSpec {
    PlotSpec {
        error: error.then(|| ErrorSpec {
            kind: PlotErrorKind::Sd,
            values: ErrorValues::Symmetric { column: "col-e".into() },
            sample_count_column: None,
            confidence_level: None,
        }),
        display,
        ..PlotSpec::new(
            "line",
            DatasetRef { dataset_id: "dataset-1".into(), version_hash: "v1".into() },
            "col-x",
            vec!["col-y".into()],
            axis("x"),
            axis("y"),
        )
    }
}

fn figure_of(plot: PlotSpec) -> FigureDocument {
    Harness::new(plot).figure()
}

#[test]
fn turning_the_key_on_names_the_mode_the_plot_has_now() {
    let figure = figure_of(plot(None, true));
    let change = plot_display_change(&figure, PANEL, true).expect("the key is switched on");
    let Operation::SetPlotDisplay { panel_id, expected_display, display } = change else {
        panic!("a display change is sent, not another operation");
    };
    assert_eq!(panel_id, PANEL);
    assert_eq!(expected_display, None, "the plot shows no key now");
    assert_eq!(display, Some(PlotDisplayMode::ErrorKey));
}

#[test]
fn turning_the_key_off_names_the_key_the_plot_shows_now() {
    let figure = figure_of(plot(Some(PlotDisplayMode::ErrorKey), true));
    let Some(Operation::SetPlotDisplay { expected_display, display, .. }) = plot_display_change(&figure, PANEL, false) else {
        panic!("the key is switched off");
    };
    assert_eq!(expected_display, Some(PlotDisplayMode::ErrorKey));
    assert_eq!(display, None);
}

#[test]
fn a_wanted_mode_the_plot_has_and_a_missing_panel_send_nothing() {
    let shown = figure_of(plot(Some(PlotDisplayMode::ErrorKey), true));
    assert!(plot_display_change(&shown, PANEL, true).is_none(), "the key already shows");
    let hidden = figure_of(plot(None, true));
    assert!(plot_display_change(&hidden, PANEL, false).is_none(), "the key is already hidden");
    assert!(plot_display_change(&hidden, "missing", true).is_none(), "no such panel");
}

#[test]
fn the_switch_sends_one_design_change_against_the_revision_shown() {
    let mut harness = Harness::new(plot(None, true));
    let figure = harness.figure();
    assert!(harness.app.set_plot_display(&figure, PANEL, true), "the window sends the change");
    let sent = harness.sent();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].1, "changes.apply");
    assert_eq!(sent[0].2["artifact_id"], FIGURE);
    assert_eq!(sent[0].2["base_revision"], REVISION, "the host checks the revision the window shows");
    assert_eq!(sent[0].2["allowed_intents"], json!(["design"]), "a display mode is a design change");
    assert_eq!(
        sent[0].2["operations"],
        json!([{ "op": "set_plot_display", "panel_id": PANEL, "expected_display": null, "display": "error_key" }])
    );
}

#[test]
fn a_figure_that_is_read_again_or_busy_gets_no_display_change() {
    let mut harness = Harness::new(plot(None, true));
    let figure = harness.figure();

    // A read of the figure is on its way: the shown mode may be older than the host's.
    let _ = harness.app.call("context.read", json!({ "artifact_id": FIGURE }), PendingAction::Context);
    let _ = harness.sent();
    assert!(!harness.app.set_plot_display(&figure, PANEL, true));
    assert!(harness.sent().is_empty(), "nothing is sent while the figure is read again");
    harness.app.pending.clear();

    // The context the window holds is at another revision than the figure on screen.
    harness.app.context.as_mut().expect("a context").revision = REVISION + 1;
    assert!(!harness.app.set_plot_display(&figure, PANEL, true));
    harness.app.context.as_mut().expect("a context").revision = REVISION;

    // An open cell edit would end with the change.
    harness.app.dataset_edit = Some(DatasetCellEdit {
        dataset_id: "dataset-1".into(),
        version_hash: "v1".into(),
        row_id: "row-1".into(),
        column_id: "col-y".into(),
        original: "0.10".into(),
        draft: "0.25".into(),
    });
    assert!(!harness.app.set_plot_display(&figure, PANEL, true));
    assert!(harness.sent().is_empty(), "no change is sent for a stale or busy figure");
    assert_eq!(harness.app.artifact.as_ref().map(|artifact| artifact.revision), Some(REVISION));
}

#[test]
fn the_command_takes_the_rule_of_the_switch_for_a_plot_without_error_bars() {
    let mut harness = Harness::new(plot(None, false));
    let refused = harness.app.invoke("figure.plot_display", json!({ "panel_id": PANEL, "on": true }));
    assert_eq!(refused, Err(PLOT_DISPLAY_NO_ERROR.to_owned()), "a plot without error bars takes no key, as the switch is not offered");
    assert!(harness.sent().is_empty(), "nothing is sent for it");

    let mut harness = Harness::new(plot(None, true));
    assert_eq!(harness.app.invoke("figure.plot_display", json!({ "panel_id": PANEL, "on": true })), Ok(()));
    assert_eq!(harness.sent().iter().filter(|request| request.1 == "changes.apply").count(), 1, "a plot with error bars takes the key");

    // A plot that shows the key without error bars can still turn it off, as the switch allows.
    let mut harness = Harness::new(plot(Some(PlotDisplayMode::ErrorKey), false));
    assert_eq!(harness.app.invoke("figure.plot_display", json!({ "panel_id": PANEL, "on": false })), Ok(()));
    assert_eq!(harness.sent().iter().filter(|request| request.1 == "changes.apply").count(), 1);
}

#[test]
fn figure_plot_refuses_an_axis_column_as_the_error_column_before_sending() {
    let mut harness = Harness::new(plot(None, true));
    for column in ["col-x", "col-y"] {
        let refused = harness.app.invoke("figure.plot", json!({ "panel_id": PANEL, "error_kind": "sd", "error_column": column }));
        assert_eq!(refused, Err(ERROR_ON_AXIS.to_owned()), "{column} is on an axis");
    }
    let refused = harness.app.invoke("figure.plot", json!({ "panel_id": PANEL, "y_columns": ["col-y", "col-e"] }));
    assert_eq!(refused, Err(ERROR_ON_AXIS.to_owned()), "the error column cannot become a y column either");
    assert!(harness.sent().is_empty(), "nothing is sent");
    assert_eq!(harness.app.invoke("figure.plot", json!({ "panel_id": PANEL, "error_kind": "sd", "error_column": "col-e" })), Ok(()));
    assert_eq!(harness.sent().len(), 1, "a column off the axes is sent");
}

#[test]
fn figure_plot_keeps_a_numeric_axis_bound_exactly_and_refuses_one_past_the_engine_digits() {
    let sent_bound = |value: Value| {
        let mut harness = Harness::new(plot(None, true));
        let result = harness.app.invoke("figure.plot", json!({ "panel_id": PANEL, "y_minimum": value }));
        let sent = harness.sent();
        (result, sent.first().map(|request| request.2["operations"][0]["plot"]["y_axis"]["minimum"].clone()))
    };
    assert_eq!(sent_bound(json!(0.0000001)), (Ok(()), Some(json!("0.0000001"))), "a small bound is not rounded to 0");
    assert_eq!(sent_bound(json!(0.30000000000000004)), (Ok(()), Some(json!("0.30000000000000004"))));
    assert_eq!(sent_bound(json!(-40)), (Ok(()), Some(json!("-40"))));
    // 18-digit integers past 2^53 keep their digits (an f64 would round them to 19).
    assert_eq!(sent_bound(json!(999_999_999_999_999_999_u64)), (Ok(()), Some(json!("999999999999999999"))));
    assert_eq!(sent_bound(json!(-999_999_999_999_999_999_i64)), (Ok(()), Some(json!("-999999999999999999"))));
    let (refused, sent) = sent_bound(json!(1e20));
    assert!(refused.is_err() && sent.is_none(), "21 digits are more than the engine reads: {refused:?}");
}

fn plot_of(figure: &FigureDocument) -> PlotSpec {
    match &figure.panels[0].body {
        PanelBody::Plot(plot) => plot.clone(),
        _ => panic!("the panel is a plot"),
    }
}

#[test]
fn the_switch_is_disabled_while_the_window_does_not_show_the_current_revision() {
    // Regression: the checkbox stayed enabled while the window showed another revision, and a click was refused silently.
    let mut harness = Harness::new(plot(None, true));
    let figure = harness.figure();
    assert!(harness.app.plot_display_ready(&plot_of(&figure)), "a window that shows the figure offers the switch");

    harness.app.context.as_mut().expect("a context").revision = REVISION + 1;
    assert!(!harness.app.plot_display_ready(&plot_of(&figure)), "a read context at another revision disables the switch");
    assert!(!harness.app.set_plot_display(&figure, PANEL, true), "nothing is sent for it either");
    harness.app.context.as_mut().expect("a context").revision = REVISION;

    harness.app.context.as_mut().expect("a context").artifact_id = "figure-2".into();
    assert!(!harness.app.plot_display_ready(&plot_of(&figure)), "a context that names another figure disables the switch");
    assert!(!harness.app.set_plot_display(&figure, PANEL, true));
    assert!(harness.sent().is_empty(), "no change is sent while the window shows another figure");
}

#[test]
fn a_plot_without_error_bars_is_not_offered_the_key_unless_it_shows_it() {
    let no_error = Harness::new(plot(None, false));
    assert!(!no_error.app.plot_display_ready(&plot_of(&no_error.figure())), "there is no error key to show");
    let shows = Harness::new(plot(Some(PlotDisplayMode::ErrorKey), false));
    assert!(shows.app.plot_display_ready(&plot_of(&shows.figure())), "a plot that shows the key can switch it off");
}

#[test]
fn a_null_tick_step_reaches_the_handler_and_sends_automatic_ticks() {
    // 49zf: `tick_step` is required and nullable; the null is not stripped, and the handler sends `tick_step: null`.
    let mut harness = Harness::new(plot(None, true));
    harness.app.invoke("figure.axis_ticks", json!({ "panel_id": PANEL, "axis": "y", "tick_step": null })).expect("null is a tick step");
    let sent = harness.sent();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].2["operations"], json!([{ "op": "set_axis_ticks", "panel_id": PANEL, "axis": "y", "tick_step": null }]));
    let error = harness.app.invoke("figure.axis_ticks", json!({ "panel_id": PANEL, "axis": "y" })).expect_err("an omitted tick step is refused");
    assert!(error.contains("missing `tick_step`"), "{error}");
    assert!(harness.sent().is_empty());
}
