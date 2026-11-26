//! S18: the profile command tells agents every intent it may need, a new plot follows the colour the figure's plots already
//! have (which outlives a restart), a legend swatch is matched as a colour, and a built-in template tile creates the project
//! under the name it shows.

use std::collections::BTreeSet;

use designllm_contracts::{
    figure_rules::figure_output_profile, ArtifactKind, AxisSpec, CommonLegend, CommonLegendItem, CommonLegendPlacement,
    DatasetRef, FigureDocument, FigurePanel, Intent, Operation, PanelBody, PlotSpec, Size, Style, TemplateInfo,
};
use eframe::egui::{self, Event, Modifiers, PointerButton, Pos2, RawInput, Rect, Vec2, ViewportId};
use serde_json::{json, Value};

use super::{
    figure_profile::{profile_style_operations, series_colors},
    figure_tools::{chart_change, chart_change_with},
    home::HomeTab,
    test_support::*,
    Screen,
};
use crate::i18n::{self, Language};

struct Restore;

impl Drop for Restore {
    fn drop(&mut self) {
        i18n::set_language(Language::Korean);
    }
}

fn legend(items: &[(&str, &str)]) -> CommonLegend {
    CommonLegend {
        items: items.iter().map(|(label, color)| CommonLegendItem { label: (*label).into(), color: (*color).into() }).collect(),
        placement: CommonLegendPlacement::Bottom,
        title: None,
    }
}

/// A figure with one chart over a two-row table (the GUI's own chart), and the table's draft for more charts.
fn charted_figure(host: &mut Host) -> (String, super::CsvImportDraft) {
    let mut draft = host.declared_draft("id,value\nA,12\nB,18\n", |_| {});
    draft.y_unit = "%".into();
    let figure_id = host.create("figure", "Chart");
    let (doc, _) = host.figure(&figure_id);
    let (operations, intents) = chart_change(&doc, &draft, true).expect("a chart");
    host.apply(&figure_id, operations, intents).expect("the chart");
    (figure_id, draft)
}

#[test]
fn an_agent_granting_exactly_the_advertised_intents_can_apply_a_profile_that_recolours_the_legend() {
    let mut host = Host::start();
    let (figure_id, _) = charted_figure(&mut host);
    host.apply(&figure_id, vec![Operation::SetCommonLegend { expected: None, legend: Some(legend(&[("value", "#1F4E79")])) }], BTreeSet::from([Intent::Content]))
        .expect("a legend");
    let mut window = Window::open(&mut host, &figure_id);
    window.app.publish_ui_if_due();
    window.settle(&mut host);
    let listing = host.call("ui.command.list", json!({ "view_id": window.app.ui.view_id })).expect("the command list");
    let entry = listing["commands"].as_array().expect("commands").iter().find(|entry| entry["id"] == "figure.output_profile").expect("listed").clone();
    let advertised: Vec<String> = entry["intents"].as_array().expect("intents").iter().map(|intent| intent.as_str().expect("an intent").to_owned()).collect();
    assert!(advertised.contains(&"content".to_owned()) && advertised.contains(&"design".to_owned()), "{entry}");
    assert_eq!(entry["enabled"], true, "{entry} / screen {:?} active {:?}", window.app.screen, window.app.active.as_ref().map(|active| active.kind));
    let intents: Vec<&str> = advertised.iter().map(String::as_str).collect();
    let run = window.agent_run_granted(&mut host, "figure.output_profile", json!({ "profile": "double_column" }), "change", &intents);
    assert_eq!(run["status"], "completed", "{run}");
    let first = figure_output_profile("double_column").expect("profile").palette[0];
    let (doc, _) = host.figure(&figure_id);
    assert_eq!(doc.common_legend.expect("the legend").items[0].color, first, "the swatch followed its series");
    assert!(crate::commands::core_calls("figure.output_profile").expect("declared").contains(&"changes.apply:set_common_legend"));
}

#[test]
fn without_a_common_legend_the_profile_asks_for_the_design_intent_only() {
    let mut host = Host::start();
    let (figure_id, _) = charted_figure(&mut host);
    let mut window = Window::open(&mut host, &figure_id);
    window.app.publish_ui_if_due();
    window.settle(&mut host);
    let listing = host.call("ui.command.list", json!({ "view_id": window.app.ui.view_id })).expect("the command list");
    let entry = listing["commands"].as_array().expect("commands").iter().find(|entry| entry["id"] == "figure.output_profile").expect("listed").clone();
    assert_eq!(entry["intents"], json!(["design"]), "{entry}");
    let run = window.agent_run_granted(&mut host, "figure.output_profile", json!({ "profile": "double_column" }), "change", &["design"]);
    assert_eq!(run["status"], "completed", "{run}");
    // A legend added later makes the list ask for content too.
    host.apply(&figure_id, vec![Operation::SetCommonLegend { expected: None, legend: Some(legend(&[("value", "#0072B2")])) }], BTreeSet::from([Intent::Content]))
        .expect("a legend");
    window.app.refresh_active();
    window.settle(&mut host);
    window.app.publish_ui_if_due();
    window.settle(&mut host);
    let listing = host.call("ui.command.list", json!({ "view_id": window.app.ui.view_id })).expect("the command list");
    let entry = listing["commands"].as_array().expect("commands").iter().find(|entry| entry["id"] == "figure.output_profile").expect("listed").clone();
    assert_eq!(entry["intents"], json!(["design", "content"]), "{entry}");
}

#[test]
fn a_new_plot_starts_at_the_fill_of_the_figures_plots_after_a_restart() {
    let mut host = Host::start();
    let (figure_id, draft) = charted_figure(&mut host);
    // The person applies the print profile; the window that chose it then closes.
    {
        let mut window = Window::open(&mut host, &figure_id);
        window.app.screen = Screen::Editor;
        window.step(&mut host, "figure.output_profile", json!({ "profile": "high_res_print" })).expect("the profile is sent");
    }
    let print = figure_output_profile("high_res_print").expect("profile").palette[0];
    assert_eq!(host.figure(&figure_id).0.panels[0].style.fill, print);
    // A new window has no chosen profile in its view; the new plot still takes the figure's plot colour.
    let mut window = Window::open(&mut host, &figure_id);
    window.app.screen = Screen::Editor;
    assert!(window.app.view_ref().figure_output_profile.is_none(), "the view's choice does not outlive the window");
    window.app.csv_draft = Some(draft.clone());
    window.app.add_csv_data(true);
    window.settle(&mut host);
    let (doc, _) = host.figure(&figure_id);
    assert_eq!(doc.panels.len(), 2);
    assert_eq!(doc.panels[1].style.fill, print, "the new plot matches the existing ones");

    // With no plot yet: the chosen profile's first colour, else the engine's own.
    let empty = FigureDocument::new(Size::a4());
    let fill_of = |operations: &[Operation]| operations.iter().find_map(|operation| match operation {
        Operation::AddPanel { panel, .. } => Some(panel.style.fill.clone()),
        _ => None,
    }).expect("a plot panel");
    let (with_profile, _) = chart_change_with(&empty, &draft, true, Some("presentation_16_9")).expect("a chart");
    assert_eq!(fill_of(&with_profile), figure_output_profile("presentation_16_9").expect("profile").palette[0]);
    let (plain, _) = chart_change_with(&empty, &draft, true, None).expect("a chart");
    assert_eq!(fill_of(&plain), "#1F4E79");
    // An existing plot wins over the view's choice.
    let mut doc_with_plot = empty.clone();
    doc_with_plot.panels.push(FigurePanel {
        id: "p".into(),
        bounds: Default::default(),
        label: "(a)".into(),
        body: PanelBody::Plot(PlotSpec::new("line", DatasetRef { dataset_id: "d".into(), version_hash: "v".into() }, "x", vec!["y".into()], axis(), axis())),
        style: Style { fill: "#123456".into(), ..Style::default() },
    });
    let (followed, _) = chart_change_with(&doc_with_plot, &draft, true, Some("presentation_16_9")).expect("a chart");
    assert_eq!(fill_of(&followed), "#123456");
}

fn axis() -> AxisSpec {
    AxisSpec { label: "v".into(), unit: Some("%".into()), scale: "linear".into(), minimum: None, maximum: None, tick_step: None }
}

#[test]
fn a_legend_swatch_matches_its_series_as_a_colour_whatever_its_case_or_opaque_alpha() {
    let profile = figure_output_profile("double_column").expect("profile");
    let mut doc = FigureDocument::new(Size::a4());
    doc.panels.push(FigurePanel {
        id: "p".into(),
        bounds: Default::default(),
        label: "(a)".into(),
        body: PanelBody::Plot(PlotSpec::new("line", DatasetRef { dataset_id: "d".into(), version_hash: "v".into() }, "x", vec!["a".into(), "b".into()], axis(), axis())),
        style: Style { fill: "#1F4E79".into(), ..Style::default() },
    });
    let series_1 = series_colors("#1F4E79", 2)[1].to_ascii_lowercase();
    doc.common_legend = Some(legend(&[("a", "#1F4E79FF"), ("b", &format!("{series_1}ff")), ("c", "#1F4E7980")]));
    let operations = profile_style_operations(&doc, profile);
    let Some(Operation::SetCommonLegend { legend: Some(new), .. }) = operations.iter().find(|operation| matches!(operation, Operation::SetCommonLegend { .. })) else {
        panic!("the legend is recoloured: {operations:?}");
    };
    let expected = series_colors(profile.palette[0], 2);
    assert_eq!(new.items[0].color, expected[0], "#1F4E79FF is the fill's colour");
    assert_eq!(new.items[1].color, expected[1], "an opaque lowercase swatch is series 1");
    assert_eq!(new.items[2].color, "#1F4E7980", "a half-transparent swatch is another colour and is kept");
}

fn home_frame(events: Vec<Event>) -> RawInput {
    let mut input = RawInput { screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(1440.0, 900.0))), ..Default::default() };
    input.viewports.get_mut(&ViewportId::ROOT).expect("root viewport").native_pixels_per_point = Some(1.0);
    input.events = events;
    input
}

fn text_center(output: &egui::FullOutput, expected: &str) -> Pos2 {
    fn find(shape: &egui::epaint::Shape, expected: &str) -> Option<Pos2> {
        match shape {
            egui::epaint::Shape::Text(text) if text.galley.text() == expected => Some(text.pos + text.galley.size() / 2.0),
            egui::epaint::Shape::Vec(shapes) => shapes.iter().find_map(|shape| find(shape, expected)),
            _ => None,
        }
    }
    output.shapes.iter().find_map(|clipped| find(&clipped.shape, expected))
        .unwrap_or_else(|| panic!("no {expected:?} drawn: {:?}", drawn_text(output)))
}

/// The home screen's template tab in `language` with the built-in mobile and WebGL templates and one the host names
/// itself (`team_board`), drawn: the window, its context and the last frame.
fn template_tab(language: Language) -> (Harness, egui::Context, egui::FullOutput) {
    let mut harness = Harness::new();
    harness.app.set_language(language);
    harness.app.home.tab = HomeTab::Templates;
    let info = |id: &str, kind: ArtifactKind, title: &str| TemplateInfo { id: id.into(), kind, title: title.into(), preset: None, size: None };
    harness.app.home.feeds.templates = vec![
        info("mobile_app", ArtifactKind::Canvas, "모바일 앱"),
        info("webgl_animation", ArtifactKind::WebCode, "WebGL animation"),
        info("team_board", ArtifactKind::Canvas, "우리 팀 보드"),
    ];
    let ctx = harness.ctx.clone();
    crate::theme::install(&ctx);
    let _ = ctx.run(home_frame(Vec::new()), |ctx| harness.app.draw_home(ctx));
    let _ = ctx.run(home_frame(Vec::new()), |ctx| harness.app.draw_home(ctx));
    let shown = ctx.run(home_frame(Vec::new()), |ctx| harness.app.draw_home(ctx));
    (harness, ctx, shown)
}

/// Clicks the template tile that reads `shown` and returns the title of the project the window asks for.
fn created_title(language: Language, shown: &str) -> String {
    let _restore = Restore;
    let (mut harness, ctx, shown_frame) = template_tab(language);
    let at = text_center(&shown_frame, shown);
    let _ = harness.sent();
    let _ = ctx.run(home_frame(vec![
        Event::PointerMoved(at),
        Event::PointerButton { pos: at, button: PointerButton::Primary, pressed: true, modifiers: Modifiers::NONE },
        Event::PointerButton { pos: at, button: PointerButton::Primary, pressed: false, modifiers: Modifiers::NONE },
    ]), |ctx| harness.app.draw_home(ctx));
    let requests = harness.sent();
    let (_, _, params): &(u64, String, Value) = requests.iter().find(|(_, method, _)| method == "artifact.create")
        .unwrap_or_else(|| panic!("the project is asked for: sent {:?}, notice {:?}", requests.iter().map(|request| &request.1).collect::<Vec<_>>(), harness.message()));
    params.pointer("/options/title").or_else(|| params.get("title")).and_then(Value::as_str).expect("a title").to_owned()
}

#[test]
fn a_built_in_template_tile_creates_the_project_under_the_name_it_shows() {
    let english = {
        let _restore = Restore;
        i18n::set_language(Language::English);
        i18n::tr("모바일 앱 화면").into_owned()
    };
    assert_ne!(english, "모바일 앱 화면", "the built-in name has its English");
    assert_eq!(created_title(Language::English, &english), english);
    assert_eq!(created_title(Language::Korean, "모바일 앱 화면"), "모바일 앱 화면");
    // The web engine's built-in template has its names too, in both languages.
    let webgl = {
        let _restore = Restore;
        i18n::set_language(Language::English);
        i18n::tr("WebGL 애니메이션").into_owned()
    };
    assert_eq!(created_title(Language::English, &webgl), webgl);
    assert_eq!(created_title(Language::Korean, "WebGL 애니메이션"), "WebGL 애니메이션");
    // A template outside the built-in list shows the host's title as written, in both languages.
    for language in [Language::English, Language::Korean] {
        let _restore = Restore;
        let (_, _, shown) = template_tab(language);
        text_center(&shown, "우리 팀 보드");
    }
}
