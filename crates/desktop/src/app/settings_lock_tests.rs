//! S23: settings writes of several windows are serialized by a lock and merged field by field, an unreadable file is never
//! overwritten, a refused export keeps no DPI, a long choice list scrolls, a failed language save is tried again, and a
//! closed picker shows one line of a multi-line value.

use eframe::egui::{self, Event, Modifiers, PointerButton, Pos2};
use serde_json::json;

use super::{
    settings::{load, settings_path, update, FigureOutput},
    test_support::*,
    DesktopApp, Screen,
};
use crate::{i18n::Language, rpc::RpcWorker};

fn app_on(root: &std::path::Path) -> DesktopApp {
    let (rpc, _replies, _work) = RpcWorker::detached();
    DesktopApp::new(rpc, "Workspace".into(), None, None).with_workspace(root)
}

fn scratch(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("designllm-{name}-{}", designllm_contracts::new_id()))
}

struct Restore;

impl Drop for Restore {
    fn drop(&mut self) {
        crate::i18n::set_language(Language::Korean);
    }
}

#[test]
fn overlapping_saves_of_two_windows_keep_every_change() {
    let root = scratch("settings-lock");
    let path = settings_path(&root);
    let writers: Vec<_> = (0..2)
        .map(|window| {
            let path = path.clone();
            std::thread::spawn(move || {
                for index in 0..25 {
                    update(&path, |settings| {
                        settings.figure_outputs.insert(format!("w{window}-{index}"), FigureOutput { profile: None, dpi: 300 });
                    })
                    .expect("the write waits for the other window and succeeds");
                }
            })
        })
        .collect();
    for writer in writers {
        writer.join().expect("the writer ends");
    }
    let saved = load(&path);
    assert_eq!(saved.figure_outputs.len(), 50, "no change is lost: {:?}", saved.figure_outputs.keys().collect::<Vec<_>>());
    // Nobody holds the lock after the writes: it is taken at once (the file may stay; only an open handle locks).
    let started = std::time::Instant::now();
    drop(super::settings::SettingsLock::acquire(&path).expect("the lock is free"));
    assert!(started.elapsed() < std::time::Duration::from_millis(500));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn an_unreadable_settings_file_is_not_overwritten_and_the_change_waits() {
    let root = scratch("settings-unreadable");
    let path = settings_path(&root);
    let mut app = app_on(&root);
    std::fs::create_dir_all(path.parent().expect("the folder")).expect("the folder");
    std::fs::write(&path, "{ not json").expect("a damaged file");
    app.home.starred.insert("project".into());
    app.save_star("project");
    assert_eq!(std::fs::read_to_string(&path).expect("the file"), "{ not json", "a file that cannot be read is left as it is");
    assert!(app.settings_unsaved(), "the star waits to be saved");
    assert!(app.notices.current(std::time::Instant::now()).is_some_and(|notice| notice.text.contains("화면 설정을 저장하지 못했습니다")));
    // The file is readable again (another window's Figure output in it): the next save merges into it.
    std::fs::write(&path, r#"{"figure_outputs":{"fig":{"profile":"high_res_print","dpi":600}}}"#).expect("a readable file");
    app.save_settings();
    let saved = load(&path);
    assert!(saved.starred.contains("project"));
    assert_eq!(saved.figure_outputs.get("fig"), Some(&FigureOutput { profile: Some("high_res_print".into()), dpi: 600 }), "nothing was erased");
    assert!(!app.settings_unsaved());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_refused_export_writes_no_setting() {
    let root = scratch("settings-export");
    let mut host = Host::start();
    let figure_id = host.create("figure", "Chart");
    let mut window = Window::open(&mut host, &figure_id);
    let (rpc, _, _) = RpcWorker::detached();
    let app = std::mem::replace(&mut window.app, DesktopApp::new(rpc, "Workspace".into(), None, None));
    window.app = app.with_workspace(&root);
    window.app.screen = Screen::Editor;
    let target = root.join("selection.png").to_string_lossy().into_owned();
    let refused = window.step(&mut host, "file.export", json!({ "format": "png", "dpi": 150, "scope": "selection", "target_path": target }));
    assert!(refused.is_err(), "nothing is selected: {refused:?}");
    assert!(load(&settings_path(&root)).figure_outputs.get(&figure_id).is_none(), "nothing was kept for the Figure");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_failed_language_save_is_tried_again_by_the_same_choice() {
    let _restore = Restore;
    let root = scratch("settings-language");
    let mut app = app_on(&root);
    std::fs::create_dir_all(&root).expect("the workspace");
    std::fs::write(root.join(".gui"), "not a folder").expect("a file in the way");
    app.set_language(Language::English);
    assert!(app.settings_unsaved());
    std::fs::remove_file(root.join(".gui")).expect("the file goes");
    app.set_language(Language::English);
    assert!(!app.settings_unsaved());
    assert_eq!(load(&settings_path(&root)).language, "en");
    let _ = std::fs::remove_dir_all(&root);
}

fn click(at: Pos2) -> Vec<Event> {
    vec![
        Event::PointerMoved(at),
        Event::PointerButton { pos: at, button: PointerButton::Primary, pressed: true, modifiers: Modifiers::NONE },
        Event::PointerButton { pos: at, button: PointerButton::Primary, pressed: false, modifiers: Modifiers::NONE },
    ]
}

#[test]
fn a_long_list_of_tall_choices_scrolls_to_its_last_choice() {
    let ctx = egui::Context::default();
    crate::theme::install(&ctx);
    let palette = crate::theme::LIGHT;
    let labels: Vec<String> = (0..14).map(|index| format!("Block {index}\nsecond line\nthird line")).collect();
    let options: Vec<&str> = labels.iter().map(String::as_str).collect();
    let height = 400.0;
    let frame = |events: Vec<Event>| {
        let mut input = screen(1.0, 600.0, height);
        input.events = events;
        ctx.run(input, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                crate::widgets::select_menu_verbatim(ui, &palette, "Block 0", &options, Some(0), 200.0);
            });
        })
    };
    frame(Vec::new());
    frame(click(Pos2::new(40.0, 22.0)));
    let last = labels.last().expect("a last choice").clone();
    let visible_bottom = |output: &egui::FullOutput| {
        output.shapes.iter().find_map(|clipped| match &clipped.shape {
            egui::epaint::Shape::Text(text) if text.galley.text() == last => Some((text.pos.y + text.galley.size().y, clipped.clip_rect)),
            _ => None,
        })
    };
    let open = frame(Vec::new());
    // Before scrolling the last choice is outside the list's view (or not drawn): the list is cut to the window.
    assert!(visible_bottom(&open).is_none_or(|(bottom, clip)| bottom > clip.bottom()), "the list is taller than its view");
    let mut reached = false;
    for _ in 0..40 {
        let output = frame(vec![
            Event::PointerMoved(Pos2::new(80.0, 200.0)),
            Event::MouseWheel { unit: egui::MouseWheelUnit::Point, delta: egui::vec2(0.0, -120.0), modifiers: Modifiers::NONE },
        ]);
        if visible_bottom(&output).is_some_and(|(bottom, clip)| bottom <= clip.bottom() + 0.5 && clip.bottom() <= height) {
            reached = true;
            break;
        }
    }
    assert!(reached, "scrolling brings the last choice into the window");
}

#[test]
fn a_closed_picker_shows_the_first_line_of_a_multi_line_value() {
    let ctx = egui::Context::default();
    crate::theme::install(&ctx);
    let palette = crate::theme::LIGHT;
    let output = ctx.run(screen(1.0, 600.0, 300.0), |ctx| {
        egui::CentralPanel::default().show(ctx, |ui| {
            crate::widgets::dropdown_verbatim(ui, &palette, "Feed\nforward network", None, 220.0);
        });
    });
    let drawn = drawn_text(&output);
    assert!(drawn.iter().any(|text| text == "Feed…"), "{drawn:?}");
    assert!(!drawn.iter().any(|text| text.contains('\n')), "no line break in the one-line field: {drawn:?}");
}
