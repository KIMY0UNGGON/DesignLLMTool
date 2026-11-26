//! Series colours under an output profile (S17): every plot panel takes the profile's first colour so series `j` looks the
//! same in every panel, a common legend's swatches follow their series, a plot made after a profile was chosen starts in
//! it, and the data table shows imported column names as written.

use std::collections::BTreeSet;

use designllm_contracts::{
    figure_rules::figure_output_profile,
    AxisSpec, CommonLegend, CommonLegendItem, CommonLegendPlacement, DatasetRef, FigureDocument, FigurePanel, Intent,
    Operation, PanelBody, PlotSpec, Size, Style,
};
use eframe::egui;
use serde_json::json;

use super::{
    figure_profile::{profile_style_operations, series_colors},
    figure_tools::{chart_change, chart_change_with},
    test_support::*,
    Screen,
};
use crate::i18n::{self, Language};

fn axis(label: &str) -> AxisSpec {
    AxisSpec { label: label.into(), unit: Some("%".into()), scale: "linear".into(), minimum: None, maximum: None, tick_step: None }
}

fn plot_panel(id: &str, fill: &str, series: usize) -> FigurePanel {
    let y_columns = (0..series).map(|index| format!("y{index}")).collect();
    let plot = PlotSpec::new("line", DatasetRef { dataset_id: "d".into(), version_hash: "v".into() }, "x", y_columns, axis("x"), axis("y"));
    FigurePanel { id: id.into(), bounds: Default::default(), label: String::new(), body: PanelBody::Plot(plot), style: Style { fill: fill.into(), ..Style::default() } }
}

fn figure(panels: Vec<FigurePanel>, legend: Option<CommonLegend>) -> FigureDocument {
    let mut figure = FigureDocument::new(Size::a4());
    figure.panels = panels;
    figure.common_legend = legend;
    figure
}

/// The intents of the last change set the window sent.
fn last_intents(window: &Window) -> serde_json::Value {
    let (_, params, _) = window.calls.iter().rev().find(|(method, _, _)| method == "changes.apply").expect("a change was sent");
    params.get("allowed_intents").cloned().unwrap_or_else(|| panic!("no intents in {params}"))
}

fn fills(operations: &[Operation]) -> Vec<Option<String>> {
    operations.iter().filter_map(|operation| match operation {
        Operation::SetStyle { patch, .. } => Some(patch.fill.clone()),
        _ => None,
    }).collect()
}

#[test]
fn every_plot_panel_takes_the_profiles_first_colour_so_a_series_matches_across_panels() {
    let profile = figure_output_profile("double_column").expect("a profile");
    let doc = figure(vec![plot_panel("a", "#1F4E79", 2), plot_panel("b", "#C0504D", 2)], None);
    let operations = profile_style_operations(&doc, profile);
    assert_eq!(fills(&operations), vec![Some(profile.palette[0].to_owned()); 2], "both plots start at the profile's first colour");
    assert!(!operations.iter().any(|operation| matches!(operation, Operation::SetCommonLegend { .. })), "no legend, no legend change");
}

#[test]
fn a_common_legend_swatch_follows_its_series_and_an_unrelated_one_is_kept() {
    let profile = figure_output_profile("double_column").expect("a profile");
    let old_series_1 = series_colors("#1F4E79", 2)[1].clone();
    let legend = CommonLegend {
        items: vec![
            CommonLegendItem { label: "control".into(), color: old_series_1.to_ascii_lowercase() },
            CommonLegendItem { label: "note".into(), color: "#ABCDEF".into() },
        ],
        placement: CommonLegendPlacement::Bottom,
        title: None,
    };
    // Panel b's one series is a colour of no other series, so the swatch names series 1 of panel a alone.
    let doc = figure(vec![plot_panel("a", "#1F4E79", 2), plot_panel("b", "#123456", 1)], Some(legend.clone()));
    let operations = profile_style_operations(&doc, profile);
    let Some(Operation::SetCommonLegend { expected, legend: Some(new) }) = operations.iter().find(|operation| matches!(operation, Operation::SetCommonLegend { .. })) else {
        panic!("the legend is recoloured in the same batch: {operations:?}");
    };
    assert_eq!(expected.as_ref(), Some(&legend), "the change names the legend it replaces");
    assert_eq!(new.items[0].color, series_colors(profile.palette[0], 2)[1], "series 1's swatch takes series 1's new colour");
    assert_eq!(new.items[1].color, "#ABCDEF", "a swatch of no series keeps its colour");
    assert_eq!(new.items[0].label, "control");
}

/// The window's profile command against the real engine: two charts and a legend; the batch is accepted with the content
/// intent the legend needs, and without a legend the change stays a design change.
#[test]
fn the_profile_command_recolours_plots_and_legend_together_in_the_engine() {
    let mut host = Host::start();
    let mut draft = host.declared_draft("id,value\nA,12\nB,18\n", |_| {});
    draft.y_unit = "%".into();
    let figure_id = host.create("figure", "Chart");
    for _ in 0..2 {
        let (doc, _) = host.figure(&figure_id);
        let (operations, intents) = chart_change(&doc, &draft, true).expect("a chart");
        host.apply(&figure_id, operations, intents).expect("the chart");
    }
    let legend = CommonLegend {
        items: vec![
            CommonLegendItem { label: "value".into(), color: "#1F4E79".into() },
            CommonLegendItem { label: "other".into(), color: "#ABCDEF".into() },
        ],
        placement: CommonLegendPlacement::Bottom,
        title: None,
    };
    host.apply(&figure_id, vec![Operation::SetCommonLegend { expected: None, legend: Some(legend) }], BTreeSet::from([Intent::Content]))
        .expect("a legend");
    let mut window = Window::open(&mut host, &figure_id);
    window.app.screen = Screen::Editor;
    window.step(&mut host, "figure.output_profile", json!({ "profile": "double_column" })).expect("the profile is sent");
    assert_eq!(last_intents(&window), json!(["design", "content"]), "the legend's recolouring is a content change");
    let (doc, _) = host.figure(&figure_id);
    let first = figure_output_profile("double_column").expect("profile").palette[0];
    assert!(doc.panels.iter().all(|panel| panel.style.fill == first), "both plots take {first}");
    let items = &doc.common_legend.as_ref().expect("the legend").items;
    assert_eq!(items[0].color, first, "the swatch of series 0 follows it");
    assert_eq!(items[1].color, "#ABCDEF");
    // Without a legend the profile is a design change only.
    let plain = host.create("figure", "Plain");
    let (doc, _) = host.figure(&plain);
    let (operations, intents) = chart_change(&doc, &draft, true).expect("a chart");
    host.apply(&plain, operations, intents).expect("the chart");
    let mut window = Window::open(&mut host, &plain);
    window.app.screen = Screen::Editor;
    window.step(&mut host, "figure.output_profile", json!({ "profile": "double_column" })).expect("the profile is sent");
    assert_eq!(last_intents(&window), json!(["design"]));
    assert_eq!(host.figure(&plain).0.panels[0].style.fill, first);
}

#[test]
fn a_plot_made_after_a_profile_was_chosen_starts_at_the_profiles_first_colour() {
    let mut host = Host::start();
    let mut draft = host.declared_draft("id,value\nA,12\nB,18\n", |_| {});
    draft.y_unit = "%".into();
    let figure_id = host.create("figure", "Chart");
    let (doc, _) = host.figure(&figure_id);
    let fill_of = |operations: &[Operation]| operations.iter().find_map(|operation| match operation {
        Operation::AddPanel { panel, .. } => Some(panel.style.fill.clone()),
        _ => None,
    }).expect("a plot panel");
    let (without, _) = chart_change(&doc, &draft, true).expect("a chart");
    assert_eq!(fill_of(&without), "#1F4E79", "without a profile the engine's own first colour");
    let (with, _) = chart_change_with(&doc, &draft, true, Some("presentation_16_9")).expect("a chart");
    assert_eq!(fill_of(&with), figure_output_profile("presentation_16_9").expect("profile").palette[0]);

    // Through the window: the view's chosen profile is the one used.
    let mut window = Window::open(&mut host, &figure_id);
    window.app.screen = Screen::Editor;
    window.app.view().figure_output_profile = Some("presentation_16_9".into());
    window.app.csv_draft = Some(draft);
    window.app.add_csv_data(true);
    window.settle(&mut host);
    let (doc, _) = host.figure(&figure_id);
    assert_eq!(doc.panels.last().expect("the new plot").style.fill, figure_output_profile("presentation_16_9").expect("profile").palette[0]);
}

#[test]
fn the_english_data_table_shows_imported_column_names_as_written() {
    struct Restore;
    impl Drop for Restore {
        fn drop(&mut self) {
            i18n::set_language(Language::Korean);
        }
    }
    let _restore = Restore;
    let mut host = Host::start();
    let draft = host.declared_draft("제목,Title\nA,B\n", |_| {});
    let figure_id = host.create("figure", "Chart");
    let (doc, _) = host.figure(&figure_id);
    let (operations, intents) = chart_change(&doc, &draft, false).expect("the table");
    host.apply(&figure_id, operations, intents).expect("the table is kept");
    let (doc, _) = host.figure(&figure_id);
    let mut window = Window::open(&mut host, &figure_id);
    window.app.set_language(Language::English);
    let ctx = window.ctx.clone();
    crate::theme::install(&ctx);
    let frame = |window: &mut Window, events: Vec<egui::Event>| {
        let mut input = screen(1.0, 600.0, 900.0);
        input.events = events;
        let output = ctx.run(input, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| window.app.draw_figure_details(ui, &doc));
        });
        output
    };
    frame(&mut window, Vec::new());
    let closed = frame(&mut window, Vec::new());
    // The table's header line is the window's words, in English: its origin and row count.
    let header = format!("{} · 1 row", i18n::tr("실험 자료"));
    let at = {
        fn find(shape: &egui::epaint::Shape, label: &str) -> Option<egui::Pos2> {
            match shape {
                egui::epaint::Shape::Text(text) if text.galley.text() == label => Some(text.pos + text.galley.size() / 2.0),
                egui::epaint::Shape::Vec(shapes) => shapes.iter().find_map(|shape| find(shape, label)),
                _ => None,
            }
        }
        closed.shapes.iter().find_map(|clipped| find(&clipped.shape, &header))
            .unwrap_or_else(|| panic!("the table's header line {header:?} is drawn: {:?}", drawn_text(&closed)))
    };
    let titles_closed = drawn_text(&closed).iter().filter(|text| *text == "Title").count();
    frame(&mut window, vec![
        egui::Event::PointerMoved(at),
        primary_button(at, true),
        primary_button(at, false),
    ]);
    let mut open = frame(&mut window, Vec::new());
    for _ in 0..3 {
        open = frame(&mut window, Vec::new());
    }
    let drawn = drawn_text(&open);
    assert!(drawn.iter().any(|text| text == "제목"), "the column 제목 keeps its name: {drawn:?}");
    assert_eq!(drawn.iter().filter(|text| *text == "Title").count(), titles_closed + 1, "only the real Title column reads Title: {drawn:?}");
}
