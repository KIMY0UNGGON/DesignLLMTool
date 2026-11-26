//! S24/S25: the settings lock is an open handle the system frees (a held lock times the save out within the deadline and the
//! change waits; nothing is ever taken from a holder), a profile choice writes its DPI too, exports write no settings, and
//! the export popover's DPI pick stays the popover's.

use std::time::{Duration, Instant};

use eframe::egui;
use serde_json::json;

use super::{
    settings::{load, settings_path, FigureOutput, SettingsLock, LOCK_WAIT},
    test_support::*,
    DesktopApp, Screen,
};
use crate::rpc::RpcWorker;

fn app_on(root: &std::path::Path) -> DesktopApp {
    let (rpc, _replies, _work) = RpcWorker::detached();
    DesktopApp::new(rpc, "Workspace".into(), None, None).with_workspace(root)
}

fn scratch(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("designllm-{name}-{}", designllm_contracts::new_id()))
}

#[test]
fn a_held_settings_lock_times_the_save_out_and_the_change_waits() {
    let root = scratch("settings-held-lock");
    let path = settings_path(&root);
    let mut app = app_on(&root);
    // Another window holds the lock (an open handle, as it does across its read, merge and replace).
    let held = SettingsLock::acquire(&path).expect("the other window's lock");
    app.home.starred.insert("project".into());
    let started = Instant::now();
    app.save_star("project");
    let waited = started.elapsed();
    assert!(waited >= LOCK_WAIT && waited < LOCK_WAIT + Duration::from_secs(2), "the save gave up at the deadline: {waited:?}");
    assert!(app.settings_unsaved(), "the star waits");
    assert!(app.notices.current(Instant::now()).is_some_and(|notice| notice.text.contains("화면 설정을 저장하지 못했습니다")));
    assert!(path.with_extension("json.lock").exists(), "the holder's lock is not taken away");
    // The other window is done: the next save goes through.
    drop(held);
    app.save_settings();
    assert!(!app.settings_unsaved());
    assert!(load(&path).starred.contains("project"));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_lock_file_left_without_a_holder_does_not_block_and_is_never_deleted() {
    let root = scratch("settings-left-lock");
    let path = settings_path(&root);
    std::fs::create_dir_all(path.parent().expect("the folder")).expect("the folder");
    std::fs::write(path.with_extension("json.lock"), "").expect("a lock file a closed window left");
    let mut app = app_on(&root);
    app.home.starred.insert("project".into());
    let started = Instant::now();
    app.save_star("project");
    assert!(started.elapsed() < LOCK_WAIT, "no holder: no wait");
    assert!(!app.settings_unsaved());
    if cfg!(windows) {
        assert!(path.with_extension("json.lock").exists(), "the lock file stays; only its handle locks");
    }
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_profile_choice_writes_its_dpi_even_when_this_window_had_the_same_dpi() {
    let root = scratch("settings-profile-dpi");
    let mut window_a = app_on(&root);
    let mut window_b = app_on(&root);
    // Window B applied the print profile (600 DPI) after A had read the file (A still holds 300).
    let view = window_b.editor_views.entry("fig".to_owned()).or_default();
    view.figure_output_profile = Some("high_res_print".into());
    view.figure_export_dpi = 600;
    window_b.remember_figure_output("fig");
    // A applies a profile whose DPI (300) equals what A had: the profile brings its DPI, so 300 is written.
    let view = window_a.editor_views.entry("fig".to_owned()).or_default();
    view.figure_output_profile = Some("single_column".into());
    view.figure_export_dpi = 300;
    window_a.remember_figure_output("fig");
    assert_eq!(load(&settings_path(&root)).figure_outputs.get("fig"), Some(&FigureOutput { profile: Some("single_column".into()), dpi: 300 }));
    let _ = std::fs::remove_dir_all(&root);
}

/// A Figure window with the settings of `root`.
fn figure_window(host: &mut Host, root: &std::path::Path) -> (Window, String) {
    let figure_id = host.create("figure", "Chart");
    let mut window = Window::open(host, &figure_id);
    let (rpc, _, _) = RpcWorker::detached();
    let app = std::mem::replace(&mut window.app, DesktopApp::new(rpc, "Workspace".into(), None, None));
    window.app = app.with_workspace(root);
    window.app.screen = Screen::Editor;
    (window, figure_id)
}

#[test]
fn exports_write_no_settings_and_a_restart_brings_back_the_default_dpi() {
    let root = scratch("settings-export-free");
    std::fs::create_dir_all(&root).expect("the workspace");
    let mut host = Host::start();
    let (mut window, figure_id) = figure_window(&mut host, &root);
    let target = root.join("figure.png").to_string_lossy().into_owned();
    window.step(&mut host, "file.export", json!({ "format": "png", "dpi": 150, "target_path": target })).expect("the export is asked for");
    window.settle(&mut host);
    assert!(load(&settings_path(&root)).figure_outputs.get(&figure_id).is_none(), "an export writes no setting");
    // No profile was chosen: a new window starts at 300 DPI.
    let (rpc, _, _) = RpcWorker::detached();
    let mut again = Window::open(&mut host, &figure_id);
    let app = std::mem::replace(&mut again.app, DesktopApp::new(rpc, "Workspace".into(), None, None));
    again.app = app.with_workspace(&root);
    assert_eq!(again.app.view_ref().figure_export_dpi, 300);
    let _ = std::fs::remove_dir_all(&root);
}

/// The centre of the first drawn text that reads `label`.
fn text_at(window: &Window, label: &str) -> Option<egui::Pos2> {
    fn walk(shape: &egui::epaint::Shape, label: &str) -> Option<egui::Pos2> {
        match shape {
            egui::epaint::Shape::Text(text) if text.galley.text() == label => Some(text.pos + text.galley.size() / 2.0),
            egui::epaint::Shape::Vec(shapes) => shapes.iter().find_map(|shape| walk(shape, label)),
            _ => None,
        }
    }
    window.shapes.iter().find_map(|clipped| walk(&clipped.shape, label))
}

fn click(at: egui::Pos2) -> Vec<egui::Event> {
    vec![
        egui::Event::PointerMoved(at),
        egui::Event::PointerButton { pos: at, button: egui::PointerButton::Primary, pressed: true, modifiers: egui::Modifiers::NONE },
        egui::Event::PointerButton { pos: at, button: egui::PointerButton::Primary, pressed: false, modifiers: egui::Modifiers::NONE },
    ]
}

#[test]
fn a_dpi_picked_in_a_dismissed_export_popover_leaves_the_view_as_it_was() {
    let root = scratch("settings-popover");
    let mut host = Host::start();
    let (mut window, _) = figure_window(&mut host, &root);
    window.app.export_format = "png".into();
    window.draw(&mut host, 1.0);
    // The popover opens (as its button does), and 150 is picked in it.
    window.app.smoke_open = Some("export".into());
    window.draw(&mut host, 1.0);
    let at = text_at(&window, "150").expect("the popover shows its DPI choices");
    window.click(&mut host, 1.0, at, false);
    window.frame(&mut host, 1.0, Vec::new());
    assert_eq!(window.app.export_dpi(), 150, "the popover shows its pick");
    assert_eq!(window.app.view_ref().figure_export_dpi, 300, "the view is not changed by a pick");
    // A press outside closes the popover without an export.
    window.frame(&mut host, 1.0, click(egui::Pos2::new(640.0, 500.0)));
    window.draw(&mut host, 1.0);
    assert!(text_at(&window, "150").is_none(), "the popover is closed");
    assert_eq!(window.app.export_dpi(), 300, "its pick went with it");
    assert_eq!(window.app.view_ref().figure_export_dpi, 300);
    let _ = std::fs::remove_dir_all(&root);
}
