//! Controls and commands follow what the engine of each kind takes (S16): a deck's engine refuses stacking order, layer
//! moves, page size, page and node names, opacity, corner radius and the text extras, so the window neither lists those
//! commands as enabled nor draws their controls on a deck. The person's own names are shown as written in English.

use designllm_contracts::{ArtifactBody, Intent, Operation, Size};
use eframe::egui;
use serde_json::json;

use super::{board::LeftTab, test_support::*, Screen};
use crate::i18n::{self, Language};

/// The commands only the canvas engine takes (every other engine's guard refuses their operations).
const CANVAS_ONLY: [&str; 9] = [
    "edit.order_forward", "edit.order_backward", "edit.order_front", "edit.order_back", "layer.move", "layer.move_to",
    "props.page_size", "props.rename_page", "edit.rename",
];

fn open(host: &mut Host, artifact: &str) -> Window {
    let mut window = Window::open(host, artifact);
    window.app.screen = Screen::Editor;
    window.draw(host, 1.0);
    window
}

/// Whether the window's published command list (what an agent reads) marks `id` enabled.
fn listed_enabled(window: &mut Window, host: &mut Host, id: &str) -> bool {
    window.app.publish_ui_if_due();
    window.settle(host);
    let listing = host.call("ui.command.list", json!({ "view_id": window.app.ui.view_id })).expect("the command list");
    listing["commands"].as_array().expect("commands").iter().find(|entry| entry["id"] == id).expect("listed")["enabled"]
        .as_bool()
        .expect("enabled flag")
}

/// The centre of the first drawn text that reads `label`.
fn text_at(window: &Window, label: &str) -> egui::Pos2 {
    fn walk(shape: &egui::epaint::Shape, label: &str) -> Option<egui::Pos2> {
        match shape {
            egui::epaint::Shape::Text(text) if text.galley.text() == label => Some(text.pos + text.galley.size() / 2.0),
            egui::epaint::Shape::Vec(shapes) => shapes.iter().find_map(|shape| walk(shape, label)),
            _ => None,
        }
    }
    window.shapes.iter().find_map(|clipped| walk(&clipped.shape, label)).unwrap_or_else(|| panic!("no {label:?} drawn"))
}

/// Adds a rectangle (or `command`'s node) and selects it, so the panel and the selection commands are about it.
fn add_selected(window: &mut Window, host: &mut Host, command: &str) {
    window.step(host, command, json!({})).expect("the node is added");
    let node = last_node(window);
    window.step(host, "select.node", json!({ "node_id": node })).expect("the node is selected");
    window.draw(host, 1.0);
}

fn drawn(window: &Window) -> Vec<String> {
    drawn_text(&egui::FullOutput { shapes: window.shapes.clone(), ..Default::default() })
}

fn last_node(window: &Window) -> String {
    match &window.app.artifact.as_ref().expect("open").body {
        ArtifactBody::Canvas(document) => document.pages[0].nodes.last().expect("a node").id.clone(),
        ArtifactBody::Slides(document) => document.pages[0].nodes.last().expect("a node").id.clone(),
        _ => panic!("a paged document"),
    }
}

#[test]
fn a_deck_does_not_list_the_commands_its_engine_refuses_and_the_engine_does_refuse_them() {
    let mut host = Host::start();
    let canvas = canvas_with_page(&mut host, "Board");
    let mut window = open(&mut host, &canvas);
    add_selected(&mut window, &mut host, "add.rect");
    for id in ["edit.order_forward", "edit.order_back", "layer.move", "edit.rename", "props.page_size", "props.rename_page"] {
        assert!(listed_enabled(&mut window, &mut host, id), "{id} is enabled on a canvas with a selected rectangle");
    }
    let deck = host.create("slides", "Deck");
    let mut window = open(&mut host, &deck);
    add_selected(&mut window, &mut host, "add.rect");
    for id in CANVAS_ONLY {
        assert!(!listed_enabled(&mut window, &mut host, id), "{id} is not listed as enabled on a deck");
        assert!(window.app.invoke(id, json!({})).is_err(), "{id} is refused on a deck before anything is sent");
    }
    // The scope tells the truth: the slides engine refuses the operations those commands send.
    let node = last_node(&window);
    let page_id = match &host.artifact(&deck).body {
        ArtifactBody::Slides(document) => document.pages[0].id.clone(),
        _ => unreachable!(),
    };
    let content: std::collections::BTreeSet<Intent> = [Intent::Content, Intent::Design].into_iter().collect();
    for operation in [
        Operation::SetZOrder { node_id: node.clone(), index: 0 },
        Operation::SetPageSize { page_id: page_id.clone(), size: Size::a4() },
        Operation::RenamePage { page_id, name: Some("Intro".into()) },
        Operation::SetNodeInfo { node_ids: vec![node], alt: None, name: Some("Box".into()), hidden: None },
    ] {
        let name = operation.name();
        assert!(host.apply(&deck, vec![operation], content.clone()).is_err(), "the slides engine refuses {name}");
    }
}

#[test]
fn a_slide_shape_shows_only_the_style_rows_the_slides_engine_takes() {
    let mut host = Host::start();
    let canvas = canvas_with_page(&mut host, "Board");
    let mut window = open(&mut host, &canvas);
    add_selected(&mut window, &mut host, "add.rect");
    let on_canvas = drawn(&window);
    for shown in ["불투명도", "모서리", "중간", "효과"] {
        assert!(on_canvas.iter().any(|text| text == shown), "a canvas rectangle shows {shown:?}: {on_canvas:?}");
    }

    let deck = host.create("slides", "Deck");
    let mut window = open(&mut host, &deck);
    add_selected(&mut window, &mut host, "add.rect");
    let on_slide = drawn(&window);
    // Opacity and corner radius are refused for slides; a slide rectangle takes a shadow, and slides align on the page.
    for hidden in ["불투명도", "모서리"] {
        assert!(!on_slide.iter().any(|text| text == hidden), "a slide rectangle has no {hidden:?} row: {on_slide:?}");
    }
    for shown in ["중간", "효과"] {
        assert!(on_slide.iter().any(|text| text == shown), "a slide rectangle shows {shown:?}: {on_slide:?}");
    }

    add_selected(&mut window, &mut host, "add.text");
    let text_box = drawn(&window);
    // A text box takes no shadow on a slide, nor line height, letter spacing or vertical alignment.
    for hidden in ["효과", "줄 높이", "자간", "세로 정렬"] {
        assert!(!text_box.iter().any(|text| text == hidden), "a slide text box has no {hidden:?} row: {text_box:?}");
    }
}

#[test]
fn a_deck_tells_its_slide_size_without_offering_to_change_it() {
    let mut host = Host::start();
    let deck = host.create("slides", "Deck");
    let mut window = open(&mut host, &deck);
    window.draw(&mut host, 1.0);
    let shown = drawn(&window);
    assert!(shown.iter().any(|text| text == "16:9"), "the size is told: {shown:?}");
    // A press on the row opens no size menu (its `사용자 크기` item) and no width field.
    let at = text_at(&window, "16:9");
    window.click(&mut host, 1.0, at, false);
    window.draw(&mut host, 1.0);
    let after = drawn(&window);
    assert!(!after.iter().any(|text| text == "사용자 크기" || text == "너비"), "{after:?}");
}

/// The centre of the text `label` in the left panel (x < 300, under the top bar): a layer row.
fn left_text(window: &Window, label: &str) -> egui::Pos2 {
    fn walk(shape: &egui::epaint::Shape, found: &mut Vec<(String, egui::Pos2)>) {
        match shape {
            egui::epaint::Shape::Text(text) => {
                let centre = text.pos + text.galley.size() / 2.0;
                if centre.x < 300.0 && centre.y > 60.0 {
                    found.push((text.galley.text().to_owned(), centre));
                }
            }
            egui::epaint::Shape::Vec(shapes) => shapes.iter().for_each(|shape| walk(shape, found)),
            _ => {}
        }
    }
    let mut found = Vec::new();
    window.shapes.iter().for_each(|clipped| walk(&clipped.shape, &mut found));
    found.into_iter().find(|(text, _)| text == label).map(|(_, at)| at).unwrap_or_else(|| panic!("no {label:?} row in the left panel"))
}

/// A double click on the row of a new rectangle: Canvas opens its name field; a deck does not, since the slides engine
/// refuses node names (SetNodeInfo).
fn double_click_newest_row(kind: &str) -> bool {
    let mut host = Host::start();
    let artifact = if kind == "canvas" { canvas_with_page(&mut host, "Board") } else { host.create(kind, "Deck") };
    let mut window = open(&mut host, &artifact);
    window.app.view().left_tab = LeftTab::Layers;
    window.step(&mut host, "add.rect", json!({})).expect("a rectangle");
    window.draw(&mut host, 1.0);
    let at = left_text(&window, "사각형");
    window.click(&mut host, 1.0, at, true);
    window.app.layer_rename.is_some()
}

#[test]
fn a_layer_row_opens_its_name_field_on_a_canvas_and_not_on_a_deck() {
    assert!(double_click_newest_row("canvas"), "a canvas layer row is renamed in place");
    assert!(!double_click_newest_row("slides"), "a deck's layer row opens no name field");
}

#[test]
fn english_mode_shows_a_data_column_and_a_component_page_by_the_names_people_gave() {
    struct Restore;
    impl Drop for Restore {
        fn drop(&mut self) {
            i18n::set_language(Language::Korean);
        }
    }
    let _restore = Restore;
    i18n::set_language(Language::English);
    // `저장` is also a word of the window ("Save"): as a name it stays as written.
    let ctx = egui::Context::default();
    crate::theme::install(&ctx);
    let palette = crate::theme::LIGHT;
    let output = ctx.run(screen(1.0, 400.0, 300.0), |ctx| {
        egui::CentralPanel::default().show(ctx, |ui| {
            crate::widgets::dropdown_verbatim(ui, &palette, "저장", None, 200.0);
            crate::widgets::menu_item_verbatim(ui, &palette, None, "저장", false);
            crate::widgets::labeled_button_verbatim(ui, &palette, None, "저장");
            crate::widgets::text_button_verbatim(ui, &palette, "저장", palette.ink);
            let mut checked = false;
            crate::widgets::checkbox_verbatim(ui, &palette, &mut checked, "저장");
            // The window's own word is translated next to them.
            crate::widgets::labeled_button(ui, &palette, None, "저장", false);
        });
    });
    let texts = drawn_text(&output);
    assert_eq!(texts.iter().filter(|text| *text == "저장").count(), 5, "{texts:?}");
    assert!(texts.iter().any(|text| text == "Save"), "{texts:?}");
    // A sentence keeps a title in a named or quoted placeholder as written.
    assert_eq!(i18n::tr("섹션: 저장 · 시작 페이지 2"), "Section: 저장 · start page 2");
}
