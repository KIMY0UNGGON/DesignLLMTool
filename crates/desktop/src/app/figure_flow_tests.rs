//! The figure-creation controls against the real figure engine, store and renderer: the change
//! sets the window builds for a new figure (a chart from a CSV table, a diagram with nodes and a
//! connector, an edited cell) must be accepted by the same code the host runs.

use std::collections::BTreeSet;

use designllm_contracts::{
    Evidence, FigureDocument, Intent, NodeKind, Operation, PanelBody, Primitive, RenderSnapshot, Rect, EMU_PER_MM,
};
use serde_json::json;

use super::{
    apply::dataset_edit_request,
    figure_tools::{chart_change, connection, evidence_additions, new_diagram_panel, node_addition, panel_additions},
    CsvImportDraft, DatasetCellEdit,
};

/// The host the window talks to (moved to `test_support.rs` for the parity tests; command tests import it from here).
pub(super) use super::test_support::{repository_root, Host};

fn diagram_nodes(figure: &FigureDocument, panel: usize) -> Vec<String> {
    match &figure.panels[panel].body {
        PanelBody::Diagram(diagram) => diagram.nodes.iter().map(|node| node.id.clone()).collect(),
        _ => panic!("not a diagram"),
    }
}

fn render(host: &mut Host, figure_id: &str, revision: u64) -> RenderSnapshot {
    serde_json::from_value(
        host.call(
            "render.get",
            json!({ "artifact_id": figure_id, "expected_revision": revision, "page_index": 0, "profile": { "dpi": 144, "transparent": false } }),
        )
        .expect("the figure renders"),
    )
    .expect("snapshot")
}

/// Fill colours of the drawn shapes whose id contains `part` (a marker or a bar of a plot).
fn fills<'a>(snapshot: &'a RenderSnapshot, part: &str) -> Vec<&'a str> {
    snapshot.scene.pages[0]
        .primitives
        .iter()
        .filter_map(|primitive| match primitive {
            Primitive::Shape { node_id, style, .. } if node_id.contains(part) => Some(style.fill.as_str()),
            _ => None,
        })
        .collect()
}

#[test]
fn a_chart_made_from_a_csv_table_is_accepted_and_renders() {
    let mut host = Host::start();
    let figure_id = host.create_figure();
    let mut draft = host.declared_draft("id,value\nA,12\nB,18\nC,25\nD,31\n", |_| {});
    assert_eq!(draft.plot_kind, "line");
    assert_eq!((draft.x_column.as_str(), draft.y_column.as_str()), ("id", "value"));

    let (figure, _) = host.figure(&figure_id);
    assert!(chart_change(&figure, &draft, true).is_none(), "the y unit is required before a chart is offered");
    draft.y_unit = "%".into();
    let (operations, intents) = chart_change(&figure, &draft, true).expect("a fully described chart");
    assert!(operations.iter().any(|op| matches!(op, Operation::PutDataset { .. })));
    host.apply(&figure_id, operations, intents)
        .expect("the engine accepts the table, the plot panel and the panel layout");

    let (figure, revision) = host.figure(&figure_id);
    assert_eq!(figure.datasets.len(), 1);
    assert_eq!(figure.panels.len(), 1);
    assert_eq!(figure.panels[0].label, "(a)");
    let PanelBody::Plot(plot) = &figure.panels[0].body else { panic!("the panel plots") };
    assert_eq!(plot.dataset_ref.version_hash, figure.datasets[0].version_hash);

    let snapshot = render(&mut host, &figure_id, revision);
    assert!(snapshot.preview.is_some());
    assert!(
        snapshot.scene.diagnostics.iter().all(|diagnostic| !diagnostic.blocking),
        "{:?}",
        snapshot.scene.diagnostics
    );
    // The data marks must be drawn in a colour that shows on the white page: the first series
    // takes the panel's fill, and the default style fills with white.
    let marks = fills(&snapshot, "/mk0/");
    assert_eq!(marks.len(), 4, "one marker per row");
    assert!(marks.iter().all(|fill| !fill.eq_ignore_ascii_case("#FFFFFF")), "{marks:?}");

    // The same table again reuses the version the figure holds instead of replacing it, and adds
    // a second chart next to the first.
    let (figure, _) = host.figure(&figure_id);
    let mut second = draft.clone();
    second.plot_kind = "bar".into();
    second.repair_columns();
    let (operations, intents) = chart_change(&figure, &second, true).expect("second chart");
    assert!(!operations.iter().any(|op| matches!(op, Operation::PutDataset { .. })));
    host.apply(&figure_id, operations, intents).expect("a second chart over the same table");
    let (figure, _) = host.figure(&figure_id);
    assert_eq!(figure.panels.len(), 2);
    assert_eq!(figure.datasets.len(), 1);
    assert_eq!(figure.panels[1].label, "(b)");
    let (a, b) = (figure.panels[0].bounds, figure.panels[1].bounds);
    assert!(a.x + a.width <= b.x || b.x + b.width <= a.x || a.y + a.height <= b.y || b.y + b.height <= a.y);
}

#[test]
fn a_numeric_x_chart_with_the_units_declared_at_import_is_accepted() {
    let mut host = Host::start();
    let figure_id = host.create_figure();
    let mut draft = host.declared_draft("dose,response\n10,12\n20,18\n30,25\n40,31\n", |draft| {
        draft.columns[0].unit = "mg".into();
        draft.columns[1].unit = "%".into();
    });
    assert_eq!((draft.x_column.as_str(), draft.y_column.as_str()), ("dose", "response"));
    assert_eq!((draft.x_unit.as_str(), draft.y_unit.as_str()), ("mg", "%"), "the declared units are the axis units");

    for kind in ["line", "scatter"] {
        draft.plot_kind = kind.into();
        draft.repair_columns();
        let (figure, _) = host.figure(&figure_id);
        let (operations, intents) = chart_change(&figure, &draft, true).expect("chart");
        host.apply(&figure_id, operations, intents)
            .unwrap_or_else(|error| panic!("the {kind} chart is refused: {error:?}"));
    }
    let (figure, revision) = host.figure(&figure_id);
    assert_eq!(figure.panels.len(), 2);
    assert_eq!(
        figure.datasets[0].columns.iter().map(|column| column.unit.as_deref()).collect::<Vec<_>>(),
        [Some("mg"), Some("%")],
        "the table keeps the units it was declared with"
    );
    let snapshot = render(&mut host, &figure_id, revision);
    assert!(
        snapshot.scene.diagnostics.iter().all(|diagnostic| !diagnostic.blocking),
        "{:?}",
        snapshot.scene.diagnostics
    );
    assert_eq!(fills(&snapshot, "/mk0/").len(), 8, "four markers in each chart");
}

#[test]
fn the_table_alone_can_be_added_without_a_chart() {
    let mut host = Host::start();
    let figure_id = host.create_figure();
    let dataset = host.import_table("id,value\nA,1\nB,2\n");
    let draft = CsvImportDraft::new(dataset);
    let (figure, _) = host.figure(&figure_id);
    let (operations, intents) = chart_change(&figure, &draft, false).expect("table only");
    assert_eq!(intents, BTreeSet::from([Intent::Data]));
    host.apply(&figure_id, operations, intents).expect("a table without a panel");
    let (figure, _) = host.figure(&figure_id);
    assert_eq!((figure.datasets.len(), figure.panels.len()), (1, 0));
    assert!(chart_change(&figure, &draft, false).is_none(), "nothing left to add");
}

#[test]
fn a_bar_chart_over_a_table_with_an_id_column_is_accepted() {
    let mut host = Host::start();
    let figure_id = host.create_figure();
    let dataset = host.import_table("id,value\nA,1\nB,2\nC,3\n");
    let mut draft = CsvImportDraft::new(dataset);
    draft.plot_kind = "bar".into();
    draft.repair_columns();
    assert_eq!((draft.x_column.as_str(), draft.y_column.as_str()), ("id", "value"));
    draft.y_unit = "mg/L".into();
    let (figure, _) = host.figure(&figure_id);
    let (operations, intents) = chart_change(&figure, &draft, true).expect("bar chart");
    host.apply(&figure_id, operations, intents).expect("categories on x, numbers on y");
}

#[test]
fn a_group_value_table_declared_as_category_makes_a_bar_chart() {
    let mut host = Host::start();
    let figure_id = host.create_figure();
    let csv = "group,value\ncontrol,12\ntreated,18\nrescued,25\n";

    // Read with inferred types, the label column is text and has no place on a bar chart's x axis.
    let inferred = CsvImportDraft::new(host.import_table(csv));
    let mut as_inferred = inferred.clone();
    as_inferred.plot_kind = "bar".into();
    as_inferred.repair_columns();
    assert!(as_inferred.x_column.is_empty(), "text cannot be an x axis");

    // The user declares the label column a category and gives the values their unit.
    let mut draft = host.declared_draft(csv, |draft| {
        draft.columns[0].data_type = "category".into();
        draft.columns[1].unit = "mg/L".into();
    });
    draft.plot_kind = "bar".into();
    draft.repair_columns();
    draft.sync_units("", "");
    assert_eq!((draft.x_column.as_str(), draft.y_column.as_str()), ("group", "value"));
    assert_eq!(draft.y_unit, "mg/L");

    let (figure, _) = host.figure(&figure_id);
    let (operations, intents) = chart_change(&figure, &draft, true).expect("a described bar chart");
    host.apply(&figure_id, operations, intents)
        .expect("the declared table and its bar chart are accepted end to end");

    let (figure, revision) = host.figure(&figure_id);
    let table = &figure.datasets[0];
    assert_eq!(
        table
            .columns
            .iter()
            .map(|column| (column.name.as_str(), column.data_type.as_str(), column.unit.as_deref()))
            .collect::<Vec<_>>(),
        [("group", "category", None), ("value", "numeric", Some("mg/L"))],
        "the figure holds the table as it was declared"
    );
    let snapshot = render(&mut host, &figure_id, revision);
    assert!(
        snapshot.scene.diagnostics.iter().all(|diagnostic| !diagnostic.blocking),
        "{:?}",
        snapshot.scene.diagnostics
    );
    let bars = fills(&snapshot, "/bar");
    assert_eq!(bars.len(), 3, "one bar per row");
    assert!(bars.iter().all(|fill| !fill.eq_ignore_ascii_case("#FFFFFF")), "{bars:?}");
}

#[test]
fn a_declaration_the_host_refuses_is_reported_with_the_hosts_own_reason() {
    let mut host = Host::start();
    let mut draft = CsvImportDraft::to_declare(host.import_table("group,value\nA,1\nB,2\n"));
    // The window never sends these (a unit only goes with a numeric column), so build the request
    // the way a hand-written client could and check what the host says.
    let mut request = draft.declaration_request();
    request["columns"][0]["unit"] = json!("mg");
    let error = host.call("source.import_csv", request).expect_err("a unit on a text column");
    assert!(error.message.contains("unit"), "{}", error.message);
    assert!(error.message.contains("group"), "{}", error.message);

    // A column left out of the declaration is refused with its name.
    draft.columns.pop();
    let error = host
        .call("source.import_csv", draft.declaration_request())
        .expect_err("every header column must be declared");
    assert!(error.message.contains("value"), "{}", error.message);
}

#[test]
fn a_diagram_gets_nodes_a_connector_labels_and_moves() {
    let mut host = Host::start();
    let figure_id = host.create_figure();
    let (figure, _) = host.figure(&figure_id);
    let (operations, intents) = (
        panel_additions(&figure, new_diagram_panel(&figure)),
        [Intent::Content, Intent::Design].into_iter().collect::<BTreeSet<_>>(),
    );
    host.apply(&figure_id, operations, intents).expect("an empty diagram panel");

    for kind in [NodeKind::Rectangle, NodeKind::Ellipse, NodeKind::Rectangle] {
        let (figure, _) = host.figure(&figure_id);
        let panel_id = figure.panels[0].id.clone();
        let operation = node_addition(&figure, &panel_id, kind).expect("room for a node");
        host.apply(&figure_id, vec![operation], [Intent::Content].into_iter().collect())
            .expect("a node inside its panel");
    }
    let (figure, _) = host.figure(&figure_id);
    let nodes = diagram_nodes(&figure, 0);
    assert_eq!(nodes.len(), 3);
    let panel_id = figure.panels[0].id.clone();

    // Labels: the figure engine rejects a label that does not fit its node.
    host.apply(
        &figure_id,
        vec![Operation::SetText { node_id: nodes[0].clone(), text: "입력".into() }],
        [Intent::Content].into_iter().collect(),
    )
    .expect("a short label fits");
    host.apply(
        &figure_id,
        vec![Operation::SetPanelLabel { panel_id: panel_id.clone(), label: "(b)".into() }],
        [Intent::Content].into_iter().collect(),
    )
    .expect("a panel label");

    host.apply(
        &figure_id,
        vec![connection(&panel_id, &nodes[0], &nodes[1])],
        [Intent::Content].into_iter().collect(),
    )
    .expect("a connector between neighbours");

    // Dragging a node sends its panel-relative bounds, moved by the drag distance.
    let (figure, _) = host.figure(&figure_id);
    let PanelBody::Diagram(diagram) = &figure.panels[0].body else { panic!("diagram") };
    let moved = Rect { y: diagram.nodes[2].bounds.y + 2 * EMU_PER_MM, ..diagram.nodes[2].bounds };
    host.apply(
        &figure_id,
        vec![Operation::SetGeometry { node_id: nodes[2].clone(), bounds: moved }],
        [Intent::Design].into_iter().collect(),
    )
    .expect("a node moved inside its panel");
}

#[test]
fn a_diagram_panel_runs_out_of_slots_before_the_engine_would_refuse_a_node() {
    let mut host = Host::start();
    let figure_id = host.create_figure();
    let (figure, _) = host.figure(&figure_id);
    host.apply(
        &figure_id,
        panel_additions(&figure, new_diagram_panel(&figure)),
        [Intent::Content, Intent::Design].into_iter().collect(),
    )
    .expect("panel");
    let mut added = 0;
    loop {
        let (figure, _) = host.figure(&figure_id);
        let panel_id = figure.panels[0].id.clone();
        let Some(operation) = node_addition(&figure, &panel_id, NodeKind::Rectangle) else { break };
        host.apply(&figure_id, vec![operation], [Intent::Content].into_iter().collect())
            .unwrap_or_else(|error| panic!("slot {added} was offered but refused: {error:?}"));
        added += 1;
        assert!(added < 64, "the slots must end");
    }
    assert!(added >= 6, "a single full-width panel holds at least six nodes, held {added}");
}

#[test]
fn an_edited_cell_goes_to_the_host_which_derives_the_version_and_rebinds_the_plot() {
    let mut host = Host::start();
    let figure_id = host.create_figure();
    let dataset = host.import_table("id,value\nA,12\nB,18\nC,25\n");
    let mut draft = CsvImportDraft::new(dataset);
    draft.y_unit = "%".into();
    let (figure, _) = host.figure(&figure_id);
    let (operations, intents) = chart_change(&figure, &draft, true).expect("chart");
    host.apply(&figure_id, operations, intents).expect("chart");

    let (figure, revision) = host.figure(&figure_id);
    let table = &figure.datasets[0];
    let row = &table.rows[1];
    let column = table.columns.iter().find(|column| column.name == "value").expect("column");
    let edit = DatasetCellEdit {
        dataset_id: table.id.clone(),
        version_hash: table.version_hash.clone(),
        row_id: row.id.clone(),
        column_id: column.id.clone(),
        original: row.cells[&column.id].original.clone(),
        draft: "19".into(),
    };
    let request = dataset_edit_request(&host.workspace_id, &figure_id, revision, &edit);
    host.call("figure.dataset.edit", request).expect("the host derives the edited table");

    let (after, _) = host.figure(&figure_id);
    let edited = &after.datasets[0];
    assert_ne!(edited.version_hash, table.version_hash, "a new version, not an overwrite");
    assert_eq!(edited.rows[1].cells[&column.id].original, "19");
    let PanelBody::Plot(plot) = &after.panels[0].body else { panic!("plot") };
    assert_eq!(plot.dataset_ref.version_hash, edited.version_hash, "the plot follows the new version");

    // An edit made on the old version is refused, so the window reloads instead of overwriting.
    let stale = dataset_edit_request(&host.workspace_id, &figure_id, revision + 1, &edit);
    let error = host.call("figure.dataset.edit", stale).expect_err("stale version");
    assert!(matches!(error.code, designllm_contracts::ErrorCode::RevisionConflict));

    // A value the engine cannot read exactly is refused, never turned into a number.
    let (current, current_revision) = host.figure(&figure_id);
    let bad = DatasetCellEdit {
        version_hash: current.datasets[0].version_hash.clone(),
        original: "19".into(),
        draft: "1,5".into(),
        ..edit
    };
    let request = dataset_edit_request(&host.workspace_id, &figure_id, current_revision, &bad);
    let error = host.call("figure.dataset.edit", request).expect_err("ambiguous number");
    assert!(matches!(error.code, designllm_contracts::ErrorCode::DataAmbiguous));
}

#[test]
fn text_evidence_read_from_a_pdf_page_is_added_to_the_figure() {
    let mut host = Host::start();
    let figure_id = host.create_figure();
    let pdf = repository_root().join("fixtures/pdf/evidence-p12.pdf");
    let asset = host
        .call("asset.import", json!({ "path": pdf.to_string_lossy() }))
        .expect("import the PDF");

    // The window asks for the page the user typed (12, counted from one) as physical page 11.
    let page = host
        .call("source.extract_pdf", json!({ "sha256": asset["sha256"], "pages": [11] }))
        .expect("page 12 has text");
    let evidence: Vec<Evidence> = serde_json::from_value(page["evidence"].clone()).expect("evidence");
    assert!(!evidence.is_empty());
    assert!(evidence.iter().all(|item| item.physical_page_index == Some(11)));
    let count = evidence.len();

    host.apply(&figure_id, evidence_additions(evidence), [Intent::Content].into_iter().collect())
        .expect("the figure accepts what the PDF reader returned");
    let (figure, _) = host.figure(&figure_id);
    assert_eq!(figure.evidence.len(), count);
    assert!(figure.evidence.iter().all(|item| item.origin == "paper_evidence"));
    assert_eq!(figure.evidence[0].physical_page_index, Some(11));
    assert!(figure
        .source_scopes
        .iter()
        .any(|scope| scope.physical_page_indices.contains(&11)));
}
