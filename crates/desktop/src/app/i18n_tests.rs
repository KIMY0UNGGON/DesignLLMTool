//! The UI language (i18n.rs): the selector on the home screen, the switch on the next frame, the setting kept across a
//! restart, and no Korean left on the key screens in English.

use eframe::egui::{self, Context, Event, Modifiers, PointerButton, Pos2, RawInput, Rect, Vec2, ViewportId};
use serde_json::json;

use super::{board::LeftTab, test_support::*, DesktopApp, Screen};
use crate::{
    i18n::{self, has_hangul, Language},
    rpc::RpcWorker,
    theme,
};

/// Puts the test's thread back in Korean however the test ends.
struct Restore;

impl Drop for Restore {
    fn drop(&mut self) {
        i18n::set_language(Language::Korean);
    }
}

fn app(workspace: &str) -> DesktopApp {
    let (rpc, _replies, _work) = RpcWorker::detached();
    DesktopApp::new(rpc, workspace.into(), None, None)
}

fn frame(events: Vec<Event>) -> RawInput {
    let mut input = RawInput { screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(1440.0, 900.0))), ..Default::default() };
    input.viewports.get_mut(&ViewportId::ROOT).expect("root viewport").native_pixels_per_point = Some(1.0);
    input.events = events;
    input
}

fn draw_home(ctx: &Context, app: &mut DesktopApp, events: Vec<Event>) -> egui::FullOutput {
    ctx.run(frame(events), |ctx| app.draw_home(ctx))
}

fn click(at: Pos2) -> Vec<Event> {
    vec![
        Event::PointerMoved(at),
        Event::PointerButton { pos: at, button: PointerButton::Primary, pressed: true, modifiers: Modifiers::NONE },
        Event::PointerButton { pos: at, button: PointerButton::Primary, pressed: false, modifiers: Modifiers::NONE },
    ]
}

/// The centre of the first text that reads `expected`.
fn text_center(output: &egui::FullOutput, expected: &str) -> Pos2 {
    fn find(shape: &egui::epaint::Shape, expected: &str) -> Option<Pos2> {
        match shape {
            egui::epaint::Shape::Text(text) if text.galley.text() == expected => Some(text.pos + text.galley.size() / 2.0),
            egui::epaint::Shape::Vec(shapes) => shapes.iter().find_map(|shape| find(shape, expected)),
            _ => None,
        }
    }
    output.shapes.iter().find_map(|clipped| find(&clipped.shape, expected))
        .unwrap_or_else(|| panic!("no {expected:?} drawn; drawn: {:?}", drawn_text(output)))
}

/// The drawn texts with Korean in them, but the language selector's own `한국어`.
fn korean(output: &egui::FullOutput) -> Vec<String> {
    drawn_text(output).into_iter().filter(|text| has_hangul(text) && text != Language::Korean.native_name()).collect()
}

#[test]
fn the_home_screen_draws_in_the_new_language_on_the_next_frame_and_back() {
    let _restore = Restore;
    let mut app = app("Workspace");
    let ctx = Context::default();
    theme::install(&ctx);
    draw_home(&ctx, &mut app, Vec::new());
    let before = drawn_text(&draw_home(&ctx, &mut app, Vec::new()));
    for shown in ["만들기", "빈 화면", "논문 그림", "첨부"] {
        assert!(before.iter().any(|text| text == shown), "Korean home draws {shown:?}: {before:?}");
    }

    app.set_language(Language::English);
    let english = drawn_text(&draw_home(&ctx, &mut app, Vec::new()));
    for (korean, english_text) in [("만들기", "Create"), ("빈 화면", "Blank"), ("첨부", "Attach")] {
        assert_eq!(i18n::tr(korean), english_text, "the table's English of {korean:?}");
        assert!(english.iter().any(|text| text == english_text), "English home draws {english_text:?}: {english:?}");
        assert!(!english.iter().any(|text| text == korean), "English home still draws {korean:?}");
    }

    app.set_language(Language::Korean);
    let after = drawn_text(&draw_home(&ctx, &mut app, Vec::new()));
    assert_eq!(after, before, "back in Korean, the home screen draws exactly what it drew before");
}

#[test]
fn the_language_is_chosen_from_the_workspace_menu_and_kept_across_a_restart() {
    let _restore = Restore;
    let root = std::env::temp_dir().join(format!("designllm-gui-language-{}", designllm_contracts::new_id()));
    let mut app = app("Workspace").with_workspace(&root);
    assert_eq!(app.language, Language::Korean, "a workspace without the setting opens in Korean");
    let ctx = Context::default();
    theme::install(&ctx);
    let home = draw_home(&ctx, &mut app, Vec::new());
    draw_home(&ctx, &mut app, click(text_center(&home, "Workspace")));
    let menu = draw_home(&ctx, &mut app, Vec::new());
    // Each language is offered in its own name.
    text_center(&menu, "한국어");
    draw_home(&ctx, &mut app, click(text_center(&menu, "English")));
    assert_eq!(app.language, Language::English);
    let switched = draw_home(&ctx, &mut app, Vec::new());
    assert!(korean(&switched).is_empty(), "the next frame is English: {:?}", korean(&switched));
    let saved: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(root.join(".gui/settings.json")).expect("the setting is written")).expect("JSON");
    assert_eq!(saved["language"], "en");

    // A restart reads it back: the new window draws in English from its first frame.
    i18n::set_language(Language::Korean);
    let mut restarted = self::app("Workspace").with_workspace(&root);
    assert_eq!(restarted.language, Language::English);
    let ctx = Context::default();
    theme::install(&ctx);
    let first = draw_home(&ctx, &mut restarted, Vec::new());
    assert!(drawn_text(&first).iter().any(|text| text == "Create"), "{:?}", drawn_text(&first));

    // And back to Korean through the same menu.
    draw_home(&ctx, &mut restarted, click(text_center(&first, "Workspace")));
    let menu = draw_home(&ctx, &mut restarted, Vec::new());
    draw_home(&ctx, &mut restarted, click(text_center(&menu, "한국어")));
    assert_eq!(restarted.language, Language::Korean);
    assert!(drawn_text(&draw_home(&ctx, &mut restarted, Vec::new())).iter().any(|text| text == "만들기"));
    let saved: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(root.join(".gui/settings.json")).expect("the setting")).expect("JSON");
    assert_eq!(saved["language"], "ko");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn the_english_home_screen_and_its_menus_show_no_korean() {
    let _restore = Restore;
    let mut app = app("Workspace");
    app.set_language(Language::English);
    let ctx = Context::default();
    theme::install(&ctx);
    let mut found = Vec::new();
    for (list_view, menu) in [(true, None), (false, None), (true, Some("size")), (true, Some("runner")), (true, Some("design_system")), (true, Some("workspace"))] {
        app.home.list_view = list_view;
        app.home.smoke_open = menu.map(str::to_owned);
        for _ in 0..3 {
            let output = draw_home(&ctx, &mut app, Vec::new());
            found.extend(korean(&output).into_iter().map(|text| format!("{menu:?}: {text}")));
        }
    }
    found.dedup();
    assert!(found.is_empty(), "Korean on the English home screen:\n{}", found.join("\n"));
}

#[test]
fn the_english_editor_shows_no_korean_in_its_panels() {
    let _restore = Restore;
    let mut host = Host::start();
    let canvas = canvas_with_page(&mut host, "Poster");
    let mut window = Window::open(&mut host, &canvas);
    window.app.workspace_name = "Workspace".into();
    window.app.screen = Screen::Editor;
    window.app.set_language(Language::English);
    let mut found = Vec::new();
    let mut scan = |window: &mut Window, host: &mut Host, what: &str| {
        window.draw(host, 1.0);
        let output = egui::FullOutput { shapes: window.shapes.clone(), ..Default::default() };
        found.extend(korean(&output).into_iter().map(|text| format!("{what}: {text}")));
    };
    for tab in [LeftTab::Layers, LeftTab::Tools, LeftTab::Ai] {
        window.app.view().left_tab = tab;
        scan(&mut window, &mut host, &format!("canvas {tab:?}"));
    }
    window.app.view().left_tab = LeftTab::Layers;
    // The properties panel of a selected shape and text.
    for add in ["add.rect", "add.text"] {
        let state = format!("{:?} {:?}", window.app.screen, window.app.active.as_ref().map(|active| active.kind));
        window.step(&mut host, add, json!({})).unwrap_or_else(|error| panic!("{add} on {state}: {error}"));
        scan(&mut window, &mut host, add);
    }

    // A figure, with its data sheet.
    // Titled in English: a document title is the user's text and is shown as it is.
    let figure = host.create("figure", "Chart");
    let mut window = Window::open(&mut host, &figure);
    window.app.workspace_name = "Workspace".into();
    window.app.screen = Screen::Editor;
    window.app.set_language(Language::English);
    scan(&mut window, &mut host, "figure");
    for tab in [LeftTab::Tools, LeftTab::Ai] {
        window.app.view().left_tab = tab;
        scan(&mut window, &mut host, &format!("figure {tab:?}"));
    }
    found.sort();
    found.dedup();
    assert!(found.is_empty(), "Korean in the English editor:\n{}", found.join("\n"));
}

#[test]
fn notices_are_shown_in_english_and_a_made_message_as_its_whole_template() {
    let _restore = Restore;
    let mut harness = Harness::new();
    harness.app.set_language(Language::English);
    let ctx = harness.ctx.clone();
    theme::install(&ctx);
    for (shown, english) in [
        ("텍스트 편집을 끝낸 뒤 이 단축키를 사용하세요".to_owned(), "Finish editing text before using this shortcut"),
        // A message made with format!: the template is translated whole, the inserted part kept.
        (format!("프로젝트 창을 열지 못했습니다. {}", "E_LOCKED"), "Could not open the project window. E_LOCKED"),
    ] {
        harness.app.set_error(shown.clone());
        // The notice keeps the source text (tests and the UI channel read it); only its display is English.
        assert_eq!(harness.message().map(|(_, text)| text), Some(shown));
        let _ = ctx.run(screen(1.0, 900.0, 600.0), |ctx| harness.app.draw_error_callout(ctx));
        let output = ctx.run(screen(1.0, 900.0, 600.0), |ctx| harness.app.draw_error_callout(ctx));
        let drawn = drawn_text(&output);
        assert!(drawn.iter().any(|text| text == english), "{english:?} not in {drawn:?}");
        assert!(korean(&output).is_empty(), "{:?}", korean(&output));
    }
}

#[test]
fn every_command_label_and_shortcut_has_its_english() {
    let _restore = Restore;
    i18n::set_language(Language::English);
    let missing: Vec<String> = crate::commands::COMMANDS
        .iter()
        .flat_map(|spec| [spec.label, spec.shortcut])
        .filter(|text| has_hangul(&i18n::tr(text)))
        .map(str::to_owned)
        .collect();
    assert!(missing.is_empty(), "command texts with no English:\n{}", missing.join("\n"));
    // The registry itself (what agents and commands.json read) stays Korean.
    assert!(crate::commands::COMMANDS.iter().any(|spec| has_hangul(spec.label)));
}

#[test]
fn every_lint_label_has_its_english() {
    let _restore = Restore;
    i18n::set_language(Language::English);
    // The labels are the `=> "…"` arms of `strings::lint_label`.
    let source = include_str!("../strings.rs");
    let labels: Vec<&str> = source.split("=> \"").skip(1).filter_map(|rest| rest.split('"').next()).filter(|label| has_hangul(label)).collect();
    assert!(labels.len() > 10, "the lint labels were found: {labels:?}");
    let missing: Vec<&str> = labels.into_iter().filter(|label| has_hangul(&i18n::tr(label))).collect();
    assert!(missing.is_empty(), "lint labels with no English: {missing:?}");
}

#[test]
fn the_engine_texts_the_window_shows_have_their_english() {
    let _restore = Restore;
    i18n::set_language(Language::English);
    // Kept by hand in i18n/engine.rs: a change of the engine's text must change the table too.
    let disclaimer = designllm_contracts::figure_rules::FIGURE_OUTPUT_PROFILE_DISCLAIMER;
    assert!(!has_hangul(&i18n::tr(disclaimer)), "{disclaimer:?} has no English");
}
