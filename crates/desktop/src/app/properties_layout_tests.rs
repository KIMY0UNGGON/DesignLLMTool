//! What the grid breakpoint controls (D05.2) and the fill popover's gradient removal (D03.2) emit. The controls build their
//! changes with these functions, so the tests check the operations and the layouts they produce.

use super::*;
use crate::app::test_support::{screen, Harness};
use designllm_contracts::{ArtifactKind, ArtifactSummary, Breakpoint, LayoutSpec, Operation};
use eframe::egui;

/// A three-column grid with a gap and padding, so the tests can see that the rest of the layout is sent along unchanged.
fn grid(breakpoints: Vec<Breakpoint>) -> LayoutSpec {
    LayoutSpec {
        mode: "grid".into(),
        width_mode: "fixed".into(),
        gap: 8 * 9_525,
        padding: 4 * 9_525,
        grid_columns: Some(3),
        breakpoints,
        ..LayoutSpec::default()
    }
}

fn breakpoint(min_width_css_px: u32, grid_columns: u32) -> Breakpoint {
    Breakpoint { min_width_css_px, grid_columns: Some(grid_columns), ..Breakpoint::default() }
}

/// The widths and columns of a layout's breakpoints, in order.
fn shape(spec: &LayoutSpec) -> Vec<(u32, Option<u32>)> {
    spec.breakpoints.iter().map(|breakpoint| (breakpoint.min_width_css_px, breakpoint.grid_columns)).collect()
}

#[test]
fn layout_breakpoint_added_starts_at_tablet_width_with_the_grid_columns() {
    let next = breakpoint_added(&grid(Vec::new())).expect("room for a breakpoint");
    assert_eq!(shape(&next), vec![(768, Some(3))]);
    assert_eq!(next.gap, 8 * 9_525, "the rest of the layout is unchanged");
    assert_eq!(next.mode, "grid");
}

#[test]
fn layout_breakpoint_added_follows_the_widest_breakpoint() {
    let next = breakpoint_added(&grid(vec![breakpoint(768, 2), breakpoint(1024, 4)])).expect("room for a breakpoint");
    let widths: Vec<u32> = next.breakpoints.iter().map(|breakpoint| breakpoint.min_width_css_px).collect();
    assert_eq!(widths, vec![768, 1024, 1152]);
}

#[test]
fn layout_breakpoint_list_stops_at_sixteen() {
    let full: Vec<Breakpoint> = (0..16).map(|index| breakpoint(100 + index * 10, 2)).collect();
    assert!(breakpoint_added(&grid(full)).is_err(), "the engine takes sixteen breakpoints at most");
    let almost: Vec<Breakpoint> = (0..15).map(|index| breakpoint(100 + index * 10, 2)).collect();
    assert_eq!(breakpoint_added(&grid(almost)).expect("the sixteenth fits").breakpoints.len(), 16);
}

#[test]
fn layout_breakpoint_width_edit_keeps_the_list_ascending_with_its_columns() {
    // Moving the first breakpoint past the second re-sorts the list, and each breakpoint keeps its columns.
    let spec = grid(vec![breakpoint(768, 2), breakpoint(1024, 4)]);
    let next = breakpoint_width_set(&spec, 0, 1100).expect("a free width");
    assert_eq!(shape(&next), vec![(1024, Some(4)), (1100, Some(2))]);
}

#[test]
fn layout_breakpoint_width_edit_refuses_a_duplicate_and_zero() {
    let spec = grid(vec![breakpoint(768, 2), breakpoint(1024, 4)]);
    assert!(breakpoint_width_set(&spec, 0, 1024).is_err(), "the second breakpoint has 1024");
    assert!(breakpoint_width_set(&spec, 0, 768).is_ok(), "keeping its own width is not a duplicate");
    assert!(breakpoint_width_set(&spec, 0, 0).is_err(), "a zero width is refused");
}

/// A typed width past what the layout stores is refused, not saturated to 4,294,967,295 px; values in range round as before.
#[test]
fn layout_breakpoint_values_past_u32_are_refused_not_saturated() {
    assert_eq!(breakpoint_whole(1e10), None);
    assert_eq!(breakpoint_whole(f64::from(u32::MAX)), Some(u32::MAX));
    assert_eq!(breakpoint_whole(1023.6), Some(1024));
    assert_eq!(breakpoint_whole(-3.0), Some(0));
    let spec = grid(vec![breakpoint(768, 8)]);
    let width = breakpoint_whole(1e10).ok_or(BREAKPOINT_WIDTH_TOO_LARGE).and_then(|width| breakpoint_width_set(&spec, 0, width));
    assert_eq!(width.err(), Some(BREAKPOINT_WIDTH_TOO_LARGE));
    assert!(breakpoint_columns_set(&spec, 0, breakpoint_whole(1e10).unwrap_or(0)).is_err(), "a huge column count is out of range");
}

/// The page size fields are keyed by the page's id: the page that takes another's index gets fields of its own.
#[test]
fn page_size_fields_are_keyed_by_the_page_id() {
    assert_ne!(page_field_id("page-a", "w"), page_field_id("page-b", "w"));
    assert_ne!(page_field_id("page-a", "w"), page_field_id("page-a", "h"));
    assert_eq!(page_field_id("page-a", "w"), page_field_id("page-a", "w"));
}

#[test]
fn layout_breakpoint_columns_edit_accepts_one_to_twenty_four_only() {
    let spec = grid(vec![breakpoint(768, 2)]);
    assert!(breakpoint_columns_set(&spec, 0, 0).is_err());
    assert!(breakpoint_columns_set(&spec, 0, 25).is_err());
    let next = breakpoint_columns_set(&spec, 0, 24).expect("24 is the most columns");
    assert_eq!(shape(&next), vec![(768, Some(24))]);
}

#[test]
fn layout_breakpoint_removal_keeps_the_others() {
    let spec = grid(vec![breakpoint(768, 2), breakpoint(1024, 4), breakpoint(1280, 6)]);
    let next = breakpoint_removed(&spec, 1).expect("the middle breakpoint");
    assert_eq!(shape(&next), vec![(768, Some(2)), (1280, Some(6))]);
    assert!(breakpoint_removed(&spec, 3).is_err(), "there is no fourth breakpoint");
}

#[test]
fn layout_breakpoint_edits_emit_the_whole_layout_as_one_set_layout() {
    let spec = grid(vec![breakpoint(768, 2)]);
    let edited = breakpoint_columns_set(&spec, 0, 5).expect("new columns");
    match layout_operation("grid-frame", edited) {
        Operation::SetLayout { node_id, layout } => {
            assert_eq!(node_id, "grid-frame");
            assert_eq!(shape(&layout), vec![(768, Some(5))]);
            assert_eq!(layout.grid_columns, Some(3), "the grid's own columns are sent along");
            assert_eq!(layout.gap, 8 * 9_525, "the gap is sent along");
            assert_eq!(layout.padding, 4 * 9_525, "the padding is sent along");
        }
        _ => panic!("a breakpoint control emits SetLayout"),
    }
}

#[test]
fn fill_items_offer_solid_when_the_node_has_no_gradient_or_the_gradient_can_be_removed() {
    let icons = |gradient_set: bool, removable: bool| -> Vec<&'static str> {
        fill_items(gradient_set, removable).into_iter().map(|(icon, _)| icon).collect()
    };
    assert_eq!(icons(false, false), vec!["fill-solid", "fill-gradient"]);
    assert_eq!(icons(true, false), vec!["fill-gradient"], "without removal, solid is not offered on a gradient");
    assert_eq!(icons(true, true), vec!["fill-solid", "fill-gradient"]);
}

#[test]
fn fill_choice_remove_gradient_emits_the_remove_command_with_no_arguments() {
    let (command, args) = fill_choice_command(&FillChoice::RemoveGradient);
    assert_eq!(command, "props.gradient_remove");
    assert_eq!(args, serde_json::json!({}));
}

#[test]
fn fill_choice_gradient_emits_the_gradient_command_with_its_colours_and_angle() {
    let choice = FillChoice::Gradient { from: "#112233".into(), to: "#445566".into(), angle: 90.0 };
    let (command, args) = fill_choice_command(&choice);
    assert_eq!(command, "props.gradient");
    assert_eq!(args, serde_json::json!({ "from": "#112233", "to": "#445566", "angle": 90.0 }));
}

/// A canvas grid node that holds `spec`: the fields the breakpoint rows read.
fn grid_node(spec: LayoutSpec) -> NodeProperties {
    NodeProperties {
        id: "grid-frame".into(),
        kind: NodeKind::Rectangle,
        bounds: designllm_contracts::Rect { x: 0, y: 0, width: 1_000_000, height: 500_000 },
        has_children: true,
        style: designllm_contracts::Style::default(),
        layout: None,
        full_layout: spec,
        list: None,
        image_fit: None,
        image_crop_pixel_rect: None,
        alt: None,
        locked: None,
    }
}

/// Draws the breakpoint rows of `node` once in the harness's egui context, as the properties panel does. `focus` takes the
/// keyboard focus inside the frame, as a click or a Tab press does: egui reports a loss of focus only when it happens in a frame.
fn draw_breakpoints(h: &mut Harness, node: &NodeProperties, focus: Option<Id>) {
    let palette = theme::palette(&h.ctx);
    let app = &mut h.app;
    let _ = h.ctx.run(screen(1.0, 640.0, 480.0), |ctx| {
        if let Some(id) = focus {
            ctx.memory_mut(|memory| memory.request_focus(id));
        }
        egui::CentralPanel::default().show(ctx, |ui| app.breakpoint_rows(ui, &palette, node));
    });
}

#[test]
fn layout_breakpoint_field_ids_follow_their_breakpoint_through_a_width_edit() {
    // A width edit re-sorts the list: the 1024 breakpoint moves to the first row and keeps its fields' ids. The old ids of the
    // edited breakpoint belong to no breakpoint now, so text typed into them cannot land on another breakpoint.
    let spec = grid(vec![breakpoint(768, 2), breakpoint(1024, 4)]);
    let before = breakpoint_field_ids("grid-frame", &spec);
    let edited = breakpoint_width_set(&spec, 0, 1100).expect("a free width");
    let after = breakpoint_field_ids("grid-frame", &edited);
    assert_eq!(after[0], before[1], "the 1024 breakpoint is first now and keeps its fields' ids");
    assert!(!after.contains(&before[0]), "the 768 breakpoint's old ids belong to no breakpoint");
    let removed = breakpoint_removed(&spec, 0).expect("the first breakpoint");
    assert_eq!(breakpoint_field_ids("grid-frame", &removed)[0], before[1], "removing the 768 breakpoint keeps the 1024 one's ids");
}

#[test]
fn layout_breakpoint_width_edit_releases_the_row_and_drops_the_text_typed_into_it() {
    let mut h = Harness::new();
    theme::install(&h.ctx);
    h.open_document("canvas", 1);
    let _ = h.sent();
    h.app.active = Some(ArtifactSummary {
        id: "a1".into(),
        kind: ArtifactKind::Canvas,
        title: "문서".into(),
        revision: 1,
        engine_id: "engine".into(),
        dirty: false,
        modified_at_ms: None,
        active_jobs: Vec::new(),
    });
    let spec = grid(vec![breakpoint(768, 2), breakpoint(1024, 4)]);
    let node = grid_node(spec.clone());
    let (width_id, columns_id) = breakpoint_field_ids(&node.id, &spec)[0];
    // The user types 1200 into the width of the 768 breakpoint, then presses Tab to its columns.
    h.ctx.data_mut(|data| data.insert_temp(width_id.with("draft"), "1200".to_owned()));
    draw_breakpoints(&mut h, &node, Some(width_id));
    draw_breakpoints(&mut h, &node, Some(columns_id));

    // The commit sends the list in ascending order. The edited row gives up focus, and the text its old ids held is gone, so the
    // row that now holds the 1024 breakpoint neither shows nor takes what was typed for the 768 one.
    let sent = h.sent();
    let change = sent.iter().find(|request| request.1 == "changes.apply").expect("the width edit is sent");
    let operation: Operation = serde_json::from_value(change.2["operations"][0].clone()).expect("an operation");
    let Operation::SetLayout { layout, .. } = operation else {
        panic!("a breakpoint control emits SetLayout");
    };
    let widths: Vec<u32> = layout.breakpoints.iter().map(|breakpoint| breakpoint.min_width_css_px).collect();
    assert_eq!(widths, vec![1024, 1200]);
    assert_ne!(h.ctx.memory(|memory| memory.focused()), Some(columns_id), "the edited row gives up focus");
    assert!(h.ctx.data(|data| data.get_temp::<String>(columns_id.with("draft"))).is_none(), "the old ids keep no typed text");
}

/// A locked node's breakpoints are shown but not edited: the same typing and Tab as above send nothing (the engine would refuse it).
#[test]
fn a_locked_node_s_breakpoints_send_no_edit() {
    let mut h = Harness::new();
    theme::install(&h.ctx);
    h.open_document("canvas", 1);
    let _ = h.sent();
    h.app.active = Some(ArtifactSummary {
        id: "a1".into(),
        kind: ArtifactKind::Canvas,
        title: "문서".into(),
        revision: 1,
        engine_id: "engine".into(),
        dirty: false,
        modified_at_ms: None,
        active_jobs: Vec::new(),
    });
    let spec = grid(vec![breakpoint(768, 2), breakpoint(1024, 4)]);
    let node = NodeProperties { locked: Some(true), ..grid_node(spec.clone()) };
    let (width_id, columns_id) = breakpoint_field_ids(&node.id, &spec)[0];
    h.ctx.data_mut(|data| data.insert_temp(width_id.with("draft"), "1200".to_owned()));
    draw_breakpoints(&mut h, &node, Some(width_id));
    draw_breakpoints(&mut h, &node, Some(columns_id));
    assert!(!h.sent().iter().any(|request| request.1 == "changes.apply"), "a locked node's breakpoint is not edited");
    let unlocked = grid_node(spec);
    assert!(all_unlocked(&unlocked, &[]) && !all_unlocked(&unlocked, std::slice::from_ref(&node)), "one locked node locks the fill of the selection");
}

/// The words of the breakpoint limits name the engine's limits (backlog desktop 48: the wording was not tied to the constants).
#[test]
fn the_breakpoint_limit_words_name_the_engine_limits() {
    let full = grid((0..designllm_contracts::MAX_BREAKPOINTS as u32).map(|n| breakpoint(100 + n * 10, 2)).collect());
    let refused = breakpoint_added(&full).expect_err("the list is full");
    assert!(refused.contains(&format!("{}개", designllm_contracts::MAX_BREAKPOINTS)), "{refused}");
    let spec = grid(vec![breakpoint(768, 2)]);
    let refused = breakpoint_columns_set(&spec, 0, designllm_contracts::MAX_GRID_COLUMNS + 1).expect_err("too many columns");
    assert!(refused.contains(&format!("1에서 {} 사이", designllm_contracts::MAX_GRID_COLUMNS)), "{refused}");
    assert!(breakpoint_columns_set(&spec, 0, designllm_contracts::MAX_GRID_COLUMNS).is_ok(), "the limit itself is taken");
}
