//! The sidebar's and the properties panel's own close buttons (`chrome.rs`): each one toggles its panel with the view group's
//! command, and the outer button's command opens it again. Closing is view state, so no history step is made.

use eframe::egui::{Event, Modifiers, PointerButton, Pos2, RawInput, Rect};
use serde_json::json;

use super::test_support::*;
use crate::theme;

fn run_frame(harness: &mut Harness, input: RawInput) -> Option<crate::app::chrome::FrameLayout> {
    let mut layout = None;
    let _ = harness.ctx.run(input, |ctx| layout = Some(harness.app.draw_frame(ctx)));
    layout
}

fn click_input(pos: Pos2) -> RawInput {
    let mut input = screen(1.0, 1280.0, 800.0);
    input.events = vec![
        Event::PointerMoved(pos),
        Event::PointerButton { pos, button: PointerButton::Primary, pressed: true, modifiers: Modifiers::NONE },
        Event::PointerButton { pos, button: PointerButton::Primary, pressed: false, modifiers: Modifiers::NONE },
    ];
    input
}

/// A canvas window with both panels open, drawn twice, and the sidebar's and the panel's rectangles.
fn both_panels_open() -> (Harness, Rect, Rect) {
    let mut harness = Harness::new();
    harness.open_document("canvas", 1);
    theme::install(&harness.ctx);
    harness.app.view().left_open = true;
    harness.app.view().right_open = true;
    let layout = run_frame(&mut harness, screen(1.0, 1280.0, 800.0)).expect("a frame was drawn");
    let _ = run_frame(&mut harness, screen(1.0, 1280.0, 800.0));
    let left = layout.left.expect("the sidebar is docked");
    let right = layout.right.expect("the properties panel is drawn");
    let _ = harness.sent();
    (harness, left, right)
}

/// The centre of a panel's close button: at the right end of its 40 px head row, 8 px in.
fn close_button(panel: Rect) -> Pos2 {
    Pos2::new(panel.right() - 8.0 - theme::ICON_BUTTON / 2.0, panel.top() + theme::SIDEBAR_TABS_HEIGHT / 2.0)
}

/// Closing is view state: no change or history step is sent.
fn assert_no_history_step(harness: &mut Harness) {
    let requests = harness.sent();
    let sent = methods(&requests);
    assert!(sent.iter().all(|method| !method.starts_with("history.") && *method != "changes.apply"), "{sent:?}");
}

#[test]
fn the_sidebar_close_button_closes_it_and_the_outer_command_reopens_it() {
    let (mut harness, left, _) = both_panels_open();
    let _ = run_frame(&mut harness, click_input(close_button(left)));
    let _ = run_frame(&mut harness, screen(1.0, 1280.0, 800.0));
    assert!(!harness.app.view_ref().left_open, "the sidebar's own button closes the sidebar");
    assert!(harness.app.view_ref().right_open, "the other panel stays open");

    // The outer view group's button runs this command (`draw_floating_groups`).
    assert_eq!(harness.app.invoke("nav.toggle_left", json!({})), Ok(()));
    assert!(harness.app.view_ref().left_open, "the outer command reopens the sidebar");
    assert!(harness.app.view_ref().right_open);
    assert_no_history_step(&mut harness);
}

#[test]
fn the_properties_close_button_closes_it_and_the_outer_command_reopens_it() {
    let (mut harness, _, right) = both_panels_open();
    let _ = run_frame(&mut harness, click_input(close_button(right)));
    let layout = run_frame(&mut harness, screen(1.0, 1280.0, 800.0)).expect("a frame was drawn");
    assert!(!harness.app.view_ref().right_open, "the panel's own button closes the properties panel");
    assert!(layout.right.is_none(), "the panel is no longer drawn");
    assert!(harness.app.view_ref().left_open, "the other panel stays open");

    assert_eq!(harness.app.invoke("nav.toggle_right", json!({})), Ok(()));
    let layout = run_frame(&mut harness, screen(1.0, 1280.0, 800.0)).expect("a frame was drawn");
    assert!(harness.app.view_ref().right_open, "the outer command reopens the properties panel");
    assert!(layout.right.is_some());
    assert_no_history_step(&mut harness);
}
