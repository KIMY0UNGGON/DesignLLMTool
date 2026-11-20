//! Task 63 item 5: an undo keeps the selection of the nodes the document still has.

use serde_json::json;

use super::{link_flow_tests::canvas_with_three_boxes, test_support::Window};

#[test]
fn undoing_an_alignment_keeps_every_selected_box_selected() {
    let mut host = super::test_support::Host::start();
    // c already sits on the shared right edge, so the alignment moves only a and b.
    let canvas = canvas_with_three_boxes(&mut host, [500_000, 0, 3_000_000]);
    let mut window = Window::open(&mut host, &canvas);
    window.step(&mut host, "select.many", json!({ "node_ids": ["a", "b", "c"] })).expect("three boxes");
    assert_eq!(window.app.selected_nodes, ["a", "b", "c"], "three boxes are selected");
    window.step(&mut host, "props.align", json!({ "edge": "right" })).expect("aligned");
    assert_eq!(window.app.selected_nodes, ["a", "b", "c"], "the alignment keeps the selection");

    window.step(&mut host, "edit.undo", json!({})).expect("undone");

    assert_eq!(window.app.selected_nodes, ["a", "b", "c"], "the undo keeps every box that is still in the document");
    let host_selection = &window.app.context.as_ref().expect("the host's context").selection.node_ids;
    assert_eq!(host_selection, &["a", "b", "c"], "and so does the host");
}
