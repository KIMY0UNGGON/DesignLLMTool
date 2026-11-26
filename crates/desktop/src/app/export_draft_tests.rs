//! S26: another export leaves the open popover's DPI pick alone, and a settings write that failed while a Figure was made
//! with a profile is still told once that Figure is open.

use eframe::egui;
use serde_json::json;

use super::{settings::SETTINGS_SAVE_FAILED, test_support::*, DesktopApp, Screen};
use crate::{notice::Severity, rpc::RpcWorker};

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

#[test]
fn another_export_leaves_the_open_popovers_dpi_pick_for_its_own_submit() {
    let root = std::env::temp_dir().join(format!("designllm-export-draft-{}", designllm_contracts::new_id()));
    std::fs::create_dir_all(&root).expect("a folder");
    let mut host = Host::start();
    let figure_id = host.create("figure", "Chart");
    let mut window = Window::open(&mut host, &figure_id);
    window.app.screen = Screen::Editor;
    window.app.export_format = "png".into();
    window.draw(&mut host, 1.0);
    window.app.smoke_open = Some("export".into());
    window.draw(&mut host, 1.0);
    let at = text_at(&window, "150").expect("the popover's DPI choices");
    window.click(&mut host, 1.0, at, false);
    window.frame(&mut host, 1.0, Vec::new());
    assert_eq!(window.app.export_dpi(), 150, "the popover holds its pick");
    // An agent's export at 600 while the popover is open.
    let target = root.join("agent.png").to_string_lossy().into_owned();
    window.step(&mut host, "file.export", json!({ "format": "png", "dpi": 600, "target_path": target })).expect("the agent's export");
    window.frame(&mut host, 1.0, Vec::new());
    assert_eq!(window.app.export_dpi(), 150, "the pick is still the popover's");
    // The person submits the popover: its export is at 150 (the view takes it).
    let submit = text_at(&window, "내보내기…").expect("the popover's export button");
    window.click(&mut host, 1.0, submit, false);
    window.frame(&mut host, 1.0, Vec::new());
    assert_eq!(window.app.view_ref().figure_export_dpi, 150, "the submitted export used the popover's pick");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_failed_settings_write_when_a_figure_is_made_with_a_profile_is_told_on_the_open_figure() {
    let root = std::env::temp_dir().join(format!("designllm-create-profile-{}", designllm_contracts::new_id()));
    std::fs::create_dir_all(&root).expect("the workspace");
    // The settings folder cannot be made: a file stands where it would be.
    std::fs::write(root.join(".gui"), "not a folder").expect("a file in the way");
    let mut harness = Harness::new();
    let (rpc, _, _) = RpcWorker::detached();
    let app = std::mem::replace(&mut harness.app, DesktopApp::new(rpc, "Workspace".into(), None, None));
    harness.app = app.with_workspace(&root);
    harness.app.home.tile = "figure".into();
    harness.app.invoke("home.preset", json!({ "preset": "high_res_print" })).expect("the profile is accepted");
    harness.app.create_from_home_command(&json!({ "tile": "figure" })).expect("the figure is requested");
    let requests = harness.sent();
    harness.answer(&requests, "artifact.create", Ok(summary("created-figure", "figure", 0)));
    assert_eq!(harness.app.screen, Screen::Editor);
    assert_eq!(harness.app.active.as_ref().map(|active| active.id.as_str()), Some("created-figure"), "the new Figure is open");
    assert_eq!(harness.message(), Some((Severity::Error, SETTINGS_SAVE_FAILED.to_owned())), "the failed save is told on the open Figure");
    assert!(harness.app.settings_unsaved(), "the profile waits for a save");
    // Once the folder can be made, the next save writes the profile.
    std::fs::remove_file(root.join(".gui")).expect("the file goes");
    harness.app.save_settings();
    assert!(!harness.app.settings_unsaved());
    let saved = super::settings::load(&super::settings::settings_path(&root));
    assert_eq!(saved.figure_outputs.get("created-figure").and_then(|output| output.profile.as_deref()), Some("high_res_print"));
    let _ = std::fs::remove_dir_all(&root);
}
