//! S19: a legend swatch that stands for different series in different plots keeps its colour and the window says so; the
//! CSV import dialog shows imported column names as written; every built-in template has a Korean and an English name.

use std::collections::BTreeSet;

use designllm_contracts::{
    figure_rules::figure_output_profile, AxisSpec, CommonLegend, CommonLegendItem, CommonLegendPlacement, DatasetRef,
    FigureDocument, FigurePanel, Intent, Operation, PanelBody, PlotSpec, Size, Style,
};
use serde_json::json;

use super::{
    figure_profile::{profile_change, series_colors},
    figure_tools::chart_change,
    state::{CsvStep, FigureForm},
    test_support::*,
    Screen,
};
use crate::i18n::{self, has_hangul, Language};

struct Restore;

impl Drop for Restore {
    fn drop(&mut self) {
        i18n::set_language(Language::Korean);
    }
}

fn axis() -> AxisSpec {
    AxisSpec { label: "v".into(), unit: Some("%".into()), scale: "linear".into(), minimum: None, maximum: None, tick_step: None }
}

fn plot(id: &str, fill: &str, series: usize) -> FigurePanel {
    let y_columns = (0..series).map(|index| format!("y{index}")).collect();
    FigurePanel {
        id: id.into(),
        bounds: Default::default(),
        label: String::new(),
        body: PanelBody::Plot(PlotSpec::new("line", DatasetRef { dataset_id: "d".into(), version_hash: "v".into() }, "x", y_columns, axis(), axis())),
        style: Style { fill: fill.into(), ..Style::default() },
    }
}

fn legend(items: &[(&str, &str)]) -> CommonLegend {
    CommonLegend {
        items: items.iter().map(|(label, color)| CommonLegendItem { label: (*label).into(), color: (*color).into() }).collect(),
        placement: CommonLegendPlacement::Bottom,
        title: None,
    }
}

#[test]
fn a_colour_two_series_share_follows_them_when_they_take_the_same_new_colour() {
    // In one seven-series plot from #1F4E79 the palette comes round: series 1 and 6 are both #C0504D, and both take the
    // profile's same colour after it, so the swatch is not ambiguous.
    let profile = figure_output_profile("double_column").expect("profile");
    let old = series_colors("#1F4E79", 7);
    assert!(old[1].eq_ignore_ascii_case(&old[6]), "{old:?}");
    let new = series_colors(profile.palette[0], 7);
    assert_eq!(new[1], new[6], "{new:?}");
    let mut doc = FigureDocument::new(Size::a4());
    doc.panels = vec![plot("a", "#1F4E79", 7)];
    doc.common_legend = Some(legend(&[("repeated", &old[1])]));
    let change = profile_change(&doc, profile);
    assert!(change.kept_legend_items.is_empty(), "{:?}", change.kept_legend_items);
    let recoloured = change.operations.iter().find_map(|operation| match operation {
        Operation::SetCommonLegend { legend: Some(legend), .. } => Some(legend.items[0].color.clone()),
        _ => None,
    });
    assert_eq!(recoloured.as_deref(), Some(new[1].as_str()));
}

#[test]
fn a_swatch_that_is_different_series_in_different_plots_keeps_its_colour_and_is_named() {
    let profile = figure_output_profile("double_column").expect("profile");
    // Panel A starts at #1F4E79, so its series 1 is the legacy palette's next colour; panel B starts at that colour.
    let a_series_1 = series_colors("#1F4E79", 2)[1].clone();
    let mut doc = FigureDocument::new(Size::a4());
    doc.panels = vec![plot("a", "#1F4E79", 2), plot("b", &a_series_1, 2)];
    doc.common_legend = Some(legend(&[("shared", &a_series_1), ("first", "#1F4E79")]));
    let change = profile_change(&doc, profile);
    assert_eq!(change.kept_legend_items, vec!["shared".to_owned()], "the ambiguous swatch is named");
    let Some(Operation::SetCommonLegend { legend: Some(new), .. }) = change.operations.iter().find(|operation| matches!(operation, Operation::SetCommonLegend { .. })) else {
        panic!("the unambiguous swatch is still recoloured: {:?}", change.operations);
    };
    assert_eq!(new.items[0].color, a_series_1, "series 1 of A and series 0 of B: the colour is kept");
    assert_eq!(new.items[1].color, profile.palette[0], "#1F4E79 is series 0 only: it follows the new first colour");
    assert_eq!(super::figure_profile::kept_legend_notice(&change.kept_legend_items).as_deref(),
        Some("범례 항목 ‘shared’은 패널마다 다른 계열의 색이라 그대로 두었습니다"));
}

#[test]
fn the_same_series_in_every_plot_is_not_ambiguous() {
    let profile = figure_output_profile("double_column").expect("profile");
    let mut doc = FigureDocument::new(Size::a4());
    doc.panels = vec![plot("a", "#1F4E79", 2), plot("b", "#1F4E79", 3)];
    let series_1 = series_colors("#1F4E79", 2)[1].clone();
    doc.common_legend = Some(legend(&[("control", &series_1)]));
    let change = profile_change(&doc, profile);
    assert!(change.kept_legend_items.is_empty());
    assert!(change.operations.iter().any(|operation| matches!(operation, Operation::SetCommonLegend { legend: Some(new), .. } if new.items[0].color == series_colors(profile.palette[0], 3)[1])));
}

#[test]
fn the_window_tells_which_legend_item_it_kept_after_the_profile_is_applied() {
    let mut host = Host::start();
    let mut draft = host.declared_draft("id,a,b\nA,12,3\nB,18,4\n", |_| {});
    draft.y_unit = "%".into();
    let figure_id = host.create("figure", "Chart");
    for _ in 0..2 {
        let (doc, _) = host.figure(&figure_id);
        let (operations, intents) = chart_change(&doc, &draft, true).expect("a chart");
        host.apply(&figure_id, operations, intents).expect("the chart");
    }
    // Both plots show both value columns; the second plot starts where the first one's series 1 is.
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
    host.apply(&figure_id, vec![Operation::SetCommonLegend { expected: None, legend: Some(legend(&[("b", &shared), ("a", "#1F4E79")])) }], BTreeSet::from([Intent::Content]))
        .expect("a legend");
    let mut window = Window::open(&mut host, &figure_id);
    window.app.screen = Screen::Editor;
    window.step(&mut host, "figure.output_profile", json!({ "profile": "double_column" })).expect("the profile is sent");
    let (doc, _) = host.figure(&figure_id);
    let items = doc.common_legend.expect("legend").items;
    assert_eq!(items[0].color, shared, "the ambiguous swatch keeps its colour");
    assert_eq!(items[1].color, figure_output_profile("double_column").expect("profile").palette[0], "the clear one follows");
    let message = window.message().expect("the window says what it kept");
    assert!(message.contains("‘b’"), "{message}");
}

/// The import dialog in English: its own words translated, the file's column names as written.
#[test]
fn the_english_csv_import_dialog_shows_the_files_column_names_as_written() {
    let _restore = Restore;
    let mut host = Host::start();
    let draft = host.declared_draft("제목,Title\n1,2\n3,4\n", |_| {});
    let figure_id = host.create("figure", "Chart");
    let mut window = Window::open(&mut host, &figure_id);
    window.app.screen = Screen::Editor;
    window.app.set_language(Language::English);
    window.app.figure_form = Some(FigureForm::Chart);
    let ctx = window.ctx.clone();
    let draw = |window: &mut Window| {
        let mut drawn = Vec::new();
        for _ in 0..2 {
            let output = ctx.run(screen(1.0, 1280.0, 800.0), |ctx| window.app.draw_figure_form(ctx));
            drawn = drawn_text(&output);
        }
        drawn
    };
    let mut columns_step = draft.clone();
    columns_step.step = CsvStep::Columns;
    window.app.csv_draft = Some(columns_step);
    let columns = draw(&mut window);
    assert!(columns.iter().any(|text| text == "제목"), "the column row keeps its name: {columns:?}");
    assert_eq!(columns.iter().filter(|text| *text == "Title").count(), 1, "{columns:?}");
    assert!(columns.iter().any(|text| text == "Cancel" || !has_hangul(text)), "the dialog's words are English: {columns:?}");

    let mut chart_step = draft;
    chart_step.step = CsvStep::Chart;
    chart_step.x_column = chart_step.dataset.columns[0].id.clone();
    chart_step.y_column = chart_step.dataset.columns[1].id.clone();
    window.app.csv_draft = Some(chart_step);
    let chart = draw(&mut window);
    assert!(chart.iter().any(|text| text == "제목"), "the X axis choice reads 제목: {chart:?}");
    assert_eq!(chart.iter().filter(|text| *text == "Title").count(), 1, "the Y axis choice alone reads Title: {chart:?}");
    let korean: Vec<&String> = chart.iter().filter(|text| has_hangul(text) && *text != "제목").collect();
    assert!(korean.is_empty(), "only the column name is Korean: {korean:?}");
}

#[test]
fn every_built_in_template_has_a_korean_and_an_english_name() {
    use designllm_contracts::EngineAdapter;
    let _restore = Restore;
    let mut ids: Vec<String> = designllm_canvas::CanvasEngine.templates().into_iter().map(|template| template.id).collect();
    ids.extend(designllm_canvas::WebEngine.templates().into_iter().map(|template| template.id));
    ids.sort();
    ids.dedup();
    assert!(ids.contains(&"webgl_animation".to_owned()), "{ids:?}");
    for id in ids {
        let korean = super::home::built_in_template_name(&id).unwrap_or_else(|| panic!("{id} has no name in the template list"));
        assert!(has_hangul(korean), "{id}: {korean:?} is the Korean name");
        assert_eq!(super::state::home_template_title(&id), korean, "{id}: one name for the tile and the created project");
        i18n::set_language(Language::English);
        let english = i18n::tr(korean).into_owned();
        i18n::set_language(Language::Korean);
        assert!(!has_hangul(&english), "{id}: {korean:?} has the English {english:?}");
    }
}
