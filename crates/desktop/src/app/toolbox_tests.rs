//! Tests of the toolbox tab (68g) on a real host and window, every frame through `App::raw_input_hook` and `App::update` as
//! eframe runs them: a tile click inserts at the visible centre (also zoomed and panned), a tile dragged onto the board lands
//! where the pointer is let go, on that page, which becomes the current one, with a preview of the object's own size; a drop
//! off the pages, over the sidebar, the panel or the floating tool bar, outside the window or after Esc is cancelled; one
//! undo takes an insert back; the `at` argument places the object the same way for an agent and refuses a bad page; the tab
//! is a canvas's and a slide deck's only; the memo tile keeps the board's memo conditions; an icon waiting for its pick does
//! not outlive its tab.

use designllm_contracts::{ArtifactBody, ArtifactSummary, Intent, Node, NodeKind, Operation, Page, Rect as ModelRect, Size, EMU_PER_CSS_PX};
use eframe::egui::{epaint::Shape, Event, Id, Key, Modifiers, Pos2, Rect, Vec2};
use serde_json::{json, Value};

use super::{
    board::Tool,
    screen_to_page_emu,
    test_support::{canvas_with_page, primary_button, screen, Host, Window},
    toolbox::{PendingIcon, PENDING_ICON},
};
use crate::ui_record;

/// A window on one artifact whose frames run as eframe runs them: the raw input hook, then `App::update`, then the host's
/// answers to what the frame sent. The shapes of the last frame are kept.
struct Bench {
    host: Host,
    window: Window,
    artifact: String,
    clock: f64,
    shapes: Vec<eframe::epaint::ClippedShape>,
}

impl Bench {
    fn open(mut host: Host, artifact: String) -> Self {
        let window = Window::open_with_previews(&mut host, &artifact);
        let mut bench = Self { host, window, artifact, clock: 1.0, shapes: Vec::new() };
        for _ in 0..4 {
            bench.frame(Vec::new());
        }
        bench
    }

    /// A fresh canvas whose one page is `page-1`, with the toolbox tab open.
    fn canvas() -> Self {
        let mut host = Host::start();
        let canvas = canvas_with_page(&mut host, "도구 상자");
        let mut bench = Self::open(host, canvas);
        bench.tools();
        bench
    }

    fn frame(&mut self, events: Vec<Event>) {
        self.clock += 0.05;
        let mut input = screen(1.0, 1280.0, 800.0);
        input.time = Some(self.clock);
        input.events = events;
        let ctx = self.window.ctx.clone();
        eframe::App::raw_input_hook(&mut self.window.app, &ctx, &mut input);
        let mut frame = eframe::Frame::_new_kittest();
        let output = ctx.run(input, |ctx| eframe::App::update(&mut self.window.app, ctx, &mut frame));
        self.shapes = output.shapes;
        self.window.settle(&mut self.host);
    }

    fn frames(&mut self, count: usize) {
        for _ in 0..count {
            self.frame(Vec::new());
        }
    }

    /// Runs a registered command as the window does, then draws.
    fn step(&mut self, id: &str, args: Value) -> Result<(), String> {
        let result = self.window.step(&mut self.host, id, args);
        self.frames(2);
        result
    }

    fn tools(&mut self) {
        self.step("nav.tab", json!({ "tab": "tools" })).expect("the toolbox tab opens");
    }

    fn board(&self) -> Rect {
        self.window.app.board_rect.expect("the board is drawn")
    }

    /// The labels and rectangles of the controls drawn left of the board (the sidebar).
    fn sidebar_controls(&mut self) -> Vec<(String, Rect)> {
        ui_record::begin();
        self.frame(Vec::new());
        let board = self.board();
        ui_record::finish()
            .into_iter()
            .filter(|control| control.enabled && control.visible && control.rect.max.x <= board.min.x)
            .map(|control| (control.label, control.rect))
            .collect()
    }

    /// The rectangle of the one tile named `label` in the sidebar.
    fn tile(&mut self, label: &str) -> Rect {
        let found: Vec<Rect> = self.sidebar_controls().into_iter().filter(|(name, _)| name == label).map(|(_, rect)| rect).collect();
        assert_eq!(found.len(), 1, "one tile named {label}");
        found[0]
    }

    fn click(&mut self, at: Pos2) {
        self.frame(vec![Event::PointerMoved(at), primary_button(at, true), primary_button(at, false)]);
        self.frame(Vec::new());
    }

    fn click_tile(&mut self, label: &str) {
        let tile = self.tile(label).center();
        self.click(tile);
    }

    /// Presses on the tile `label` and moves the pointer to `to` in steps; the button stays down.
    fn drag_tile_to(&mut self, label: &str, to: Pos2) {
        let from = self.tile(label).center();
        self.frame(vec![Event::PointerMoved(from), primary_button(from, true)]);
        for step in 1..=6 {
            let at = from + (to - from) * (step as f32 / 6.0);
            self.frame(vec![Event::PointerMoved(at)]);
        }
    }

    fn release_at(&mut self, at: Pos2) {
        self.frame(vec![Event::PointerMoved(at), primary_button(at, false)]);
        self.frames(2);
    }

    fn drop_tile_at(&mut self, label: &str, to: Pos2) {
        self.drag_tile_to(label, to);
        self.release_at(to);
    }

    fn page_rect(&self, index: u32) -> Rect {
        self.window.app.toolbox_page_rects().expect("the board is drawn").into_iter().find(|(page, _)| *page == index).expect("the page is drawn").1
    }

    fn page_size(&mut self, index: usize) -> Size {
        match &self.host.artifact(&self.artifact).body {
            ArtifactBody::Canvas(document) => document.pages[index].size,
            ArtifactBody::Slides(document) => document.pages[index].size,
            _ => panic!("a canvas or slides"),
        }
    }

    fn nodes(&mut self, index: usize) -> Vec<Node> {
        match &self.host.artifact(&self.artifact).body {
            ArtifactBody::Canvas(document) => document.pages[index].nodes.clone(),
            ArtifactBody::Slides(document) => document.pages[index].nodes.clone(),
            _ => panic!("a canvas or slides"),
        }
    }

    /// The page EMU of a screen point over the page `index` as it is drawn now.
    fn emu_at(&mut self, index: u32, point: Pos2) -> (i64, i64) {
        let size = self.page_size(index as usize);
        screen_to_page_emu(point, self.page_rect(index), size).expect("the point maps onto the page")
    }

    /// The centre of the part of page `index` on the board, in page EMU.
    fn visible_centre(&mut self, index: u32) -> (i64, i64) {
        let point = self.page_rect(index).intersect(self.board()).center();
        self.emu_at(index, point)
    }

    /// Two screen points of page `index` in page EMU, plus one px: the rounding of a screen point.
    fn slack(&mut self, index: u32) -> i64 {
        let size = self.page_size(index as usize);
        (size.width as f64 / self.page_rect(index).width() as f64 * 2.0).ceil() as i64 + EMU_PER_CSS_PX
    }

    /// The outline the last frame drew for a dragged tile (the toolbox preview's fill), if any.
    fn preview(&self) -> Option<Rect> {
        let palette = crate::theme::palette(&self.window.ctx);
        let fill = palette.accent_soft.gamma_multiply(0.5);
        self.shapes.iter().find_map(|clipped| match &clipped.shape {
            Shape::Rect(rect) if rect.fill == fill => Some(rect.rect),
            _ => None,
        })
    }

    fn pending_icon(&self) -> Option<PendingIcon> {
        self.window.ctx.data(|data| data.get_temp::<PendingIcon>(Id::new(PENDING_ICON)))
    }
}

fn centre(bounds: &ModelRect) -> (i64, i64) {
    (bounds.x + bounds.width / 2, bounds.y + bounds.height / 2)
}

fn near(got: (i64, i64), want: (i64, i64), slack: i64) -> bool {
    (got.0 - want.0).abs() <= slack && (got.1 - want.1).abs() <= slack
}

fn escape() -> Event {
    Event::Key { key: Key::Escape, physical_key: None, pressed: true, repeat: false, modifiers: Modifiers::NONE }
}

/// A canvas with the pages `page-1` and `page-2`, no nodes.
fn two_page_canvas(host: &mut Host) -> String {
    let canvas = canvas_with_page(host, "두 쪽");
    let intents = [Intent::Content, Intent::Design].into_iter().collect();
    host.apply(&canvas, vec![Operation::InsertPage { index: 1, page: Page::new("page-2", Size::slide()) }], intents).expect("the second page");
    canvas
}

#[test]
fn a_tile_click_inserts_at_the_visible_centre_and_one_undo_takes_it_back() {
    let mut bench = Bench::canvas();
    bench.click_tile("텍스트");
    let nodes = bench.nodes(0);
    assert_eq!(nodes.len(), 1, "the click adds one text node");
    assert_eq!(nodes[0].kind, NodeKind::Text);
    let (want, slack) = (bench.visible_centre(0), bench.slack(0));
    assert!(near(centre(&nodes[0].bounds), want, slack), "centre {:?} is the visible centre {want:?}", centre(&nodes[0].bounds));

    bench.step("edit.undo", json!({})).expect("the insert can be undone");
    assert!(bench.nodes(0).is_empty(), "one undo step removes the tile's object");
}

#[test]
fn a_dragged_tile_shows_its_object_and_lands_where_the_pointer_is_let_go() {
    let mut bench = Bench::canvas();
    let target = bench.page_rect(0).center() + Vec2::new(60.0, -40.0);
    bench.drag_tile_to("사각형", target);
    let preview = bench.preview().expect("the drag over the page shows the rectangle's outline");
    bench.release_at(target);

    let nodes = bench.nodes(0);
    assert_eq!(nodes.len(), 1, "the drop adds one rectangle");
    assert_eq!(nodes[0].kind, NodeKind::Rectangle);
    let (want, slack) = (bench.emu_at(0, target), bench.slack(0));
    assert!(near(centre(&nodes[0].bounds), want, slack), "centre {:?} is the drop point {want:?}", centre(&nodes[0].bounds));
    let size = bench.page_size(0);
    let drawn = super::page_to_screen(nodes[0].bounds, size, bench.page_rect(0));
    assert!((preview.width() - drawn.width()).abs() <= 2.0 && (preview.height() - drawn.height()).abs() <= 2.0, "the preview {preview:?} is the rectangle's size {drawn:?}");
    assert!(bench.preview().is_none(), "the outline goes with the drop");
}

#[test]
fn the_table_preview_is_the_size_of_the_table_the_drop_inserts() {
    let mut bench = Bench::canvas();
    let target = bench.page_rect(0).center() + Vec2::new(-30.0, 20.0);
    bench.drag_tile_to("표", target);
    let preview = bench.preview().expect("the drag shows the table's outline");
    bench.release_at(target);
    let table = bench.nodes(0).into_iter().find(|node| node.kind == NodeKind::Group).expect("the drop adds the table");
    let size = bench.page_size(0);
    let drawn = super::page_to_screen(table.bounds, size, bench.page_rect(0));
    assert!((preview.width() - drawn.width()).abs() <= 2.0 && (preview.height() - drawn.height()).abs() <= 2.0, "the preview {preview:?} is the table's size {drawn:?}");
    let (want, slack) = (bench.emu_at(0, target), bench.slack(0));
    assert!(near(centre(&table.bounds), want, slack), "the table is centred on the drop point");
}

#[test]
fn a_drop_is_cancelled_off_the_pages_off_the_board_over_a_floating_bar_and_outside_the_window() {
    let mut bench = Bench::canvas();
    // Zoomed in: the page reaches under the sidebar, the panel and the tool bar.
    let board = bench.board();
    bench.window.app.zoom_about(board, board.center(), 3.0);
    bench.window.app.view().pan.y += 300.0;
    bench.frames(3);
    let page = bench.page_rect(0);
    let sidebar = Pos2::new(board.left() - 30.0, board.center().y);
    let panel = Pos2::new(board.right() + 30.0, board.center().y);
    let bar = bench.window.ctx.memory(|memory| memory.area_rect(Id::new("tool-group"))).expect("the tool bar is drawn").center();
    for (point, what) in [(sidebar, "the sidebar"), (panel, "the panel"), (bar, "the tool bar")] {
        assert!(page.contains(point), "the page lies under {what}");
        bench.drag_tile_to("사각형", point);
        assert!(bench.preview().is_none(), "no outline over {what}");
        bench.release_at(point);
        assert!(bench.nodes(0).is_empty(), "a drop over {what} is cancelled");
    }
    // Outside the window (a release the window still hears, at a point off its edge).
    let outside = Pos2::new(-40.0, board.center().y);
    bench.drag_tile_to("사각형", outside);
    bench.release_at(outside);
    assert!(bench.nodes(0).is_empty(), "a drop outside the window is cancelled");
    // The pointer gone from the window before the release.
    let on_page = board.center();
    bench.drag_tile_to("사각형", on_page);
    bench.frame(vec![Event::PointerGone]);
    bench.frame(vec![primary_button(outside, false)]);
    bench.frames(2);
    assert!(bench.nodes(0).is_empty(), "a drop after the pointer left the window is cancelled");

    // Off the pages, unzoomed.
    let mut bench = Bench::canvas();
    let corner = bench.board().min + Vec2::new(4.0, 4.0);
    assert!(!bench.page_rect(0).contains(corner), "the corner of the board is off the page");
    bench.drop_tile_at("사각형", corner);
    assert!(bench.nodes(0).is_empty(), "a drop off the pages is cancelled");
}

#[test]
fn escape_cancels_a_drag_and_keeps_the_selection() {
    let mut bench = Bench::canvas();
    let run = bench.window.agent_run(&mut bench.host, "add.rect", json!({ "at": { "x": 100.0, "y": 100.0 } }));
    assert_eq!(run["status"], "completed", "{run}");
    bench.frames(2);
    let id = bench.nodes(0)[0].id.clone();
    bench.window.app.select_node(id.clone());
    bench.frames(2);
    let target = bench.page_rect(0).center();
    bench.drag_tile_to("타원", target);
    assert!(bench.preview().is_some());
    bench.frame(vec![escape()]);
    assert!(bench.preview().is_none(), "Esc takes the outline away");
    bench.frame(vec![Event::PointerMoved(target + Vec2::new(5.0, 5.0))]);
    assert!(bench.preview().is_none(), "the cancelled drag shows no outline again");
    bench.release_at(target);
    assert_eq!(bench.nodes(0).len(), 1, "the release after Esc inserts nothing");
    assert_eq!(bench.window.app.selected_nodes, vec![id], "Esc cancelled the drag, not the selection");
}

#[test]
fn a_zoomed_and_panned_board_takes_the_click_and_the_drop_at_their_points() {
    let mut bench = Bench::canvas();
    let board = bench.board();
    bench.window.app.zoom_about(board, board.center(), 1.6);
    bench.window.app.view().pan += Vec2::new(-90.0, 50.0);
    bench.frames(3);
    bench.click_tile("텍스트");
    let (want, slack) = (bench.visible_centre(0), bench.slack(0));
    let text = bench.nodes(0).remove(0);
    assert!(near(centre(&text.bounds), want, slack), "the click lands on the visible centre {want:?}, got {:?}", centre(&text.bounds));

    let target = bench.page_rect(0).intersect(board).center() + Vec2::new(40.0, 30.0);
    bench.drop_tile_at("타원", target);
    let oval = bench.nodes(0).into_iter().find(|node| node.kind == NodeKind::Ellipse).expect("the drop adds the oval");
    let want = bench.emu_at(0, target);
    assert!(near(centre(&oval.bounds), want, slack), "the drop lands on {want:?}, got {:?}", centre(&oval.bounds));
}

#[test]
fn a_drop_on_another_page_makes_it_current_and_the_table_stays_editable() {
    let mut host = Host::start();
    let canvas = two_page_canvas(&mut host);
    let mut bench = Bench::open(host, canvas);
    bench.tools();
    assert_eq!(bench.window.app.view_ref().page_index, 0);
    let target = bench.page_rect(1).intersect(bench.board()).center();
    bench.drop_tile_at("표", target);
    bench.frames(3);

    assert_eq!(bench.window.app.view_ref().page_index, 1, "the page of the drop is the current page");
    assert!(bench.nodes(0).is_empty(), "nothing on the first page");
    let table = bench.nodes(1).into_iter().find(|node| node.kind == NodeKind::Group).expect("the table is on the second page");
    let (want, slack) = (bench.emu_at(1, target), bench.slack(1));
    assert!(near(centre(&table.bounds), want, slack), "the table is centred on the drop point");
    assert_eq!(bench.window.app.selected_nodes, vec![table.id.clone()], "the new table is selected");
    assert_eq!(bench.window.app.context.as_ref().map(|context| context.visible_page), Some(1), "the host shows the second page");

    let rows = table.ext.table.as_ref().expect("a table").rows.len();
    bench.step("canvas.table.add_row", json!({ "table_id": table.id })).expect("the table on the new current page takes a row");
    let table = bench.nodes(1).into_iter().find(|node| node.id == table.id).expect("the table");
    assert_eq!(table.ext.table.as_ref().expect("a table").rows.len(), rows + 1);

    // An agent's `at` on the first page switches back the same way.
    let run = bench.window.agent_run(&mut bench.host, "add.rect", json!({ "at": { "page_index": 0, "x": 50.0, "y": 50.0 } }));
    assert_eq!(run["status"], "completed", "{run}");
    bench.frames(2);
    assert_eq!(bench.window.app.view_ref().page_index, 0);
    assert_eq!(bench.nodes(0).len(), 1);
}

#[test]
fn the_at_argument_places_an_object_for_an_agent_and_refuses_a_bad_point_or_page() {
    let mut bench = Bench::canvas();
    let run = bench.window.agent_run(&mut bench.host, "add.oval", json!({ "at": { "x": 30.0, "y": 20.0 } }));
    assert_eq!(run["status"], "completed", "{run}");
    let nodes = bench.nodes(0);
    assert_eq!(nodes.len(), 1);
    assert_eq!(nodes[0].kind, NodeKind::Ellipse);
    assert_eq!(centre(&nodes[0].bounds), (30 * EMU_PER_CSS_PX, 20 * EMU_PER_CSS_PX), "the centre is at the given point in px");

    let refused = bench.step("add.rect", json!({ "at": { "x": -5.0, "y": 10.0 } }));
    assert!(refused.is_err_and(|message| message.contains("쪽 밖")), "a point off the page is refused");
    let unknown_page = bench.step("add.rect", json!({ "at": { "page_index": 9, "x": 1.0, "y": 1.0 } }));
    assert!(unknown_page.is_err(), "an unknown page is refused");
    // 2^32 would wrap to page 0 as a u32.
    let huge = bench.step("add.rect", json!({ "at": { "page_index": 4_294_967_296_u64, "x": 1.0, "y": 1.0 } }));
    assert!(huge.is_err_and(|message| message.contains("page_index")), "a page index past u32 is refused, not wrapped");
    // (A null is an absent argument for every command, `without_absent_nulls`.)
    for index in [json!(0.0), json!(1.5), json!(-1), json!("0")] {
        let refused = bench.step("add.rect", json!({ "at": { "page_index": index, "x": 1.0, "y": 1.0 } }));
        assert!(refused.is_err(), "the page index {index} is not a whole page number and is refused");
    }
    let run = bench.window.agent_run(&mut bench.host, "add.rect", json!({ "at": { "page_index": 4_294_967_296_u64, "x": 1.0, "y": 1.0 } }));
    assert_eq!(run["status"], "failed", "the agent's run is refused too: {run}");
    assert_eq!(bench.nodes(0).len(), 1, "a refused insert adds nothing");
}

#[test]
fn a_tile_click_and_an_agent_add_at_the_same_point_make_the_same_node() {
    let mut bench = Bench::canvas();
    bench.click_tile("텍스트");
    let by_tile = bench.nodes(0).remove(0);
    bench.step("edit.undo", json!({})).expect("the insert can be undone");

    let (x, y) = centre(&by_tile.bounds);
    let scale = EMU_PER_CSS_PX as f64;
    let run = bench.window.agent_run(&mut bench.host, "add.text", json!({ "at": { "x": x as f64 / scale, "y": y as f64 / scale } }));
    assert_eq!(run["status"], "completed", "{run}");
    let by_agent = bench.nodes(0).remove(0);
    assert_eq!(by_agent.kind, by_tile.kind);
    assert_eq!(by_agent.bounds, by_tile.bounds, "the same point gives the same bounds");
}

#[test]
fn the_canvas_tiles_and_group_heads_are_drawn_and_the_tab_is_reported_as_tools() {
    let mut bench = Bench::canvas();
    let labels: Vec<String> = bench.sidebar_controls().into_iter().map(|(label, _)| label).collect();
    for name in ["텍스트", "메모", "표", "프레임", "사각형", "타원", "선", "이미지", "동영상", "아이콘"] {
        assert!(labels.iter().any(|label| label == name), "the tile {name} is drawn");
    }
    for head in ["기본", "도형", "미디어"] {
        assert!(!labels.iter().any(|label| label == head), "the group head {head} is no control");
    }
    let state = serde_json::to_value(bench.window.app.ui_state()).expect("the state");
    assert_eq!(state["panels"][0]["tab"], "tools", "ui.state reports the toolbox tab: {state}");
    let layout = bench.window.app.ui_layout();
    let sidebar = layout.panels.iter().find(|panel| panel.id == "sidebar").expect("the sidebar is laid out");
    assert_eq!(sidebar.tab.as_deref(), Some("tools"), "ui.layout reports the toolbox tab");
}

#[test]
fn the_tiles_have_gaps_between_rows_and_the_heads_keep_the_sidebar_margin() {
    let mut bench = Bench::canvas();
    let controls = bench.sidebar_controls();
    let rect = |name: &str| controls.iter().find(|(label, _)| label == name).map(|(_, rect)| *rect).expect("the tile");
    let (text, table) = (rect("텍스트"), rect("표"));
    assert_eq!(table.top() - text.bottom(), 8.0, "the rows of a group are 8 apart");
    let (text, memo) = (rect("텍스트"), rect("메모"));
    assert_eq!(memo.left() - text.right(), 8.0, "the tiles of a row are 8 apart");
    // The docked sidebar starts at the window's left edge.
    assert_eq!(text.left(), 12.0, "the content keeps the sidebar's side margin of 12");
}

#[test]
fn a_slide_deck_shows_the_slide_tiles_and_a_click_inserts_on_the_slide() {
    let mut host = Host::start();
    let slides = host.create("slides", "슬라이드");
    let mut bench = Bench::open(host, slides);
    bench.tools();
    let labels: Vec<String> = bench.sidebar_controls().into_iter().map(|(label, _)| label).collect();
    for name in ["텍스트", "사각형", "타원", "선", "이미지"] {
        assert!(labels.iter().any(|label| label == name), "the slide tile {name} is drawn");
    }
    for name in ["메모", "표", "프레임", "동영상"] {
        assert!(!labels.iter().any(|label| label == name), "a slide has no {name} tile");
    }
    let before = bench.nodes(0).len();
    bench.click_tile("사각형");
    let nodes = bench.nodes(0);
    assert_eq!(nodes.len(), before + 1, "the click adds one rectangle to the slide");
    let added = nodes.iter().find(|node| node.kind == NodeKind::Rectangle).expect("the rectangle");
    let (want, slack) = (bench.visible_centre(0), bench.slack(0));
    assert!(near(centre(&added.bounds), want, slack), "on the slide's visible centre");
}

#[test]
fn a_text_document_has_no_toolbox_tab_and_refuses_it() {
    let mut host = Host::start();
    let document = host.create("docx", "문서");
    let mut bench = Bench::open(host, document);
    let refused = bench.step("nav.tab", json!({ "tab": "tools" }));
    assert!(refused.is_err_and(|message| message.contains("도구 탭")), "a text document refuses the toolbox tab");
    let labels: Vec<String> = bench.sidebar_controls().into_iter().map(|(label, _)| label).collect();
    assert!(!labels.iter().any(|label| label == "도구"), "no toolbox tab is drawn: {labels:?}");
    let state = serde_json::to_value(bench.window.app.ui_state()).expect("the state");
    assert_eq!(state["panels"][0]["tab"], "layers", "ui.state reports the tab on screen: {state}");
}

#[test]
fn a_memo_tile_opens_a_memo_at_its_point_and_says_why_it_cannot() {
    let mut bench = Bench::canvas();
    bench.click_tile("메모");
    let editor = bench.window.app.board_memo_editor.as_ref().expect("the memo editor opens");
    assert!(editor.is_new, "a new memo");
    let got = centre(&editor.memo.bounds);
    let (want, slack) = (bench.visible_centre(0), bench.slack(0));
    assert!(near(got, want, slack), "the memo is centred on {want:?}, got {got:?}");

    // A second memo while the first is open: refused with the reason, the open memo stays.
    bench.click_tile("메모");
    assert!(bench.window.message().is_some_and(|message| message.contains("메모 편집")), "{:?}", bench.window.message());
    bench.window.app.board_memo_editor = None;
    bench.frames(1);

    // The hand tool takes no memo, as the board's own memo input.
    bench.window.app.view().tool = Tool::Hand;
    bench.click_tile("메모");
    assert!(bench.window.app.board_memo_editor.is_none(), "no memo with the hand tool");
    assert!(bench.window.message().is_some_and(|message| message.contains("손 도구")), "{:?}", bench.window.message());
    bench.window.app.view().tool = Tool::Select;

    // A memo dropped on the page is centred on the drop point.
    let target = bench.page_rect(0).center() + Vec2::new(-70.0, 40.0);
    bench.drop_tile_at("메모", target);
    let want = bench.emu_at(0, target);
    let editor = bench.window.app.board_memo_editor.as_ref().expect("the drop opens the memo editor");
    assert!(near(centre(&editor.memo.bounds), want, slack), "the dropped memo is centred on the drop point");
}

#[test]
fn an_icon_tile_waits_for_its_pick_and_the_pick_lands_at_the_point() {
    let mut bench = Bench::canvas();
    bench.click_tile("아이콘");
    let pending = bench.pending_icon().expect("the icon picker waits for a pick");
    bench.frames(2);
    assert!(bench.pending_icon().is_some(), "the picker stays while its tab is drawn");
    assert!(bench.nodes(0).is_empty(), "nothing is inserted before the pick");
    let name = crate::content_icons::search("")[0].name.clone();
    let ctx = bench.window.ctx.clone();
    bench.window.app.finish_icon_pick(&ctx, &name);
    bench.window.settle(&mut bench.host);
    let nodes = bench.nodes(0);
    assert_eq!(nodes.len(), 1, "the pick adds one icon");
    assert_eq!(nodes[0].kind, NodeKind::Path);
    assert_eq!(centre(&nodes[0].bounds), pending.at.center, "the icon is centred on the point it waited for");
    assert!(bench.pending_icon().is_none(), "the pick clears the wait");
}

#[test]
fn an_icon_wait_ends_with_escape_another_tab_or_another_artifact() {
    let mut host = Host::start();
    let other = canvas_with_page(&mut host, "다른 캔버스");
    let canvas = canvas_with_page(&mut host, "도구 상자");
    let mut bench = Bench::open(host, canvas);
    bench.tools();

    bench.click_tile("아이콘");
    assert!(bench.pending_icon().is_some());
    bench.frame(vec![escape()]);
    bench.frame(Vec::new());
    assert!(bench.pending_icon().is_none(), "Esc closes the picker");

    bench.click_tile("아이콘");
    assert!(bench.pending_icon().is_some());
    bench.step("nav.tab", json!({ "tab": "layers" })).expect("the layers tab");
    assert!(bench.pending_icon().is_none(), "leaving the tab drops the wait");
    bench.tools();
    assert!(bench.pending_icon().is_none(), "back on the tab, the picker does not come back by itself");

    bench.click_tile("아이콘");
    assert!(bench.pending_icon().is_some());
    bench.step("nav.toggle_left", json!({})).expect("the sidebar closes");
    assert!(bench.pending_icon().is_none(), "a closed sidebar drops the wait");
    bench.step("nav.toggle_left", json!({})).expect("the sidebar opens");

    bench.click_tile("아이콘");
    assert!(bench.pending_icon().is_some());
    let list = bench.host.call("workspace.list", json!({})).expect("the work list");
    let summary = list["artifacts"].as_array().and_then(|all| all.iter().find(|item| item["id"] == other.as_str())).cloned().expect("the other canvas");
    bench.window.app.activate(serde_json::from_value::<ArtifactSummary>(summary).expect("a summary"));
    bench.window.settle(&mut bench.host);
    bench.frames(2);
    assert!(bench.pending_icon().is_none(), "another artifact drops the wait");
}

#[test]
fn an_image_dropped_on_the_page_is_centred_on_the_drop_point() {
    let mut bench = Bench::canvas();
    let dir = std::env::temp_dir().join(format!("designllm-68g-image-{}", designllm_contracts::new_id()));
    std::fs::create_dir_all(&dir).expect("a folder");
    let png = dir.join("drop.png");
    image::RgbaImage::from_pixel(4, 3, image::Rgba([200, 40, 40, 255])).save(&png).expect("a PNG");
    crate::dialogs::set_test_answers(Some([png].into_iter().collect()));
    let target = bench.page_rect(0).center() + Vec2::new(50.0, 25.0);
    bench.drop_tile_at("이미지", target);
    bench.frames(2);
    assert_eq!(crate::dialogs::set_test_answers(None), 0, "the drop opened the picture dialog once");
    let image = bench.nodes(0).into_iter().find(|node| node.kind == NodeKind::Image).expect("the picture is inserted");
    let (want, slack) = (bench.emu_at(0, target), bench.slack(0));
    assert!(near(centre(&image.bounds), want, slack), "the picture is centred on {want:?}, got {:?}", centre(&image.bounds));
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn a_video_dropped_on_the_page_keeps_the_drop_point() {
    let mut bench = Bench::canvas();
    let dir = std::env::temp_dir().join(format!("designllm-68g-video-{}", designllm_contracts::new_id()));
    std::fs::create_dir_all(&dir).expect("a folder");
    let video = dir.join("drop.mp4");
    std::fs::write(&video, b"not a real video").expect("a file");
    crate::dialogs::set_test_answers(Some([video].into_iter().collect()));
    let target = bench.page_rect(0).center() + Vec2::new(-40.0, -20.0);
    bench.drag_tile_to("동영상", target);
    bench.frame(vec![Event::PointerMoved(target), primary_button(target, false)]);
    assert_eq!(crate::dialogs::set_test_answers(None), 0, "the drop opened the video dialog once");
    let want = bench.emu_at(0, target);
    let pending = bench.window.app.video_insert.as_ref().expect("the video insert starts with the chosen file");
    assert_eq!(pending.intent.page_index, 0);
    let got = pending.intent.center.expect("the drop point is kept");
    assert!(near(got, want, bench.slack(0)), "the video is to be centred on {want:?}, got {got:?}");
    let intent = bench.window.app.video_insert_intent(None);
    assert!(intent.is_err(), "one video insert at a time");
}
