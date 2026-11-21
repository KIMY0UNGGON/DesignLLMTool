//! Tests of the page guides: the overlay mapping at each pan and zoom, the snap window of each zoom, the
//! zero-extent rules, and the guide commands against a real host. A guide change carries the stored setting
//! as `expected`, keeps the other axis, and a window whose revision has moved sends nothing.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::mpsc::{Receiver, Sender},
    time::Instant,
};

use designllm_contracts::{
    layout_guides::{ActiveEdge, EmuRect, GuideError, Point, ResizeHandle, SnapFeature, SnapHit},
    ArtifactBody, ArtifactSummary, CanvasAxisGuides, CanvasPageGuides, Intent, Node, NodeKind, Operation, Size,
};
use eframe::egui::{Context, Pos2, Rect, Vec2};
use serde_json::{json, Value};

use super::page_guides::{create_snap, guide_positions, move_snap, page_layout, resize_snap, validate_node_extent};
use crate::{
    app::{
        figure_flow_tests::Host,
        link_flow_tests::canvas_with_three_boxes,
        test_support::{canvas_with_page, change_set_arguments, content_of, settle_work, HostBackend, Mcp},
        tests::screen,
        DesktopApp,
    },
    rpc::{RpcPayload, RpcReply, RpcWorker, Work},
    theme,
};

/// One CSS px in EMU.
const EMU_PX: i64 = designllm_contracts::EMU_PER_CSS_PX;
/// The sentence for a guide setting or a page size whose spacing and margins leave no room for a guide track.
const GUIDE_SPACING_SENTENCE: &str = "안내선 간격과 여백이 페이지에 들어가지 않습니다. 간격과 여백을 줄여 주세요";

fn axis(count: u32) -> CanvasAxisGuides {
    CanvasAxisGuides { count, spacing_emu: 0, leading_margin_emu: 0, trailing_margin_emu: 0 }
}

/// A window on `canvas` with a recording RPC: every call the window sends is kept before the host answers it.
struct Window {
    app: DesktopApp,
    replies: Sender<RpcReply>,
    work: Receiver<Work>,
    ctx: Context,
    calls: Vec<(String, Value)>,
}

impl Window {
    fn open(host: &mut Host, canvas: &str) -> Self {
        let (rpc, replies, work) = RpcWorker::detached();
        let ctx = Context::default();
        theme::install(&ctx);
        let mut window = Self { app: DesktopApp::new(rpc, "작업공간".into(), None, None), replies, work, ctx, calls: Vec::new() };
        window.settle(host);
        let list = host.call("workspace.list", json!({})).expect("the work list");
        let summary = list["artifacts"]
            .as_array()
            .and_then(|all| all.iter().find(|item| item["id"] == canvas))
            .cloned()
            .expect("the canvas is in the work list");
        window.app.activate(serde_json::from_value::<ArtifactSummary>(summary).expect("a summary"));
        window.settle(host);
        window.draw();
        window.settle(host);
        window
    }

    /// Whole-window frames: the board learns its rectangle and draws its pages and guides, and the panel draws its sections.
    fn draw(&mut self) {
        for _ in 0..3 {
            let _ = self.ctx.run(screen(1.0, 1280.0, 800.0), |ctx| {
                self.app.draw_frame(ctx);
            });
        }
    }

    /// Plays the host's side until the window has nothing left to ask.
    fn settle(&mut self, host: &mut Host) {
        settle_work(&self.work, &mut self.app, &self.ctx, |item| {
            if let Work::Call { ticket, method, params, .. } = item {
                self.calls.push((method.clone(), params.clone()));
                let result = host.call(&method, params).map(RpcPayload::Json);
                self.replies.send(RpcReply { ticket, result }).expect("the window listens");
            }
        });
    }

    /// Runs one command as the window does, then plays the host's side of what it sent.
    fn invoke(&mut self, host: &mut Host, id: &str, args: Value) -> Result<(), String> {
        let result = self.app.invoke(id, args);
        self.settle(host);
        result
    }

    /// The first operation of the last change set the window sent.
    fn last_operation(&self) -> Value {
        let (_, params) = self.calls.iter().rev().find(|(method, _)| method == "changes.apply").expect("a change set was sent");
        params["operations"][0].clone()
    }

    fn changed_since(&self, mark: usize) -> bool {
        self.calls[mark..].iter().any(|(method, _)| method == "changes.apply")
    }
}

fn canvas_page(host: &mut Host, canvas: &str) -> String {
    match host.artifact(canvas).body {
        ArtifactBody::Canvas(document) => document.pages[0].id.clone(),
        _ => panic!("a canvas"),
    }
}

fn stored_guides(host: &mut Host, canvas: &str) -> BTreeMap<String, CanvasPageGuides> {
    match host.artifact(canvas).body {
        ArtifactBody::Canvas(document) => document.layout_guides,
        _ => panic!("a canvas"),
    }
}

#[test]
fn guide_lines_land_on_the_page_fractions_at_every_pan_and_zoom() {
    let size = Size::a4();
    let layout = page_layout(size, &CanvasPageGuides { columns: Some(axis(4)), rows: Some(axis(2)) }).expect("a valid setting");
    for (zoom, origin) in [(0.5_f32, Pos2::new(40.0, 30.0)), (1.0, Pos2::new(-200.0, 12.5)), (2.5, Pos2::new(7.0, -90.0))] {
        let world = Rect::from_min_size(Pos2::ZERO, Vec2::new(size.width as f32 / EMU_PX as f32, size.height as f32 / EMU_PX as f32));
        let rect = crate::board::to_screen(world, origin, zoom);
        let (xs, ys) = guide_positions(&layout, size, rect);
        assert_eq!(xs.len(), 5, "four columns have five edges");
        assert_eq!(ys.len(), 3, "two rows have three edges");
        for (index, x) in xs.iter().enumerate() {
            let expected = rect.left() + rect.width() * index as f32 / 4.0;
            assert!((x - expected).abs() < 0.01, "zoom {zoom}: column edge {index} at {x}, expected {expected}");
        }
        for (index, y) in ys.iter().enumerate() {
            let expected = rect.top() + rect.height() * index as f32 / 2.0;
            assert!((y - expected).abs() < 0.01, "zoom {zoom}: row edge {index} at {y}, expected {expected}");
        }
    }
}

#[test]
fn a_move_snaps_only_inside_the_eight_physical_pixel_window_of_its_zoom() {
    let size = Size { width: 1_000_000, height: 800_000 };
    let layout = page_layout(size, &CanvasPageGuides { columns: Some(axis(2)), rows: None }).expect("a valid setting");
    // The line is at 500_000 and the moved start edge is 50_000 EMU away from it.
    let moved = EmuRect { x_emu: 550_000, y_emu: 0, width_emu: 100_000, height_emu: 100_000 };
    // At zoom 1 the window is 76_200 EMU, so the edge snaps.
    let at_one = move_snap(Some(&layout), moved, 1.0, 1.0).expect("a valid snap");
    assert_eq!(at_one.rect.x_emu, 500_000);
    assert_eq!(at_one.x, Some(SnapHit { line_emu: 500_000, feature: SnapFeature::Start }));
    // At zoom 4 the window is 19_050 EMU, so the same move stays where the pointer put it.
    let at_four = move_snap(Some(&layout), moved, 4.0, 1.0).expect("a valid snap");
    assert_eq!(at_four.rect, moved);
    assert_eq!(at_four.x, None);
    // Two physical pixels per point at zoom 2 is also 4 pixels per EMU step: the same 19_050 window.
    assert_eq!(move_snap(Some(&layout), moved, 2.0, 2.0).expect("a valid snap").rect, moved);
    // Without guides there is nothing to snap to, and a zero zoom has no scale.
    assert_eq!(move_snap(None, moved, 1.0, 1.0).expect("a valid snap").rect, moved);
    assert_eq!(move_snap(Some(&layout), moved, 0.0, 1.0), Err(GuideError::InvalidViewScale));
}

#[test]
fn a_resize_moves_only_the_dragged_edge_and_only_inside_the_window() {
    let size = Size { width: 1_000_000, height: 800_000 };
    let layout = page_layout(size, &CanvasPageGuides { columns: Some(axis(2)), rows: None }).expect("a valid setting");
    // The right edge is 100_000 EMU short of the line at 500_000: outside the 76_200 window at zoom 1.
    let rect = EmuRect { x_emu: 0, y_emu: 0, width_emu: 400_000, height_emu: 100_000 };
    let end = ResizeHandle { x: Some(ActiveEdge::End), y: None };
    let near = resize_snap(Some(&layout), rect, end, 1.0, 1.0).expect("a valid snap");
    assert_eq!(near.rect, rect);
    assert_eq!(near.x, None);
    // At zoom 0.5 the window is 152_400 EMU: the edge snaps and the left edge stays at 0.
    let far = resize_snap(Some(&layout), rect, end, 0.5, 1.0).expect("a valid snap");
    assert_eq!(far.rect, EmuRect { x_emu: 0, y_emu: 0, width_emu: 500_000, height_emu: 100_000 });
    assert_eq!(far.x, Some(SnapHit { line_emu: 500_000, feature: SnapFeature::End }));
}

#[test]
fn a_creation_snaps_its_corners_and_a_click_stays_a_refused_zero_rectangle() {
    let size = Size { width: 1_000_000, height: 800_000 };
    let layout = page_layout(size, &CanvasPageGuides { columns: Some(axis(2)), rows: None }).expect("a valid setting");
    // The anchor is 2 EMU from the left edge; the cursor is 30_000 EMU short of the line at 500_000.
    let anchor = Point { x_emu: 2, y_emu: 10 };
    let cursor = Point { x_emu: 530_000, y_emu: 90 };
    let at_one = create_snap(Some(&layout), anchor, cursor, 1.0, 1.0).expect("a valid snap");
    assert_eq!(at_one.anchor.x_emu, 0);
    assert_eq!(at_one.cursor.x_emu, 500_000);
    assert_eq!(at_one.cursor_x, Some(SnapHit { line_emu: 500_000, feature: SnapFeature::End }));
    assert_eq!(at_one.rect, EmuRect { x_emu: 0, y_emu: 10, width_emu: 500_000, height_emu: 80 });
    // At zoom 4 the cursor is outside the 19_050 window; the anchor is still inside it.
    let at_four = create_snap(Some(&layout), anchor, cursor, 4.0, 1.0).expect("a valid snap");
    assert_eq!(at_four.cursor_x, None);
    assert_eq!(at_four.rect.width_emu, 530_000);

    // A click moves nothing after the anchor snap: the result is 0 × 0, which every shape refuses.
    let click = create_snap(Some(&layout), Point { x_emu: 2, y_emu: 2 }, Point { x_emu: 2, y_emu: 2 }, 1.0, 1.0).expect("a valid snap");
    assert_eq!((click.rect.width_emu, click.rect.height_emu), (0, 0));
    assert!(validate_node_extent(NodeKind::Rectangle, click.rect.width_emu, click.rect.height_emu).is_err());
    assert!(validate_node_extent(NodeKind::Line, click.rect.width_emu, click.rect.height_emu).is_err());
}

#[test]
fn zero_extent_rules_refuse_0x0_and_generic_zero_sides_but_keep_one_zero_side_on_a_line() {
    assert!(validate_node_extent(NodeKind::Rectangle, 10, 10).is_ok());
    assert!(validate_node_extent(NodeKind::Rectangle, 0, 0).is_err());
    assert!(validate_node_extent(NodeKind::Rectangle, 0, 10).is_err());
    assert!(validate_node_extent(NodeKind::Text, 10, 0).is_err());
    assert!(validate_node_extent(NodeKind::Rectangle, -1, 10).is_err());
    assert!(validate_node_extent(NodeKind::Line, 100, 0).is_ok(), "a horizontal line");
    assert!(validate_node_extent(NodeKind::Line, 0, 100).is_ok(), "a vertical line");
    assert!(validate_node_extent(NodeKind::Line, 0, 0).is_err());
    assert!(validate_node_extent(NodeKind::Line, -1, 100).is_err());
}

#[test]
fn props_size_applies_the_zero_extent_rules_to_the_selected_node() {
    let mut host = Host::start();
    let canvas = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
    let page = canvas_page(&mut host, &canvas);
    let line: Node = serde_json::from_value(json!({
        "id": "rule", "kind": "line", "bounds": { "x": 0, "y": 3_000_000, "width": 2_000_000, "height": 0 }
    }))
    .expect("a line node parses");
    host.apply(
        &canvas,
        vec![Operation::AddNode { page_id: page, parent_id: None, node: line }],
        [Intent::Content, Intent::Design].into_iter().collect(),
    )
    .expect("the line goes in");
    let mut window = Window::open(&mut host, &canvas);

    // A text box cannot be 0 high.
    window.invoke(&mut host, "select.node", json!({ "node_id": "a" })).expect("the box is selected");
    assert!(window.invoke(&mut host, "props.size", json!({ "height": 0 })).is_err());
    // A horizontal line has a 0 height, so setting its width to 0 would make it 0 × 0: refused.
    window.invoke(&mut host, "select.node", json!({ "node_id": "rule" })).expect("the line is selected");
    let mark = window.calls.len();
    assert!(window.invoke(&mut host, "props.size", json!({ "width": 0 })).is_err());
    assert!(!window.changed_since(mark), "a refused size sends nothing");
    // Its height becomes 50 px, then its width becomes 0: a vertical line, which is allowed.
    window.invoke(&mut host, "props.size", json!({ "height": 50 })).expect("the height is set");
    window.invoke(&mut host, "props.size", json!({ "width": 0 })).expect("a vertical line is allowed");
    let applied = window.last_operation();
    assert_eq!(applied["op"], "set_geometry");
    assert_eq!(applied["bounds"]["width"], 0);
    assert_eq!(applied["bounds"]["height"], 50 * EMU_PX);
}

#[test]
fn guide_commands_send_the_stored_setting_as_expected_and_keep_the_other_axis() {
    let mut host = Host::start();
    let canvas = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
    let page = canvas_page(&mut host, &canvas);
    let mut window = Window::open(&mut host, &canvas);

    window
        .invoke(&mut host, "canvas.guides.set", json!({ "axis": "columns", "count": 4, "spacing_emu": 0, "leading_margin_emu": 0, "trailing_margin_emu": 0 }))
        .expect("the columns are set");
    let first = window.last_operation();
    assert_eq!(first["op"], "set_canvas_page_guides");
    assert_eq!(first["expected"], Value::Null, "nothing was stored before");
    assert_eq!(first["guides"]["columns"]["count"], 4);
    assert!(first["guides"].get("rows").is_none());
    assert_eq!(stored_guides(&mut host, &canvas).get(&page).and_then(|guides| guides.columns).map(|axis| axis.count), Some(4));

    window
        .invoke(
            &mut host,
            "canvas.guides.set",
            json!({ "axis": "rows", "count": 2, "spacing_emu": 9_525, "leading_margin_emu": 0, "trailing_margin_emu": 0 }),
        )
        .expect("the rows are set");
    let second = window.last_operation();
    assert_eq!(second["expected"]["columns"]["count"], 4, "the stored columns are the expected value");
    assert_eq!(second["guides"]["columns"]["count"], 4, "the other axis is kept");
    assert_eq!(second["guides"]["rows"]["count"], 2);
    assert_eq!(second["guides"]["rows"]["spacing_emu"], 9_525);
    window.draw();

    window.invoke(&mut host, "canvas.guides.clear", json!({ "axis": "columns" })).expect("the columns are cleared");
    let third = window.last_operation();
    assert_eq!(third["expected"]["columns"]["count"], 4);
    assert_eq!(third["expected"]["rows"]["count"], 2);
    assert!(third["guides"].get("columns").is_none());
    assert_eq!(third["guides"]["rows"]["count"], 2);

    window.invoke(&mut host, "canvas.guides.clear", json!({})).expect("the page guides are cleared");
    let fourth = window.last_operation();
    assert_eq!(fourth["expected"]["rows"]["count"], 2);
    assert_eq!(fourth["guides"], Value::Null);
    assert!(stored_guides(&mut host, &canvas).is_empty());
}

#[test]
fn a_window_whose_revision_moved_refuses_a_guide_change_and_sends_nothing() {
    let mut host = Host::start();
    let canvas = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
    let mut window = Window::open(&mut host, &canvas);
    window.app.context.as_mut().expect("the window has a context").revision += 1;
    let mark = window.calls.len();
    let result = window.invoke(
        &mut host,
        "canvas.guides.set",
        json!({ "axis": "columns", "count": 2, "spacing_emu": 0, "leading_margin_emu": 0, "trailing_margin_emu": 0 }),
    );
    assert!(result.is_err(), "a moved revision is refused");
    assert!(!window.changed_since(mark), "nothing is sent");
    assert!(stored_guides(&mut host, &canvas).is_empty());
}

#[test]
fn the_engine_refuses_a_guide_change_whose_expected_value_is_not_the_stored_one() {
    let mut host = Host::start();
    let canvas = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
    let page = canvas_page(&mut host, &canvas);
    let intents: BTreeSet<Intent> = [Intent::Design].into_iter().collect();
    let first = CanvasPageGuides { columns: Some(axis(2)), rows: None };
    host.apply(
        &canvas,
        vec![Operation::SetCanvasPageGuides { page_id: page.clone(), expected: None, guides: Some(first.clone()) }],
        intents.clone(),
    )
    .expect("the first setting");
    // This change still expects that nothing is stored. The engine refuses it and keeps the first setting.
    let error = host
        .apply(
            &canvas,
            vec![Operation::SetCanvasPageGuides {
                page_id: page.clone(),
                expected: None,
                guides: Some(CanvasPageGuides { columns: None, rows: Some(axis(3)) }),
            }],
            intents,
        )
        .expect_err("a stale expected value is refused");
    assert!(error.diagnostics.iter().any(|diagnostic| diagnostic.code == "CANVAS_GUIDES_STALE"), "{error:?}");
    assert_eq!(stored_guides(&mut host, &canvas).get(&page), Some(&first));
}

#[test]
fn the_guide_switches_flip_the_view_and_change_no_document() {
    let mut host = Host::start();
    let canvas = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
    let mut window = Window::open(&mut host, &canvas);
    assert!(window.app.view_ref().guides_visible && window.app.view_ref().snap_guides, "both are on by default");
    let mark = window.calls.len();
    window.invoke(&mut host, "view.toggle_guides", json!({})).expect("the guides hide");
    window.invoke(&mut host, "view.toggle_snap", json!({})).expect("the snapping turns off");
    assert!(!window.app.view_ref().guides_visible);
    assert!(!window.app.view_ref().snap_guides);
    assert!(!window.changed_since(mark), "a view switch changes no document");
    window.invoke(&mut host, "view.toggle_guides", json!({})).expect("the guides show again");
    assert!(window.app.view_ref().guides_visible);
}

/// The page size of the first page of `canvas`.
fn page_size(host: &mut Host, canvas: &str) -> Size {
    match host.artifact(canvas).body {
        ArtifactBody::Canvas(document) => document.pages[0].size,
        _ => panic!("a canvas"),
    }
}

/// The frame of the node `id` on the first page of `canvas`.
fn node_bounds(host: &mut Host, canvas: &str, id: &str) -> designllm_contracts::Rect {
    match host.artifact(canvas).body {
        ArtifactBody::Canvas(document) => document.pages[0].nodes.iter().find(|node| node.id == id).expect("the node is on the page").bounds,
        _ => panic!("a canvas"),
    }
}

/// On a multi-selection each node takes the zero-extent rule of its own kind: two Lines may take 0 on one side, while a
/// Line with both sides 0 and a text box with one side 0 are refused, and nothing is sent for a refusal.
#[test]
fn a_multi_selection_size_follows_the_zero_extent_rule_of_each_node() {
    let mut host = Host::start();
    let canvas = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
    let page = canvas_page(&mut host, &canvas);
    let lines: Vec<Operation> = [("r1", 3_000_000), ("r2", 4_000_000)]
        .into_iter()
        .map(|(id, y)| {
            let node: Node = serde_json::from_value(json!({
                "id": id, "kind": "line", "bounds": { "x": 0, "y": y, "width": 2_000_000, "height": 0 }
            }))
            .expect("a line node parses");
            Operation::AddNode { page_id: page.clone(), parent_id: None, node }
        })
        .collect();
    host.apply(&canvas, lines, [Intent::Content, Intent::Design].into_iter().collect()).expect("the lines go in");
    let mut window = Window::open(&mut host, &canvas);
    window.invoke(&mut host, "select.many", json!({ "node_ids": ["r1", "r2"] })).expect("the two lines are selected");

    // Both lines would be 0 × 0: refused, and nothing is sent.
    let mark = window.calls.len();
    assert!(window.invoke(&mut host, "props.size", json!({ "width": 0 })).is_err());
    assert!(!window.changed_since(mark), "a refused size sends nothing");
    // Their height becomes 50 px, then their width becomes 0: two vertical lines, which are allowed. A change leaves only the
    // first node selected (the window restores one node after a change), so both lines are chosen again for the second step.
    window.invoke(&mut host, "props.size", json!({ "height": 50 })).expect("the height is set on both lines");
    window.invoke(&mut host, "select.many", json!({ "node_ids": ["r1", "r2"] })).expect("both lines are chosen again");
    window.invoke(&mut host, "props.size", json!({ "width": 0 })).expect("two vertical lines are allowed");
    assert_eq!(node_bounds(&mut host, &canvas, "r1"), designllm_contracts::Rect { x: 0, y: 3_000_000, width: 0, height: 50 * EMU_PX });
    assert_eq!(node_bounds(&mut host, &canvas, "r2"), designllm_contracts::Rect { x: 0, y: 4_000_000, width: 0, height: 50 * EMU_PX });

    // A negative side is refused.
    assert!(window.invoke(&mut host, "props.size", json!({ "width": -1 })).is_err());
    // A text box in the selection cannot be 0 high, so the line beside it does not change either.
    window.invoke(&mut host, "select.many", json!({ "node_ids": ["a", "r1"] })).expect("a box and a line are selected");
    let mark = window.calls.len();
    assert!(window.invoke(&mut host, "props.size", json!({ "height": 0 })).is_err());
    assert!(!window.changed_since(mark), "a refused size sends nothing");
    assert_eq!(node_bounds(&mut host, &canvas, "r1"), designllm_contracts::Rect { x: 0, y: 3_000_000, width: 0, height: 50 * EMU_PX });
}

/// A page shrink that leaves a guide track no room is refused by the engine. The window shows the sentence that tells the
/// user to narrow the guide spacing and margins first, and the page keeps its size.
#[test]
fn a_page_shrink_that_leaves_no_guide_room_shows_the_sentence_to_narrow_the_guides_first() {
    let mut host = Host::start();
    let canvas = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
    let mut window = Window::open(&mut host, &canvas);
    window
        .invoke(&mut host, "canvas.guides.set", json!({ "axis": "columns", "count": 1, "spacing_emu": 0, "leading_margin_emu": 10_000, "trailing_margin_emu": 10_000 }))
        .expect("one column with margins");
    let size = page_size(&mut host, &canvas);
    // One pixel is 9_525 EMU, less than the 20_000 EMU of the margins: the engine refuses the page.
    window.invoke(&mut host, "props.page_size", json!({ "width": 1, "height": size.height as f64 / EMU_PX as f64 })).expect("the size is sent");
    assert_eq!(page_size(&mut host, &canvas), size, "the refused size changes nothing");
    let shown = window.app.notices.current(Instant::now()).expect("the refusal is shown").text.clone();
    assert_eq!(shown, GUIDE_SPACING_SENTENCE);
}

/// A guide setting that leaves no room for a track is refused and stores nothing. The window shows the same sentence.
#[test]
fn a_guide_setting_without_track_room_shows_the_sentence_about_the_guide_spacing() {
    let mut host = Host::start();
    let canvas = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
    let mut window = Window::open(&mut host, &canvas);
    let width = page_size(&mut host, &canvas).width;
    // The leading margin is the whole page width, so the one column has no room.
    window
        .invoke(&mut host, "canvas.guides.set", json!({ "axis": "columns", "count": 1, "spacing_emu": 0, "leading_margin_emu": width, "trailing_margin_emu": 0 }))
        .expect("the setting is sent");
    assert!(stored_guides(&mut host, &canvas).is_empty(), "the refused setting stores nothing");
    let shown = window.app.notices.current(Instant::now()).expect("the refusal is shown").text.clone();
    assert_eq!(shown, GUIDE_SPACING_SENTENCE);
}

/// The guide setting sent by the guide command and by the MCP `changes_apply` tool leaves the same canvas.
#[test]
fn the_guide_setting_gives_the_same_canvas_by_command_and_by_tool() {
    let mut host = Host::start();
    let gui = canvas_with_page(&mut host, "안내선");
    let mcp_canvas = canvas_with_page(&mut host, "안내선");
    let before = host.artifact(&gui);

    let mut window = Window::open(&mut host, &gui);
    window
        .invoke(&mut host, "canvas.guides.set", json!({ "axis": "columns", "count": 4, "spacing_emu": 0, "leading_margin_emu": 0, "trailing_margin_emu": 0 }))
        .expect("the columns are set");
    let gui_after = host.artifact(&gui);
    drop(window);

    let backend = HostBackend::new(host);
    let mcp = Mcp::connect(backend.clone());
    let (workspace, revision) = backend.with(|host| (host.workspace_id.clone(), host.artifact(&mcp_canvas).revision));
    let operation = Operation::SetCanvasPageGuides { page_id: "page-1".into(), expected: None, guides: Some(CanvasPageGuides { columns: Some(axis(4)), rows: None }) };
    mcp.call("changes_apply", change_set_arguments(&workspace, &mcp_canvas, revision, &[Intent::Design], vec![operation]));
    let mcp_after = backend.with(|host| host.artifact(&mcp_canvas));

    assert_ne!(content_of(&before), content_of(&gui_after), "the command changed nothing");
    assert_eq!(content_of(&gui_after), content_of(&mcp_after), "the command and the MCP tool left different content");
}

/// A clear that leaves the page as it is sends no change set and creates no revision: a page without guides, and an axis that
/// is not set while the other one is.
#[test]
fn a_clear_that_leaves_the_page_as_it_is_sends_no_change() {
    let mut host = Host::start();
    let canvas = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
    let page = canvas_page(&mut host, &canvas);
    let mut window = Window::open(&mut host, &canvas);
    let revision = host.artifact(&canvas).revision;
    let mark = window.calls.len();
    window.invoke(&mut host, "canvas.guides.clear", json!({})).expect("a clear of nothing is not refused");
    window.invoke(&mut host, "canvas.guides.clear", json!({ "axis": "rows" })).expect("a clear of an axis that is not set is not refused");
    assert!(!window.changed_since(mark), "no change set is sent");
    assert_eq!(host.artifact(&canvas).revision, revision, "no revision is created");

    window
        .invoke(&mut host, "canvas.guides.set", json!({ "axis": "columns", "count": 2, "spacing_emu": 0, "leading_margin_emu": 0, "trailing_margin_emu": 0 }))
        .expect("the columns are set");
    let mark = window.calls.len();
    window.invoke(&mut host, "canvas.guides.clear", json!({ "axis": "rows" })).expect("the rows are not set");
    assert!(!window.changed_since(mark), "clearing the rows, which are not set, changes nothing");
    assert_eq!(stored_guides(&mut host, &canvas).get(&page).and_then(|guides| guides.columns).map(|axis| axis.count), Some(2), "the columns stay");
}

/// A drag whose zoom changes before the drop snaps with the window of the zoom it started at: the move was measured on the page
/// as drawn then. A drop 6 physical px beside a guide snaps (8 px at the start zoom), though at four times the zoom it would not.
#[test]
fn a_drop_after_a_zoom_change_snaps_with_the_window_of_the_starting_zoom() {
    let mut host = Host::start();
    let canvas = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
    let mut window = Window::open(&mut host, &canvas);
    window
        .invoke(&mut host, "canvas.guides.set", json!({ "axis": "columns", "count": 2, "spacing_emu": 0, "leading_margin_emu": 0, "trailing_margin_emu": 0 }))
        .expect("the columns are set");
    window.draw();
    let (page_size, guide, before) = {
        let artifact = host.artifact(&canvas);
        let ArtifactBody::Canvas(document) = &artifact.body else { panic!("a canvas") };
        let page = &document.pages[0];
        let layout = super::page_guides::page_guide_layout(document, page).expect("the guides fit the page");
        let node = page.nodes.iter().find(|node| node.id == "b").expect("the box").bounds;
        (page.size, layout.vertical_lines()[1], node)
    };
    let view = window.app.view_ref();
    let board_rect = window.app.board_rect.expect("the board is laid out");
    let slots = crate::board::arrange(designllm_contracts::ArtifactKind::Canvas, &window.app.page_sizes(), view.zoom);
    let slot = &slots[0];
    let page_rect = crate::board::to_screen(slot.world, board_rect.min + view.pan, view.zoom);
    let to_screen_x = |emu: i64| page_rect.left() + (emu as f64 / page_size.width as f64 * f64::from(page_rect.width())) as f32;
    let to_screen_y = |emu: i64| page_rect.top() + (emu as f64 / page_size.height as f64 * f64::from(page_rect.height())) as f32;
    let start = Pos2::new(to_screen_x(before.x + before.width / 2), to_screen_y(before.y + before.height / 2));
    window.app.start_drag_at_scale("b".into(), 0, before, page_size, page_rect, start, 1.0);
    window.settle(&mut host);
    assert!(window.app.drag.as_ref().is_some_and(|drag| drag.reservation_id.is_some()), "the host grants the drag");
    let distance = (6.0 * EMU_PX as f64 / f64::from(view.zoom)).round() as i64;
    let wanted = guide + distance;
    let delta = Vec2::new(((wanted - before.x) as f64 / page_size.width as f64 * f64::from(page_rect.width())) as f32, 0.0);
    // The zoom changes while the drag is held.
    window.app.view().zoom = view.zoom * 4.0;
    let drag = window.app.drag.as_mut().expect("the drag is active");
    drag.delta = delta;
    drag.released = true;
    window.app.commit_drag_if_reserved();
    window.settle(&mut host);
    let artifact = host.artifact(&canvas);
    let ArtifactBody::Canvas(document) = &artifact.body else { panic!("a canvas") };
    let x = document.pages[0].nodes.iter().find(|node| node.id == "b").expect("the box").bounds.x;
    assert_ne!(wanted, guide);
    assert_eq!(x, guide, "6 physical px at the starting zoom is inside its 8 px window");
}

/// The two guide check boxes are recorded under their names (`열 가이드`, `행 가이드`) and draw no text of their own: the row
/// labels `열` and `행` are the only words beside them.
#[test]
fn the_guide_check_boxes_are_recorded_by_name_and_draw_no_text_of_their_own() {
    let mut host = Host::start();
    let canvas = canvas_with_three_boxes(&mut host, [0, 1_000_000, 3_000_000]);
    let mut window = Window::open(&mut host, &canvas);
    crate::ui_record::begin();
    let app = &mut window.app;
    let output = window.ctx.run(screen(1.0, 1280.0, 2000.0), |ctx| {
        app.draw_frame(ctx);
    });
    let controls = crate::ui_record::finish();
    let texts = super::test_support::drawn_text(&output);
    for name in ["열 가이드", "행 가이드"] {
        assert!(controls.iter().any(|control| control.label == name), "{name} is recorded: {:?}", controls.iter().map(|c| &c.label).collect::<Vec<_>>());
        assert!(!texts.iter().any(|text| text == name), "{name} is not drawn as text");
    }
    assert!(texts.iter().any(|text| text == "열") && texts.iter().any(|text| text == "행"), "the row labels are drawn: {texts:?}");
}
