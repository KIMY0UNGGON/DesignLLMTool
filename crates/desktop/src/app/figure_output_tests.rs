//! S21: a Figure's output profile and export DPI outlive a restart, a new plot takes the type size and strokes of the plots
//! already there, a kept-legend notice reaches its Figure even after a switch, and connector menus show whole node texts.

use std::collections::BTreeSet;

use designllm_contracts::{
    figure_rules::figure_output_profile, ArtifactSummary, CommonLegend, CommonLegendItem, CommonLegendPlacement, Intent,
    Operation, PanelBody, Size,
};
use eframe::egui::{self, Event, Modifiers, PointerButton, Pos2};
use serde_json::json;

use super::{
    figure_profile::series_colors,
    figure_tools::{chart_change, chart_change_with},
    test_support::*,
    DesktopApp, Screen,
};
use crate::rpc::RpcWorker;

/// A figure with one chart over a three-column table (an x column and two value columns), and the table's draft.
fn charted_figure(host: &mut Host, title: &str) -> (String, super::CsvImportDraft) {
    let mut draft = host.declared_draft("id,a,b\nA,12,3\nB,18,4\n", |_| {});
    draft.y_unit = "%".into();
    let figure_id = host.create("figure", title);
    let (doc, _) = host.figure(&figure_id);
    let (operations, intents) = chart_change(&doc, &draft, true).expect("a chart");
    host.apply(&figure_id, operations, intents).expect("the chart");
    (figure_id, draft)
}

/// The window's app given the GUI settings of `root`, as at start-up (`with_workspace`).
fn with_workspace(window: &mut Window, root: &std::path::Path) {
    let (rpc, _, _) = RpcWorker::detached();
    let app = std::mem::replace(&mut window.app, DesktopApp::new(rpc, "Workspace".into(), None, None));
    window.app = app.with_workspace(root);
}

fn summary_of(host: &mut Host, artifact_id: &str) -> ArtifactSummary {
    let list = host.call("workspace.list", json!({})).expect("the work list");
    let summary = list["artifacts"].as_array().and_then(|all| all.iter().find(|item| item["id"] == artifact_id)).cloned().expect("listed");
    serde_json::from_value(summary).expect("a summary")
}

#[test]
fn a_figures_profile_and_export_dpi_come_back_after_a_restart() {
    let root = std::env::temp_dir().join(format!("designllm-figure-output-{}", designllm_contracts::new_id()));
    let mut host = Host::start();
    let (figure_id, _) = charted_figure(&mut host, "Chart");
    {
        let mut window = Window::open(&mut host, &figure_id);
        with_workspace(&mut window, &root);
        window.app.screen = Screen::Editor;
        window.step(&mut host, "figure.output_profile", json!({ "profile": "high_res_print" })).expect("the profile is sent");
        assert_eq!(window.app.view_ref().figure_export_dpi, 600);
    }
    // A new window: the view is new, the settings file is the same.
    let mut window = Window::open(&mut host, &figure_id);
    assert_eq!(window.app.view_ref().figure_export_dpi, 300, "without the settings a view starts at 300 DPI");
    with_workspace(&mut window, &root);
    window.app.screen = Screen::Editor;
    assert_eq!(window.app.view_ref().figure_output_profile.as_deref(), Some("high_res_print"));
    assert_eq!(window.app.view_ref().figure_export_dpi, 600);
    let request = window.app.export_request("test.png".into(), Some(&json!({ "format": "png", "target_path": "test.png" }))).expect("an export");
    assert_eq!(request.options.profile.dpi, 600, "the export after the restart is at the profile's DPI");

    // An export at another DPI is that export's: it is written at 150, and a restart brings back the profile's 600.
    std::fs::create_dir_all(&root).expect("the workspace");
    let target = root.join("figure.png").to_string_lossy().into_owned();
    window.step(&mut host, "file.export", json!({ "format": "png", "dpi": 150, "target_path": target })).expect("the export is asked for");
    assert_eq!(window.app.view_ref().figure_export_dpi, 150, "the session's view takes the export's DPI");
    let mut again = Window::open(&mut host, &figure_id);
    with_workspace(&mut again, &root);
    assert_eq!(again.app.view_ref().figure_export_dpi, 600, "the profile's DPI comes back, not the export's");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_new_plot_takes_type_size_and_strokes_from_the_plots_there_else_the_profile() {
    let mut host = Host::start();
    let (figure_id, draft) = charted_figure(&mut host, "Chart");
    let single = figure_output_profile("single_column").expect("profile");
    {
        let mut window = Window::open(&mut host, &figure_id);
        window.app.screen = Screen::Editor;
        window.step(&mut host, "figure.output_profile", json!({ "profile": "single_column" })).expect("the profile is sent");
    }
    // A later window (no chosen profile in its view) adds a plot: it matches the restyled one.
    let mut window = Window::open(&mut host, &figure_id);
    window.app.screen = Screen::Editor;
    window.app.csv_draft = Some(draft.clone());
    window.app.add_csv_data(true);
    window.settle(&mut host);
    let (doc, _) = host.figure(&figure_id);
    let new = &doc.panels.last().expect("the new plot").style;
    assert_eq!((new.fill.as_str(), new.font_size, new.stroke_width), (single.palette[0], single.font_size_emu, single.stroke_width_emu));

    // No plot yet: the chosen profile's, else the defaults.
    let empty = designllm_contracts::FigureDocument::new(Size::a4());
    let style_of = |operations: &[Operation]| operations.iter().find_map(|operation| match operation {
        Operation::AddPanel { panel, .. } => Some(panel.style.clone()),
        _ => None,
    }).expect("a plot panel");
    let (with_profile, _) = chart_change_with(&empty, &draft, true, Some("single_column")).expect("a chart");
    let style = style_of(&with_profile);
    assert_eq!((style.font_size, style.stroke_width), (single.font_size_emu, single.stroke_width_emu));
    let (plain, _) = chart_change_with(&empty, &draft, true, None).expect("a chart");
    let style = style_of(&plain);
    let defaults = designllm_contracts::Style::default();
    assert_eq!((style.fill.as_str(), style.font_size, style.stroke_width), ("#1F4E79", defaults.font_size, defaults.stroke_width));
}

#[test]
fn a_kept_legend_notice_waits_for_its_figure_when_the_reply_comes_after_a_switch() {
    let mut host = Host::start();
    let (figure_id, draft) = charted_figure(&mut host, "Chart");
    let (doc, _) = host.figure(&figure_id);
    let (operations, intents) = chart_change(&doc, &draft, true).expect("a second chart");
    host.apply(&figure_id, operations, intents).expect("the chart");
    // Two series per plot; the second plot starts at the first one's series-1 colour; the legend swatch is that colour.
    let (doc, _) = host.figure(&figure_id);
    let y_columns: Vec<String> = doc.datasets[0].columns.iter().skip(1).map(|column| column.id.clone()).collect();
    let mut operations = Vec::new();
    for panel in &doc.panels {
        let PanelBody::Plot(mut spec) = panel.body.clone() else { unreachable!() };
        spec.y_columns = y_columns.clone();
        operations.push(Operation::SetPlot { panel_id: panel.id.clone(), plot: spec });
    }
    host.apply(&figure_id, operations, BTreeSet::from([Intent::Data])).expect("two series per plot");
    let shared = series_colors("#1F4E79", 2)[1].clone();
    host.apply(&figure_id, vec![Operation::SetStyle {
        node_ids: vec![doc.panels[1].id.clone()],
        patch: designllm_contracts::StylePatch { fill: Some(shared.clone()), ..Default::default() },
    }], BTreeSet::from([Intent::Design])).expect("the second plot's colour");
    let legend = CommonLegend { items: vec![CommonLegendItem { label: "b".into(), color: shared }], placement: CommonLegendPlacement::Bottom, title: None };
    host.apply(&figure_id, vec![Operation::SetCommonLegend { expected: None, legend: Some(legend) }], BTreeSet::from([Intent::Content])).expect("a legend");
    let (other, _) = charted_figure(&mut host, "Other");

    let mut window = Window::open(&mut host, &figure_id);
    window.app.screen = Screen::Editor;
    // The profile is sent, then the person switches Figures before the host answers.
    window.app.invoke("figure.output_profile", json!({ "profile": "double_column" })).expect("the profile is sent");
    let other_summary = summary_of(&mut host, &other);
    window.app.activate(other_summary);
    window.settle(&mut host);
    assert!(window.message().is_none_or(|message| !message.contains("‘b’")), "not told on another Figure: {:?}", window.message());
    // Back on the Figure the notice is shown.
    let summary = summary_of(&mut host, &figure_id);
    window.app.activate(summary);
    window.settle(&mut host);
    let message = window.message().expect("the notice is shown on its Figure");
    assert!(message.contains("‘b’"), "{message}");
    assert_eq!(window.app.view_ref().figure_output_profile.as_deref(), Some("double_column"), "the profile reached its Figure");
}

#[test]
fn an_open_connector_menu_tells_nodes_with_long_alike_texts_apart() {
    let ctx = egui::Context::default();
    crate::theme::install(&ctx);
    let palette = crate::theme::LIGHT;
    let options = ["Encoder output A", "Encoder output B"];
    let frame = |events: Vec<Event>| {
        let mut input = screen(1.0, 600.0, 400.0);
        input.events = events;
        ctx.run(input, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                // The picker is narrow, as in the connector row: the closed control cuts the text, the menu does not.
                crate::widgets::select_menu_verbatim(ui, &palette, options[0], &options, Some(0), 90.0);
            });
        })
    };
    let closed = frame(Vec::new());
    let at = closed.shapes.iter().find_map(|clipped| match &clipped.shape {
        egui::epaint::Shape::Text(text) => Some(text.pos + text.galley.size() / 2.0),
        egui::epaint::Shape::Vec(shapes) => shapes.iter().find_map(|shape| match shape {
            egui::epaint::Shape::Text(text) => Some(text.pos + text.galley.size() / 2.0),
            _ => None,
        }),
        _ => None,
    }).unwrap_or(Pos2::new(40.0, 20.0));
    frame(vec![
        Event::PointerMoved(at),
        Event::PointerButton { pos: at, button: PointerButton::Primary, pressed: true, modifiers: Modifiers::NONE },
        Event::PointerButton { pos: at, button: PointerButton::Primary, pressed: false, modifiers: Modifiers::NONE },
    ]);
    let open = drawn_text(&frame(Vec::new()));
    for option in options {
        assert!(open.iter().any(|text| text == option), "the open menu reads {option:?} whole: {open:?}");
    }
}
