//! What the plot section sends to `figure.plot`: the error kinds with the sample count of SEM and the level of CI, the scale,
//! unit and range of each axis. The controls build their requests with the functions in properties.rs, so the first tests
//! check those requests; the window tests send them through `invoke` and check the change set the host receives.

use std::sync::mpsc::{Receiver, Sender};

use designllm_contracts::{
    Artifact, ArtifactBody, ArtifactKind, ArtifactSummary, AxisSpec, ContextSnapshot, DataColumn, DatasetRef, ErrorSpec, ErrorValues,
    FigureDocument, FigurePanel, PanelBody, PlotErrorKind, PlotSpec, Rect, Size, Style,
};
use serde_json::{json, Value};

use super::{axis_field_request, count_columns, error_kind_change, error_request, error_value_columns, range_change, range_commit, ErrorParts, RangeDraft};
use super::super::{DesktopApp, Screen};
use crate::{
    commands,
    rpc::{RpcReply, RpcWorker, Work},
};

const FIGURE: &str = "figure-1";
const PANEL: &str = "panel-1";
const REVISION: u64 = 5;

fn axis(label: &str) -> AxisSpec {
    AxisSpec { label: label.into(), unit: None, scale: "linear".into(), minimum: None, maximum: None, tick_step: None }
}

fn column(id: &str, data_type: &str) -> DataColumn {
    DataColumn { id: id.into(), name: id.into(), data_type: data_type.into(), unit: None }
}

/// A line plot of `col-x` against `col-y` with the error `error`.
fn line_plot(error: Option<ErrorSpec>) -> PlotSpec {
    PlotSpec {
        error,
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

/// Symmetric error bars over `column`, with the sample count and the confidence level as given.
fn symmetric(kind: PlotErrorKind, column: &str, count: Option<&str>, level: Option<&str>) -> ErrorSpec {
    ErrorSpec {
        kind,
        values: ErrorValues::Symmetric { column: column.into() },
        sample_count_column: count.map(str::to_owned),
        confidence_level: level.map(str::to_owned),
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

/// An open figure at `REVISION` whose one panel is `plot`. Requests go to a detached RPC that the test reads.
struct Harness {
    app: DesktopApp,
    work: Receiver<Work>,
    // Kept alive: the window reads replies from the worker, which needs this end open.
    _replies: Sender<RpcReply>,
}

impl Harness {
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

    /// The operations of the `changes.apply` calls sent since the last call, in order.
    fn operations(&mut self) -> Vec<Value> {
        let mut operations = Vec::new();
        while let Ok(work) = self.work.try_recv() {
            if let Work::Call { method, params, .. } = work {
                if method == "changes.apply" {
                    operations.extend(params["operations"].as_array().cloned().unwrap_or_default());
                }
            }
        }
        operations
    }
}

#[test]
fn a_sem_request_sends_the_sample_count_and_no_level() {
    let spec = symmetric(PlotErrorKind::Sem, "col-e", Some("col-n"), None);
    let request = error_request(PANEL, "sem", &ErrorParts::of(Some(&spec)));
    assert_eq!(request, json!({ "panel_id": PANEL, "error_kind": "sem", "error_column": "col-e", "error_count_column": "col-n" }));
}

#[test]
fn a_column_change_keeps_the_sample_count_of_sem_and_the_level_of_ci() {
    // figure_plot clears a part a request leaves out, so each request carries the count (SEM) or the level (CI) the plot has.
    let sem = symmetric(PlotErrorKind::Sem, "col-e", Some("col-n"), None);
    let mut parts = ErrorParts::of(Some(&sem));
    parts.column = Some("col-e2".into());
    let request = error_request(PANEL, "sem", &parts);
    assert_eq!(request["error_count_column"], "col-n");
    assert_eq!(request["error_column"], "col-e2");

    let ci = symmetric(PlotErrorKind::Ci, "col-e", None, Some("0.99"));
    let mut parts = ErrorParts::of(Some(&ci));
    parts.column = Some("col-e2".into());
    let request = error_request(PANEL, "ci", &parts);
    assert_eq!(request["confidence_level"], "0.99", "the level is not reset to the default");
}

#[test]
fn a_ci_request_sends_the_level_and_no_sample_count() {
    let parts = ErrorParts { column: Some("col-e".into()), count: Some("col-n".into()), level: Some("0.9".into()), ..ErrorParts::default() };
    assert_eq!(error_request(PANEL, "ci", &parts), json!({ "panel_id": PANEL, "error_kind": "ci", "error_column": "col-e", "confidence_level": "0.9" }));
}

#[test]
fn a_ci_request_without_a_level_sends_the_default_level() {
    let parts = ErrorParts { column: Some("col-e".into()), ..ErrorParts::default() };
    let request = error_request(PANEL, "ci", &parts);
    assert_eq!(request["confidence_level"], "0.95");
}

#[test]
fn an_sd_request_sends_neither_a_sample_count_nor_a_level() {
    let parts = ErrorParts { column: Some("col-e".into()), count: Some("col-n".into()), level: Some("0.9".into()), ..ErrorParts::default() };
    assert_eq!(error_request(PANEL, "sd", &parts), json!({ "panel_id": PANEL, "error_kind": "sd", "error_column": "col-e" }));
}

#[test]
fn a_range_request_sends_both_bounds() {
    let parts = ErrorParts { lower: Some("col-lo".into()), upper: Some("col-hi".into()), ..ErrorParts::default() };
    assert_eq!(
        error_request(PANEL, "range", &parts),
        json!({ "panel_id": PANEL, "error_kind": "range", "error_lower_column": "col-lo", "error_upper_column": "col-hi" })
    );
}

#[test]
fn a_none_request_sends_only_the_kind() {
    assert_eq!(error_request(PANEL, "none", &ErrorParts::default()), json!({ "panel_id": PANEL, "error_kind": "none" }));
}

#[test]
fn a_new_sem_kind_takes_the_first_count_column_unless_it_has_one() {
    let sd = ErrorParts { column: Some("col-e".into()), ..ErrorParts::default() };
    let next = error_kind_change(&sd, "sem", &["col-spare"], Some("col-n"));
    assert_eq!(next.count.as_deref(), Some("col-n"));
    assert_eq!(next.column.as_deref(), Some("col-e"), "the column it had is kept");

    let counted = ErrorParts { column: Some("col-e".into()), count: Some("col-n2".into()), ..ErrorParts::default() };
    assert_eq!(error_kind_change(&counted, "sem", &[], Some("col-n")).count.as_deref(), Some("col-n2"));
}

#[test]
fn a_new_range_kind_takes_two_different_spare_columns_for_its_bounds() {
    let sd = ErrorParts { column: Some("col-e".into()), ..ErrorParts::default() };
    let next = error_kind_change(&sd, "range", &["col-a", "col-b"], None);
    assert_eq!(next.lower.as_deref(), Some("col-a"));
    assert_eq!(next.upper.as_deref(), Some("col-b"));
}

#[test]
fn a_range_with_one_spare_column_leaves_the_upper_bound_empty_so_the_window_refuses_it() {
    let spec = symmetric(PlotErrorKind::Sd, "col-e", None, None);
    let next = error_kind_change(&ErrorParts::of(Some(&spec)), "range", &["col-a"], None);
    assert_eq!(next.lower.as_deref(), Some("col-a"));
    assert_eq!(next.upper, None);
    let mut harness = Harness::new(line_plot(Some(spec)));
    let message = harness.app.invoke("figure.plot", error_request(PANEL, "range", &next)).expect_err("a range needs both columns");
    assert!(message.contains("아래·위"), "{message}");
    assert!(harness.operations().is_empty(), "nothing is sent");
}

#[test]
fn an_asymmetric_error_is_not_read_as_range_bounds() {
    let spec = ErrorSpec {
        kind: PlotErrorKind::Sd,
        values: ErrorValues::Asymmetric { minus_column: "col-m".into(), plus_column: "col-p".into() },
        sample_count_column: None,
        confidence_level: None,
    };
    let parts = ErrorParts::of(Some(&spec));
    assert!(parts.lower.is_none() && parts.upper.is_none(), "the minus and plus amounts are not the bounds");
    let next = error_kind_change(&parts, "range", &["col-a", "col-b"], None);
    assert_eq!((next.lower.as_deref(), next.upper.as_deref()), (Some("col-a"), Some("col-b")));
}

#[test]
fn the_error_menus_list_numeric_columns_that_are_not_on_an_axis() {
    let columns = [column("col-x", "numeric"), column("col-y", "numeric"), column("col-e", "numeric"), column("col-n", "count"), column("col-t", "text")];
    let ids: Vec<&str> = error_value_columns(&columns, "col-x", &["col-y".to_owned()]).into_iter().map(|column| column.id.as_str()).collect();
    assert_eq!(ids, vec!["col-e"]);
}

#[test]
fn axis_requests_name_the_field_of_their_axis() {
    assert_eq!(axis_field_request(PANEL, "x", "scale", json!("log10")), json!({ "panel_id": PANEL, "x_scale": "log10" }));
    assert_eq!(axis_field_request(PANEL, "y", "unit", json!("mg/L")), json!({ "panel_id": PANEL, "y_unit": "mg/L" }));
}

#[test]
fn the_sample_count_menu_lists_count_columns_only() {
    let columns = [column("col-x", "numeric"), column("col-n", "count"), column("col-t", "text")];
    let counts: Vec<String> = count_columns(&columns).into_iter().map(|column| column.id).collect();
    assert_eq!(counts, vec!["col-n".to_owned()]);
}

#[test]
fn the_requests_pass_the_figure_plot_schema_the_window_checks_before_it_sends() {
    let spec = commands::find("figure.plot").expect("figure.plot is registered");
    let parts = ErrorParts {
        column: Some("col-e".into()),
        lower: Some("col-lo".into()),
        upper: Some("col-hi".into()),
        count: Some("col-n".into()),
        level: Some("0.9".into()),
    };
    for kind in ["none", "sd", "sem", "ci", "range"] {
        let request = error_request(PANEL, kind, &parts);
        assert!(commands::validate(spec, &request).is_ok(), "{kind}: {request}");
    }
    for request in [
        axis_field_request(PANEL, "x", "scale", json!("linear")),
        axis_field_request(PANEL, "x", "unit", json!("mg")),
        axis_field_request(PANEL, "y", "unit", json!("mg/L")),
    ] {
        assert!(commands::validate(spec, &request).is_ok(), "{request}");
    }
}

#[test]
fn a_sem_column_change_reaches_the_host_with_the_sample_count_it_had() {
    let spec = symmetric(PlotErrorKind::Sem, "col-e", Some("col-n"), None);
    let mut parts = ErrorParts::of(Some(&spec));
    let mut harness = Harness::new(line_plot(Some(spec)));
    parts.column = Some("col-e2".into());
    harness.app.invoke("figure.plot", error_request(PANEL, "sem", &parts)).expect("the window accepts the request");
    let operations = harness.operations();
    assert_eq!(operations.len(), 1, "one change set");
    let error = &operations[0]["plot"]["error"];
    assert_eq!(error["kind"], "sem");
    assert_eq!(error["values"]["column"], "col-e2");
    assert_eq!(error["sample_count_column"], "col-n", "the count survives the column change");
}

#[test]
fn a_ci_column_change_reaches_the_host_with_the_level_it_had() {
    let spec = symmetric(PlotErrorKind::Ci, "col-e", None, Some("0.99"));
    let mut parts = ErrorParts::of(Some(&spec));
    let mut harness = Harness::new(line_plot(Some(spec)));
    parts.column = Some("col-e2".into());
    harness.app.invoke("figure.plot", error_request(PANEL, "ci", &parts)).expect("the window accepts the request");
    let operations = harness.operations();
    let error = &operations[0]["plot"]["error"];
    assert_eq!(error["kind"], "ci");
    assert_eq!(error["values"]["column"], "col-e2");
    assert_eq!(error["confidence_level"], "0.99", "the level survives the column change");
}

#[test]
fn a_range_bound_change_reaches_the_host_with_both_bounds() {
    let spec = ErrorSpec {
        kind: PlotErrorKind::Range,
        values: ErrorValues::Bounds { lower_column: "col-lo".into(), upper_column: "col-hi".into() },
        sample_count_column: None,
        confidence_level: None,
    };
    let mut parts = ErrorParts::of(Some(&spec));
    let mut harness = Harness::new(line_plot(Some(spec)));
    parts.upper = Some("col-hi2".into());
    harness.app.invoke("figure.plot", error_request(PANEL, "range", &parts)).expect("the window accepts the request");
    let operations = harness.operations();
    let error = &operations[0]["plot"]["error"];
    assert_eq!(error["values"]["type"], "bounds");
    assert_eq!(error["values"]["lower_column"], "col-lo");
    assert_eq!(error["values"]["upper_column"], "col-hi2");
}

#[test]
fn an_axis_unit_reaches_the_host_on_its_own_axis() {
    let mut harness = Harness::new(line_plot(None));
    harness.app.invoke("figure.plot", axis_field_request(PANEL, "y", "unit", json!("mg/L"))).expect("the window accepts the unit");
    let operations = harness.operations();
    assert_eq!(operations.len(), 1);
    assert_eq!(operations[0]["plot"]["y_axis"]["unit"], "mg/L");
    assert!(operations[0]["plot"]["x_axis"]["unit"].is_null(), "the x unit is left as it was");
}

#[test]
fn a_range_request_carries_both_ends_or_neither() {
    assert_eq!(range_change(PANEL, "y", "0", "40"), Some(json!({ "panel_id": PANEL, "y_minimum": 0.0, "y_maximum": 40.0 })));
    assert_eq!(range_change(PANEL, "y", "", ""), Some(json!({ "panel_id": PANEL, "y_minimum": null, "y_maximum": null })));
    assert_eq!(range_change(PANEL, "y", "0", ""), None, "one end alone is a partial range");
    assert_eq!(range_change(PANEL, "y", "abc", "40"), None, "a text that is not a number is not sent");
}

#[test]
fn the_range_requests_pass_the_figure_plot_schema_the_window_checks_before_it_sends() {
    let spec = commands::find("figure.plot").expect("figure.plot is registered");
    for (min, max) in [("0", "40"), ("", "")] {
        let request = range_change(PANEL, "y", min, max).expect("a sendable range");
        assert!(commands::validate(spec, &request).is_ok(), "{request}");
    }
}

#[test]
fn an_end_typed_alone_waits_for_the_other_end() {
    let (draft, change) = range_commit(PANEL, "y", RangeDraft::default(), Some("0".into()), None);
    assert_eq!(change, None, "the max is automatic, so the pair is partial");
    assert_eq!(draft.min.as_deref(), Some("0"));
    let (draft, change) = range_commit(PANEL, "y", draft, None, Some("40".into()));
    assert_eq!(change, Some(json!({ "panel_id": PANEL, "y_minimum": 0.0, "y_maximum": 40.0 })));
    assert_eq!((draft.min, draft.max), (None, None), "the pair was sent, nothing waits");
}

#[test]
fn a_changed_end_of_a_set_range_sends_the_other_end_as_it_is() {
    let stored = RangeDraft { base: ("0".into(), "40".into()), ..RangeDraft::default() };
    let (_, change) = range_commit(PANEL, "y", stored, None, Some("50".into()));
    assert_eq!(change, Some(json!({ "panel_id": PANEL, "y_minimum": 0.0, "y_maximum": 50.0 })));
}

#[test]
fn clearing_both_ends_of_a_range_returns_the_axis_to_automatic() {
    let stored = RangeDraft { base: ("0".into(), "40".into()), ..RangeDraft::default() };
    let (draft, change) = range_commit(PANEL, "y", stored, Some(String::new()), None);
    assert_eq!(change, None, "the max is still set, so the pair is partial");
    let (_, change) = range_commit(PANEL, "y", draft, None, Some(String::new()));
    assert_eq!(change, Some(json!({ "panel_id": PANEL, "y_minimum": null, "y_maximum": null })));
}

#[test]
fn a_bound_that_is_not_a_number_is_dropped_and_the_stored_range_stays() {
    let stored = RangeDraft { base: ("0".into(), "40".into()), ..RangeDraft::default() };
    let (draft, change) = range_commit(PANEL, "y", stored.clone(), Some("abc".into()), None);
    assert_eq!(change, None);
    assert_eq!(draft, stored, "nothing waits: the field shows the stored end again");
}

#[test]
fn a_range_change_reaches_the_host_with_both_ends() {
    let mut plot = line_plot(None);
    plot.y_axis.minimum = Some("0".into());
    plot.y_axis.maximum = Some("40".into());
    let mut harness = Harness::new(plot);
    let change = range_change(PANEL, "y", "0", "50").expect("both ends");
    harness.app.invoke("figure.plot", change).expect("the window accepts the range");
    let operations = harness.operations();
    assert_eq!(operations.len(), 1);
    assert_eq!(operations[0]["plot"]["y_axis"]["minimum"], "0");
    assert_eq!(operations[0]["plot"]["y_axis"]["maximum"], "50");
}

#[test]
fn a_cleared_range_reaches_the_host_as_automatic() {
    let mut plot = line_plot(None);
    plot.y_axis.minimum = Some("0".into());
    plot.y_axis.maximum = Some("40".into());
    let mut harness = Harness::new(plot);
    let change = range_change(PANEL, "y", "", "").expect("both automatic");
    harness.app.invoke("figure.plot", change).expect("the window accepts an automatic range");
    let operations = harness.operations();
    assert_eq!(operations.len(), 1);
    assert!(operations[0]["plot"]["y_axis"]["minimum"].is_null());
    assert!(operations[0]["plot"]["y_axis"]["maximum"].is_null());
}
