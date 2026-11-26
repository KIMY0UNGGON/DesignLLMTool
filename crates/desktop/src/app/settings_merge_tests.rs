//! S22: two windows on one workspace keep each other's settings, a failed settings write is tried again, a kept-legend
//! notice waits while the Figure's editor is off screen (Home included), a first plot without a fill still gives its sizes,
//! and a menu row of several lines is as high as its lines.

use std::collections::BTreeSet;

use designllm_contracts::{
    CommonLegend, CommonLegendItem, CommonLegendPlacement, FigureDocument, FigurePanel, Intent, Operation, PanelBody, PlotSpec,
    Size, Style,
};
use eframe::egui::{self, Event, Modifiers, PointerButton};
use serde_json::json;

use super::{
    figure_profile::series_colors,
    figure_tools::{chart_change, chart_change_with},
    settings::{load, settings_path, FigureOutput},
    test_support::*,
    DesktopApp, Screen,
};
use crate::{i18n::Language, rpc::RpcWorker};

fn app_on(root: &std::path::Path) -> DesktopApp {
    let (rpc, _replies, _work) = RpcWorker::detached();
    DesktopApp::new(rpc, "Workspace".into(), None, None).with_workspace(root)
}

/// Sets the output choice of Figure `id` in `app`'s view and keeps it, as the export popover does.
fn choose(app: &mut DesktopApp, id: &str, profile: Option<&str>, dpi: u32) {
    let view = app.editor_views.entry(id.to_owned()).or_default();
    view.figure_output_profile = profile.map(str::to_owned);
    view.figure_export_dpi = dpi;
    app.remember_figure_output(id);
}

#[test]
fn two_windows_on_one_workspace_keep_each_others_settings() {
    struct Restore;
    impl Drop for Restore {
        fn drop(&mut self) {
            crate::i18n::set_language(Language::Korean);
        }
    }
    let _restore = Restore;
    let root = std::env::temp_dir().join(format!("designllm-settings-merge-{}", designllm_contracts::new_id()));
    // Both windows start from the same (empty) file.
    let mut first = app_on(&root);
    let mut second = app_on(&root);
    choose(&mut first, "fig-1", Some("high_res_print"), 600);
    first.home.starred.insert("project-1".into());
    first.save_star("project-1");
    // The second window, holding its older snapshot, changes only its language, a figure of its own and a star.
    second.set_language(Language::English);
    choose(&mut second, "fig-2", None, 150);
    second.home.starred.insert("project-2".into());
    second.save_star("project-2");
    let saved = load(&settings_path(&root));
    assert_eq!(saved.figure_outputs.get("fig-1"), Some(&FigureOutput { profile: Some("high_res_print".into()), dpi: 600 }), "window 1's figure is kept");
    assert_eq!(saved.figure_outputs.get("fig-2"), Some(&FigureOutput { profile: None, dpi: 150 }));
    assert_eq!(saved.language, "en");
    assert_eq!(saved.starred, ["project-1".to_owned(), "project-2".to_owned()].into_iter().collect());
    // The first window unstars its project and changes its runner: the second window's choices stay.
    first.home.starred.remove("project-1");
    first.save_star("project-1");
    first.ai_effort = "low".into();
    first.save_runner();
    let saved = load(&settings_path(&root));
    assert_eq!(saved.starred, ["project-2".to_owned()].into_iter().collect());
    assert_eq!((saved.language.as_str(), saved.effort.as_str()), ("en", "low"));
    assert!(saved.figure_outputs.contains_key("fig-2"));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_failed_settings_write_is_tried_again_by_the_same_choice() {
    let root = std::env::temp_dir().join(format!("designllm-settings-retry-{}", designllm_contracts::new_id()));
    let mut app = app_on(&root);
    // The settings folder cannot be made: a file stands where it would be.
    std::fs::create_dir_all(&root).expect("the workspace");
    std::fs::write(root.join(".gui"), "not a folder").expect("a file in the way");
    choose(&mut app, "fig", Some("single_column"), 300);
    assert!(app.settings_unsaved(), "the failed write leaves the choice unsaved");
    assert!(app.notices.current(std::time::Instant::now()).is_some_and(|notice| notice.text.contains("화면 설정을 저장하지 못했습니다")));
    // The folder can be made again; the same choice is written now.
    std::fs::remove_file(root.join(".gui")).expect("the file goes");
    choose(&mut app, "fig", Some("single_column"), 300);
    assert!(!app.settings_unsaved());
    assert_eq!(load(&settings_path(&root)).figure_outputs.get("fig"), Some(&FigureOutput { profile: Some("single_column".into()), dpi: 300 }));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_kept_legend_notice_waits_while_the_window_is_on_the_home_screen() {
    let mut host = Host::start();
    let mut draft = host.declared_draft("id,a,b\nA,12,3\nB,18,4\n", |_| {});
    draft.y_unit = "%".into();
    let figure_id = host.create("figure", "Chart");
    for _ in 0..2 {
        let (doc, _) = host.figure(&figure_id);
        let (operations, intents) = chart_change(&doc, &draft, true).expect("a chart");
        host.apply(&figure_id, operations, intents).expect("the chart");
    }
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

    let mut window = Window::open(&mut host, &figure_id);
    window.app.screen = Screen::Editor;
    window.app.invoke("figure.output_profile", json!({ "profile": "double_column" })).expect("the profile is sent");
    // Home before the answer: the Figure stays the active document, but its editor is off screen.
    window.app.invoke("nav.home", json!({})).expect("home");
    window.settle(&mut host);
    assert!(window.message().is_none_or(|message| !message.contains("‘b’")), "not told on the home screen: {:?}", window.message());
    window.app.invoke("nav.open_project", json!({ "artifact_id": figure_id })).expect("the Figure opens again");
    window.settle(&mut host);
    let message = window.message().expect("the notice is shown when the Figure is open again");
    assert!(message.contains("‘b’"), "{message}");
}

#[test]
fn a_first_plot_without_a_fill_still_gives_its_type_size_and_stroke() {
    let mut host = Host::start();
    let mut draft = host.declared_draft("id,value\nA,12\nB,18\n", |_| {});
    draft.y_unit = "%".into();
    let mut doc = FigureDocument::new(Size::a4());
    let axis = || designllm_contracts::AxisSpec { label: "v".into(), unit: Some("%".into()), scale: "linear".into(), minimum: None, maximum: None, tick_step: None };
    doc.panels.push(FigurePanel {
        id: "p".into(),
        bounds: Default::default(),
        label: "(a)".into(),
        body: PanelBody::Plot(PlotSpec::new("line", designllm_contracts::DatasetRef { dataset_id: "d".into(), version_hash: "v".into() }, "x", vec!["y".into()], axis(), axis())),
        style: Style { fill: String::new(), font_size: 101_600, stroke_width: 9_525, ..Style::default() },
    });
    let style_of = |profile: Option<&str>| {
        let (operations, _) = chart_change_with(&doc, &draft, true, profile).expect("a chart");
        operations.iter().find_map(|operation| match operation {
            Operation::AddPanel { panel, .. } => Some(panel.style.clone()),
            _ => None,
        }).expect("a plot panel")
    };
    let with_profile = style_of(Some("presentation_16_9"));
    assert_eq!((with_profile.font_size, with_profile.stroke_width), (101_600, 9_525), "the first plot's sizes");
    assert_eq!(with_profile.fill, designllm_contracts::figure_rules::figure_output_profile("presentation_16_9").expect("profile").palette[0], "the colour falls back alone");
    let plain = style_of(None);
    assert_eq!((plain.fill.as_str(), plain.font_size, plain.stroke_width), ("#1F4E79", 101_600, 9_525));
}

#[test]
fn a_menu_row_of_several_lines_is_as_high_as_its_lines() {
    let ctx = egui::Context::default();
    crate::theme::install(&ctx);
    let palette = crate::theme::LIGHT;
    let options = ["Multi-head\nattention", "Feed\nforward\nnetwork", "Norm"];
    let frame = |events: Vec<Event>| {
        let mut input = screen(1.0, 600.0, 600.0);
        input.events = events;
        ctx.run(input, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                crate::widgets::select_menu_verbatim(ui, &palette, "Norm", &options, Some(2), 160.0);
            });
        })
    };
    frame(Vec::new());
    let at = egui::Pos2::new(40.0, 22.0);
    frame(vec![
        Event::PointerMoved(at),
        Event::PointerButton { pos: at, button: PointerButton::Primary, pressed: true, modifiers: Modifiers::NONE },
        Event::PointerButton { pos: at, button: PointerButton::Primary, pressed: false, modifiers: Modifiers::NONE },
    ]);
    let open = frame(Vec::new());
    // The drawn rectangles of the three labels: none may overlap the next.
    let mut boxes: Vec<(String, egui::Rect)> = Vec::new();
    fn walk(shape: &egui::epaint::Shape, options: &[&str], out: &mut Vec<(String, egui::Rect)>) {
        match shape {
            egui::epaint::Shape::Text(text) if options.contains(&text.galley.text()) => {
                out.push((text.galley.text().to_owned(), egui::Rect::from_min_size(text.pos, text.galley.size())));
            }
            egui::epaint::Shape::Vec(shapes) => shapes.iter().for_each(|shape| walk(shape, options, out)),
            _ => {}
        }
    }
    open.shapes.iter().for_each(|clipped| walk(&clipped.shape, &options, &mut boxes));
    // The menu's rows (the closed control also draws "Norm"; the menu's are the three in a column).
    boxes.sort_by(|a, b| a.1.top().total_cmp(&b.1.top()));
    let menu: Vec<&(String, egui::Rect)> = boxes.iter().filter(|(text, _)| text != "Norm").collect();
    assert_eq!(menu.len(), 2, "both multi-line labels are drawn whole: {boxes:?}");
    assert!(menu[0].1.bottom() <= menu[1].1.top(), "the first label ends before the second starts: {boxes:?}");
    let norm_in_menu = boxes.iter().filter(|(text, _)| text == "Norm").map(|(_, rect)| *rect).max_by(|a, b| a.top().total_cmp(&b.top())).expect("Norm in the menu");
    assert!(menu[1].1.bottom() <= norm_in_menu.top(), "the second label ends before the row under it: {boxes:?}");
}
