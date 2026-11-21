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
    /// The host's answers wait: a frame leaves what it sent queued, so a page change's `selection.set` stays on its way.
    hold: bool,
}

impl Bench {
    fn open(mut host: Host, artifact: String) -> Self {
        let window = Window::open_with_previews(&mut host, &artifact);
        let mut bench = Self { host, window, artifact, clock: 1.0, shapes: Vec::new(), hold: false };
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
        if !self.hold {
            self.window.settle(&mut self.host);
        }
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

    /// The page EMU of a screen point over the page `index` as it is drawn now, worked out by hand (not by `screen_to_page_emu`,
    /// the function under test): the page's drawn corner is the origin, a screen point is `zoom` px per CSS px, and a CSS px is
    /// `EMU_PER_CSS_PX`. Screen points are px here (scale 1.0), and the view is at the default zoom or the one set in the test.
    fn emu_at(&mut self, index: u32, point: Pos2) -> (i64, i64) {
        let rect = self.page_rect(index);
        let zoom = f64::from(self.window.app.view_ref().zoom);
        let x = f64::from(point.x - rect.left()) / zoom * EMU_PER_CSS_PX as f64;
        let y = f64::from(point.y - rect.top()) / zoom * EMU_PER_CSS_PX as f64;
        (x.round() as i64, y.round() as i64)
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

/// Selects the text node the tile click made on page 0 (the tile itself does not select the insert).
fn select_new_text(bench: &mut Bench) {
    let id = bench.nodes(0).remove(0).id;
    bench.window.app.set_selected_ids(vec![id.clone()]);
    bench.window.app.set_selection(designllm_contracts::Selection { node_ids: vec![id], text_range: None });
    bench.frames(2);
}
fn enter() -> Event {
    Event::Key { key: Key::Enter, physical_key: None, pressed: true, repeat: false, modifiers: Modifiers::NONE }
}

/// 68p 3d: a drag with the right or the middle button inserts nothing; the primary button's drop does.
#[test]
fn a_tile_dragged_with_another_button_than_the_primary_inserts_nothing() {
    use eframe::egui::PointerButton;
    let mut host = Host::start();
    let canvas = two_page_canvas(&mut host);
    let mut bench = Bench::open(host, canvas);
    bench.tools();
    let target = bench.page_rect(1).intersect(bench.board()).center();
    for button in [PointerButton::Secondary, PointerButton::Middle] {
        let from = bench.tile("표").center();
        let press = |pos: Pos2, pressed: bool| Event::PointerButton { pos, button, pressed, modifiers: Modifiers::NONE };
        bench.frame(vec![Event::PointerMoved(from), press(from, true)]);
        for step in 1..=6 {
            let at = from + (target - from) * (step as f32 / 6.0);
            bench.frame(vec![Event::PointerMoved(at)]);
        }
        assert_eq!(bench.preview(), None, "a {button:?} drag shows no outline of an insert (68p 4)");
        bench.frame(vec![Event::PointerMoved(target), press(target, false)]);
        bench.frames(2);
        assert!(bench.nodes(1).is_empty(), "a {button:?} drag inserts nothing");
        assert_eq!(bench.window.app.view_ref().page_index, 0, "and does not change the page");
    }
    bench.drop_tile_at("표", target);
    bench.frames(3);
    assert!(bench.nodes(1).iter().any(|node| node.kind == NodeKind::Group), "the primary button's drop still inserts the table");
}

/// 68p 3a: the page and selection a failed insert puts back are the ones before it (`add_focus_state`, `restore_add_focus`).
#[test]
fn a_failed_insert_puts_back_the_page_and_the_selection_its_page_change_cleared() {
    let mut host = Host::start();
    let canvas = two_page_canvas(&mut host);
    let mut bench = Bench::open(host, canvas);
    bench.tools();
    bench.click_tile("텍스트");
    bench.frames(2);
    select_new_text(&mut bench);
    let selected = bench.window.app.selected_nodes.clone();
    assert!(!selected.is_empty(), "the new text is selected");
    assert!(bench.window.app.add_focus_state(0).is_none(), "an insert on the current page has nothing to put back");
    let before = bench.window.app.add_focus_state(1);
    bench.window.app.focus_add_page(1);
    assert_eq!(bench.window.app.view_ref().page_index, 1, "the page change is made");
    assert!(bench.window.app.selected_nodes.is_empty(), "and the selection is cleared");
    bench.window.app.restore_add_focus(before);
    assert_eq!(bench.window.app.view_ref().page_index, 0, "the failed insert puts the page back");
    assert_eq!(bench.window.app.context.as_ref().map(|context| context.visible_page), Some(0), "the host's visible page too");
    assert_eq!(bench.window.app.selected_nodes, selected, "and the selection");
}

/// 68p 3b: a tile dropped on another page while a text edit is open does not leave the edit on the page it was not made on.
#[test]
fn a_tile_dropped_on_another_page_during_an_inline_text_edit_keeps_the_edit_on_its_page() {
    let mut host = Host::start();
    let canvas = two_page_canvas(&mut host);
    let mut bench = Bench::open(host, canvas);
    bench.tools();
    bench.click_tile("텍스트");
    bench.frames(2);
    select_new_text(&mut bench);
    bench.frame(vec![enter()]);
    bench.frames(2);
    assert!(bench.window.app.inline_text_edit.is_some(), "Enter starts the inline edit of the new text");
    let target = bench.page_rect(1).intersect(bench.board()).center();
    bench.drop_tile_at("표", target);
    bench.frames(3);
    let view_page = bench.window.app.view_ref().page_index;
    let edit = bench.window.app.inline_text_edit.as_ref().expect("the drop does not end the edit");
    assert_eq!(edit.page_index, view_page, "the open edit is on the page that is current");
    assert!(edit.conflict.is_none(), "and raises no conflict: {:?}", edit.conflict);
    assert!(bench.nodes(1).is_empty(), "the drop onto the other page is refused while the edit is open");
    assert!(bench.window.message().is_some_and(|message| message.contains("텍스트 편집")), "the refusal says why: {:?}", bench.window.message());
}

/// A two-page canvas whose page 0 holds a selected text node, the toolbox tab open, and the page and selection it shows.
fn two_pages_with_a_selection() -> (Bench, Vec<String>) {
    let mut host = Host::start();
    let canvas = two_page_canvas(&mut host);
    let mut bench = Bench::open(host, canvas);
    bench.tools();
    bench.click_tile("텍스트");
    bench.frames(2);
    select_new_text(&mut bench);
    let selected = bench.window.app.selected_nodes.clone();
    assert_eq!(selected.len(), 1, "the new text is selected");
    assert_eq!(bench.window.app.view_ref().page_index, 0);
    (bench, selected)
}

/// The page, the host's visible page and the selection are the ones before a tile insert that did not go ahead.
fn assert_page_and_selection_kept(bench: &Bench, selected: &[String], what: &str) {
    assert_eq!(bench.window.app.view_ref().page_index, 0, "{what}: the page stays the current one");
    assert_eq!(bench.window.app.context.as_ref().map(|context| context.visible_page), Some(0), "{what}: the host's visible page too");
    assert_eq!(bench.window.app.selected_nodes, selected, "{what}: the selection stays");
    let local = bench.window.app.local_selection.as_ref().map(|selection| selection.node_ids.clone());
    assert_eq!(local.as_deref(), Some(selected), "{what}: the selection asked of the host is the kept one");
}

/// Drops the video tile on page 1 with `answers` queued for its file dialog, then draws frames until a video insert starts
/// (it is checked before its file inspection's answer, a frame later, can end it) or for ten frames; returns how many
/// answers were left.
fn drop_video_on_page_1(bench: &mut Bench, answers: Vec<std::path::PathBuf>) -> usize {
    crate::dialogs::set_test_answers(Some(answers.into_iter().collect()));
    let target = bench.page_rect(1).intersect(bench.board()).center();
    bench.drag_tile_to("동영상", target);
    bench.frame(vec![Event::PointerMoved(target), primary_button(target, false)]);
    for _ in 0..10 {
        if bench.window.app.video_insert.is_some() {
            break;
        }
        bench.frame(Vec::new());
    }
    crate::dialogs::set_test_answers(None)
}

/// A file named `name` in a new folder of its own (the folder, the file).
fn video_file(name: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!("designllm-68p-video-{}", designllm_contracts::new_id()));
    std::fs::create_dir_all(&dir).expect("a folder");
    let file = dir.join(name);
    std::fs::write(&file, b"not a real video").expect("a file");
    (dir, file)
}

/// 68p 3a: a video tile dropped on another page whose insert is refused (the chosen file is no video) leaves the page and
/// the selection as they were, through the tile's own run.
#[test]
fn a_refused_video_drop_on_another_page_keeps_the_page_and_the_selection() {
    let (mut bench, selected) = two_pages_with_a_selection();
    let (dir, file) = video_file("drop.txt");
    let unanswered = drop_video_on_page_1(&mut bench, vec![file]);
    let message = bench.window.message();
    assert_eq!(unanswered, 0, "the dialog opened; notice: {message:?}");
    assert!(bench.window.app.video_insert.is_none(), "no video insert starts");
    assert!(message.as_deref().is_some_and(|message| message.contains("MP4")), "the refusal says why: {message:?}");
    assert_page_and_selection_kept(&bench, &selected, "a refused video drop");
    let _ = std::fs::remove_dir_all(dir);
}

/// 68p (68g): a video tile dropped on another page opens the file dialog once that page is the current one, and the video
/// insert starts there, at the drop point.
#[test]
fn a_video_dropped_on_another_page_opens_the_dialog_and_starts_there() {
    let (mut bench, _) = two_pages_with_a_selection();
    let (dir, file) = video_file("drop.mp4");
    let unanswered = drop_video_on_page_1(&mut bench, vec![file]);
    assert_eq!(unanswered, 0, "the drop opened the video dialog; notice: {:?}", bench.window.message());
    let pending = bench.window.app.video_insert.as_ref().expect("the video insert starts with the chosen file");
    assert_eq!(pending.intent.page_index, 1, "on the page of the drop");
    assert!(pending.intent.center.is_some(), "at the drop point");
    assert_eq!(bench.window.app.view_ref().page_index, 1, "the page of the drop is the current page");
    let _ = std::fs::remove_dir_all(dir);
}

/// 68p 3a: a video tile dropped on another page whose file dialog is cancelled leaves the page and the selection as they
/// were, with no notice.
#[test]
fn a_cancelled_video_drop_on_another_page_keeps_the_page_and_the_selection() {
    let (mut bench, selected) = two_pages_with_a_selection();
    drop_video_on_page_1(&mut bench, Vec::new());
    assert!(bench.window.app.video_insert.is_none(), "no video insert starts");
    assert_eq!(bench.window.message(), None, "a cancel says nothing");
    assert_page_and_selection_kept(&bench, &selected, "a cancelled video drop");
}

/// 68p 1: with the hand tool, Esc closes the toolbox's icon picker in one press (the picker's Esc, not the tool's).
#[test]
fn esc_closes_the_icon_picker_in_one_press_on_the_hand_tool() {
    let mut bench = Bench::canvas();
    bench.window.app.view().tool = Tool::Hand;
    bench.click_tile("아이콘");
    assert!(bench.pending_icon().is_some(), "the picker waits for a pick");
    bench.frame(vec![escape()]);
    bench.frame(Vec::new());
    assert!(bench.pending_icon().is_none(), "one Esc closes the picker");
}

/// 68p 1: with the hand tool, Esc and the release of a tile's drag in one frame cancel the drop.
#[test]
fn esc_and_the_release_in_one_frame_cancel_a_tile_drag_on_the_hand_tool() {
    let mut bench = Bench::canvas();
    bench.window.app.view().tool = Tool::Hand;
    let target = bench.page_rect(0).intersect(bench.board()).center();
    bench.drag_tile_to("표", target);
    bench.frame(vec![escape(), Event::PointerMoved(target), primary_button(target, false)]);
    bench.frames(2);
    assert!(!bench.nodes(0).iter().any(|node| node.kind == NodeKind::Group), "the Esc cancels the drop");
}

/// 68p 3b: during an inline text edit a tile dropped on the edited page is inserted, as a tile click is.
#[test]
fn a_tile_dropped_on_the_edited_page_during_an_inline_text_edit_is_inserted() {
    let mut host = Host::start();
    let canvas = two_page_canvas(&mut host);
    let mut bench = Bench::open(host, canvas);
    bench.tools();
    bench.click_tile("텍스트");
    bench.frames(2);
    select_new_text(&mut bench);
    bench.frame(vec![enter()]);
    bench.frames(2);
    assert!(bench.window.app.inline_text_edit.is_some(), "Enter starts the inline edit of the new text");
    let target = bench.page_rect(0).intersect(bench.board()).left_top() + Vec2::new(60.0, 60.0);
    bench.drop_tile_at("표", target);
    bench.frames(3);
    assert!(bench.nodes(0).iter().any(|node| node.kind == NodeKind::Group), "the table is inserted on the edited page; notice: {:?}", bench.window.message());
    assert_eq!(bench.window.app.view_ref().page_index, 0, "the page stays the current one");
}

/// 68p 5: a page put back after an insert that did not go ahead goes through `focus_page`, so a page reorder's pending
/// selection names the page shown again at once.
#[test]
fn a_put_back_page_is_the_one_a_pending_page_reorder_remembers() {
    let (mut bench, _) = two_pages_with_a_selection();
    let revision = bench.window.app.artifact.as_ref().expect("the artifact").revision;
    let pages = vec!["page-1".to_owned(), "page-2".to_owned()];
    bench.window.app.pending_page_reorder_selection = Some(super::state::PendingPageReorderSelection {
        token: "reorder-1".into(),
        artifact_id: bench.artifact.clone(),
        base_revision: revision,
        base_page_ids: pages.clone(),
        expected_page_ids: pages.into_iter().rev().collect(),
        selected_page_id: Some("page-1".into()),
        last_page_index: 0,
        committed_revision: None,
    });
    crate::dialogs::set_test_answers(Some(Default::default()));
    let target = bench.page_rect(1).intersect(bench.board()).center();
    bench.drag_tile_to("이미지", target);
    bench.frame(vec![Event::PointerMoved(target), primary_button(target, false)]);
    crate::dialogs::set_test_answers(None);
    assert_eq!(bench.window.app.view_ref().page_index, 0, "the cancelled picture drop puts the page back");
    let pending = bench.window.app.pending_page_reorder_selection.as_ref().expect("the reorder still waits");
    assert_eq!((pending.selected_page_id.as_deref(), pending.last_page_index), (Some("page-1"), 0), "it remembers the page shown again");
}

/// 68p 5: with no selection of the window's own (`local_selection` is None until the context is read), the selection asked
/// of the host when a page is put back is the one the host showed, not an empty one.
#[test]
fn a_put_back_selection_without_a_local_one_is_the_one_the_host_showed() {
    let (mut bench, selected) = two_pages_with_a_selection();
    crate::dialogs::set_test_answers(Some(Default::default()));
    let target = bench.page_rect(1).intersect(bench.board()).center();
    bench.drag_tile_to("이미지", target);
    bench.window.app.local_selection = None;
    bench.frame(vec![Event::PointerMoved(target), primary_button(target, false)]);
    crate::dialogs::set_test_answers(None);
    bench.frames(2);
    assert_page_and_selection_kept(&bench, &selected, "a cancelled picture drop with no local selection");
}

/// 68p 3a: a picture tile dropped on another page whose file dialog is cancelled inserts nothing and leaves the page and
/// the selection as they were.
#[test]
fn a_cancelled_picture_drop_on_another_page_keeps_the_page_and_the_selection() {
    let (mut bench, selected) = two_pages_with_a_selection();
    crate::dialogs::set_test_answers(Some(Default::default()));
    let target = bench.page_rect(1).intersect(bench.board()).center();
    bench.drop_tile_at("이미지", target);
    crate::dialogs::set_test_answers(None);
    bench.frames(2);
    assert!(bench.nodes(1).is_empty(), "nothing is inserted");
    assert_page_and_selection_kept(&bench, &selected, "a cancelled picture drop");
}

/// 68p 3a: a memo tile dropped on another page whose drawing is not in yet opens no memo editor (the board says why) and
/// leaves the page and the selection as they were.
#[test]
fn a_memo_drop_on_another_page_that_opens_no_memo_keeps_the_page_and_the_selection() {
    let (mut bench, selected) = two_pages_with_a_selection();
    let target = bench.page_rect(1).intersect(bench.board()).center();
    bench.drag_tile_to("메모", target);
    // The page's drawing is asked for again: until it is in, a memo cannot be placed on it.
    bench.window.app.page_textures.remove(&1);
    bench.frame(vec![Event::PointerMoved(target), primary_button(target, false)]);
    let message = bench.window.message();
    bench.frames(2);
    assert!(bench.window.app.board_memo_editor.is_none(), "no memo editor opens");
    assert!(message.is_some_and(|message| message.contains("메모")), "the board says why");
    assert_page_and_selection_kept(&bench, &selected, "a memo drop that opens no memo");
}

/// Drops the video tile on page 1 while the host holds its answers (`Bench::hold`): the page change's `selection.set` stays
/// on its way, so the video waits.
fn drop_video_on_page_1_held(bench: &mut Bench) {
    bench.hold = true;
    let target = bench.page_rect(1).intersect(bench.board()).center();
    bench.drag_tile_to("동영상", target);
    bench.frame(vec![Event::PointerMoved(target), primary_button(target, false)]);
}

/// Lets the host answer what the held frames sent, and draws a few frames.
fn release_hold(bench: &mut Bench) {
    bench.hold = false;
    bench.window.settle(&mut bench.host);
    bench.frames(3);
}

/// 68p 2차 MINOR 1: while a video insert runs on page 1, a video tile dropped on page 0 is refused before the page changes.
#[test]
fn a_video_tile_dropped_on_another_page_while_a_video_insert_runs_is_refused_before_the_page_changes() {
    let (mut bench, _) = two_pages_with_a_selection();
    bench.window.app.focus_add_page(1);
    bench.frames(2);
    hold_video_insert_on_page_1(&mut bench);
    let target = bench.page_rect(0).intersect(bench.board()).center();
    let sent_before = bench.window.calls.len();
    bench.drop_tile_at("동영상", target);
    let sent: Vec<String> = bench.window.calls[sent_before..].iter().map(|(method, _, _)| method.clone()).collect();
    assert!(!sent.iter().any(|method| method == "selection.set"), "the refused drop asks the host for no page change: {sent:?}");
    assert_eq!(bench.window.app.view_ref().page_index, 1, "the page of the running insert stays the current one");
    assert!(bench.window.app.video_insert.is_some(), "the running insert is kept");
    assert!(bench.window.message().is_some_and(|message| message.contains("진행 중")), "the refusal says why: {:?}", bench.window.message());
}

/// A video insert on page 1 that stays in its source inspection: the state the insert's own flow leaves while its worker runs.
/// The inspection runs on a thread the test does not wait for, so the state is put in place directly.
fn hold_video_insert_on_page_1(bench: &mut Bench) {
    let intent = bench.window.app.video_insert_intent(None).expect("page 1 is the current page with nothing pending");
    bench.window.app.video_insert = Some(super::state::PendingVideoInsert {
        intent,
        source_path: std::path::PathBuf::from("drop.mp4"),
        stage: super::state::VideoInsertStage::InspectingSource,
        source_info: None,
        source_asset: None,
        poster: None,
        poster_asset: None,
        poster_temp_path: None,
        worker_cancel: None,
        cancel_requested: false,
        node_id: designllm_contracts::new_id(),
        apply: None,
        resolution_retry_sent: false,
        message: None,
    });
}

/// 68p 2차 MINOR 1: while a video waits for its page change, the video tile clicked on the page it waits for is refused, the
/// file dialog does not open, and the wait stays.
#[test]
fn a_video_tile_clicked_while_another_video_waits_for_its_page_change_is_refused() {
    let (mut bench, _) = two_pages_with_a_selection();
    drop_video_on_page_1_held(&mut bench);
    crate::dialogs::set_test_answers(Some(std::iter::once(std::path::PathBuf::from("drop.mp4")).collect()));
    bench.click_tile("동영상");
    assert_eq!(crate::dialogs::set_test_answers(None), 1, "the file dialog did not open");
    assert!(bench.window.app.video_insert.is_none(), "no video insert starts");
    assert_eq!(bench.window.app.view_ref().page_index, 1, "the page of the wait stays");
    assert!(bench.window.message().is_some_and(|message| message.contains("기다리세요")), "the refusal says why: {:?}", bench.window.message());
    release_hold(&mut bench);
}

/// 68p 2차 MINOR 2: a text node made on page 1, with the view back on page 0. The node is one a wait's selection can name.
fn text_node_on_page_1(bench: &mut Bench) -> String {
    bench.window.app.focus_add_page(1);
    bench.frames(2);
    bench.click_tile("텍스트");
    bench.frames(2);
    let id = bench.nodes(1).remove(0).id;
    bench.window.app.focus_add_page(0);
    bench.frames(2);
    id
}

/// The user selects `id` (a node on the page the wait is for) while the host holds its answers.
fn select_node_held(bench: &mut Bench, id: String) {
    bench.window.app.set_selected_ids(vec![id.clone()]);
    bench.window.app.set_selection(designllm_contracts::Selection { node_ids: vec![id], text_range: None });
}

/// The user opens a memo editor on page 1 (the page the wait is for) at the centre of the page.
fn open_memo_on_page_1(bench: &mut Bench) {
    let size = bench.page_size(1);
    let ctx = bench.window.ctx.clone();
    bench.window.app.begin_board_memo_centred(&ctx, 1, (size.width / 2, size.height / 2)).expect("the memo editor opens");
}

/// 68p 2차 MINOR 1 (regression): a memo editor the user opens during a video wait keeps its page when the wait ends. The page
/// and selection are not put back under it, and no file dialog opens.
#[test]
fn a_memo_editor_opened_during_a_video_wait_keeps_the_page_when_the_wait_ends() {
    let (mut bench, _) = two_pages_with_a_selection();
    crate::dialogs::set_test_answers(Some(std::iter::once(std::path::PathBuf::from("drop.mp4")).collect()));
    drop_video_on_page_1_held(&mut bench);
    open_memo_on_page_1(&mut bench);
    release_hold(&mut bench);
    assert_eq!(crate::dialogs::set_test_answers(None), 1, "the file dialog did not open");
    assert!(bench.window.app.video_insert.is_none(), "no video insert starts");
    assert_eq!(bench.window.app.view_ref().page_index, 1, "the page of the memo stays the current one");
    assert!(bench.window.app.board_memo_editor.is_some(), "the memo editor stays open");
    assert!(bench.window.message().is_some_and(|message| message.contains("다른 입력")), "the notice says why: {:?}", bench.window.message());
}

/// 68p 2차 MINOR 1 (regression, expiry path): the same memo editor when the five seconds run out.
#[test]
fn a_memo_editor_opened_during_a_video_wait_keeps_the_page_when_the_wait_expires() {
    let (mut bench, _) = two_pages_with_a_selection();
    crate::dialogs::set_test_answers(Some(Default::default()));
    drop_video_on_page_1_held(&mut bench);
    open_memo_on_page_1(&mut bench);
    bench.clock += 6.0;
    bench.frame(Vec::new());
    release_hold(&mut bench);
    assert_eq!(crate::dialogs::set_test_answers(None), 0, "no file dialog was asked for");
    assert_eq!(bench.window.app.view_ref().page_index, 1, "the page of the memo stays the current one");
    assert!(bench.window.app.board_memo_editor.is_some(), "the memo editor stays open");
    assert!(bench.window.message().is_some_and(|message| message.contains("페이지 전환이 끝나지 않아")), "the wait says why: {:?}", bench.window.message());
}

/// 68p 2차 MINOR 2: a node the user selects on page 1 during a video wait stays selected when the wait ends with the dialog
/// cancelled.
#[test]
fn a_node_the_user_selects_while_a_video_waits_is_not_put_back() {
    let (mut bench, _) = two_pages_with_a_selection();
    let id = text_node_on_page_1(&mut bench);
    crate::dialogs::set_test_answers(Some(Default::default()));
    drop_video_on_page_1_held(&mut bench);
    select_node_held(&mut bench, id.clone());
    release_hold(&mut bench);
    assert_eq!(crate::dialogs::set_test_answers(None), 0, "the file dialog was cancelled");
    assert_eq!(bench.window.app.view_ref().page_index, 1, "the page the user is on stays");
    let local = bench.window.app.local_selection.as_ref().map(|selection| selection.node_ids.clone());
    assert_eq!(local, Some(vec![id]), "the node the user selected stays selected");
}

/// 68p 2차 MINOR 3, 90 B1 3차 MINOR 4: a commit during a video wait (a rectangle added on page 1 through the command run, which
/// also answers the held page change) is not put back. The file dialog opens and takes its one answer, a file that is no video;
/// the refusal leaves the page and the commit's selection as they are.
#[test]
fn a_document_change_during_a_video_wait_is_not_put_back() {
    let (mut bench, selected) = two_pages_with_a_selection();
    let (dir, file) = video_file("drop.txt");
    crate::dialogs::set_test_answers(Some(std::iter::once(file).collect()));
    drop_video_on_page_1_held(&mut bench);
    let revision = bench.window.app.artifact.as_ref().expect("the artifact").revision;
    bench.step("add.rect", json!({})).expect("the rectangle is added");
    release_hold(&mut bench);
    assert!(bench.window.app.artifact.as_ref().expect("the artifact").revision > revision, "the commit moved the revision on");
    assert_eq!(bench.nodes(1).len(), 1, "the rectangle is on page 1");
    assert_eq!(crate::dialogs::set_test_answers(None), 0, "the file dialog opened and took its answer");
    assert!(bench.window.app.video_insert.is_none(), "no video insert starts");
    assert!(bench.window.message().is_some_and(|message| message.contains("MP4")), "the refusal says why: {:?}", bench.window.message());
    assert_eq!(bench.window.app.view_ref().page_index, 1, "the page of the change stays the current one");
    assert_ne!(bench.window.app.selected_nodes, selected, "the selection of page 0 is not put back");
    let _ = std::fs::remove_dir_all(dir);
}

/// 68p 2차 MINOR 3: an inline text edit the user starts on page 1 during a video wait opens no file dialog, and the edit stays.
#[test]
fn an_inline_text_edit_started_during_a_video_wait_keeps_the_page() {
    let (mut bench, _) = two_pages_with_a_selection();
    let id = text_node_on_page_1(&mut bench);
    crate::dialogs::set_test_answers(Some(std::iter::once(std::path::PathBuf::from("drop.mp4")).collect()));
    drop_video_on_page_1_held(&mut bench);
    select_node_held(&mut bench, id);
    bench.frame(vec![enter()]);
    assert!(bench.window.app.inline_text_edit.is_some(), "Enter starts the inline edit during the wait");
    release_hold(&mut bench);
    assert_eq!(crate::dialogs::set_test_answers(None), 1, "the file dialog did not open");
    assert!(bench.window.app.video_insert.is_none(), "no video insert starts");
    assert_eq!(bench.window.app.view_ref().page_index, 1, "the page of the edit stays the current one");
    assert!(bench.window.app.inline_text_edit.is_some(), "the inline edit stays open");
}

/// 68p 2차 MINOR 3, 90 B1 3차 MINOR 3: a key dialog that stays open when the wait ends (the host-lost prompt holds until the
/// host is back) opens no file dialog. It is not bound to the page, so the page and the selection go back.
#[test]
fn a_video_wait_that_ends_while_a_key_dialog_is_open_opens_no_file_dialog() {
    let (mut bench, selected) = two_pages_with_a_selection();
    crate::dialogs::set_test_answers(Some(std::iter::once(std::path::PathBuf::from("drop.mp4")).collect()));
    drop_video_on_page_1_held(&mut bench);
    bench.window.app.host_link.lost = true;
    release_hold(&mut bench);
    assert_eq!(crate::dialogs::set_test_answers(None), 1, "the file dialog did not open");
    assert!(bench.window.app.video_insert.is_none(), "no video insert starts");
    assert!(bench.window.app.host_link.lost, "the key dialog is still open");
    assert!(bench.window.message().is_some_and(|message| message.contains("다른 입력")), "the notice says why: {:?}", bench.window.message());
    assert_page_and_selection_kept(&bench, &selected, "a video wait ended under a key dialog");
}

/// 68p 2차 MINOR 3: the icon picker the user opens during a video wait opens no file dialog, and the page stays.
#[test]
fn an_icon_picker_opened_during_a_video_wait_keeps_the_page() {
    let (mut bench, _) = two_pages_with_a_selection();
    crate::dialogs::set_test_answers(Some(std::iter::once(std::path::PathBuf::from("drop.mp4")).collect()));
    drop_video_on_page_1_held(&mut bench);
    bench.click_tile("아이콘");
    assert!(bench.pending_icon().is_some(), "the icon picker waits for a pick");
    release_hold(&mut bench);
    assert_eq!(crate::dialogs::set_test_answers(None), 1, "the file dialog did not open");
    assert_eq!(bench.window.app.view_ref().page_index, 1, "the page of the wait stays");
    assert!(bench.window.message().is_some_and(|message| message.contains("다른 입력")), "the notice says why: {:?}", bench.window.message());
}

/// 68p 2차 MINOR 4: a video wait that ends on the home screen says that the edit was left, and opens no file dialog.
#[test]
fn a_video_wait_that_ends_on_the_home_screen_says_the_edit_was_left() {
    let (mut bench, _) = two_pages_with_a_selection();
    crate::dialogs::set_test_answers(Some(std::iter::once(std::path::PathBuf::from("drop.mp4")).collect()));
    drop_video_on_page_1_held(&mut bench);
    bench.window.app.screen = super::Screen::Home;
    release_hold(&mut bench);
    assert_eq!(crate::dialogs::set_test_answers(None), 1, "the file dialog did not open");
    assert!(bench.window.app.video_insert.is_none(), "no video insert starts");
    assert!(bench.window.message().is_some_and(|message| message.contains("편집 화면을 벗어나")), "the notice says why: {:?}", bench.window.message());
}

/// 68p 2차 MINOR 3(a): a video wait that outlasts five seconds puts the page and selection back and says why. The clock is
/// moved on by the test driver (`Bench::clock`), which is what the wait reads.
#[test]
fn a_video_wait_that_outlasts_five_seconds_puts_the_page_back_with_a_notice() {
    let (mut bench, selected) = two_pages_with_a_selection();
    crate::dialogs::set_test_answers(Some(Default::default()));
    drop_video_on_page_1_held(&mut bench);
    assert_eq!(bench.window.app.view_ref().page_index, 1, "the page change is made");
    bench.clock += 6.0;
    bench.frame(Vec::new());
    release_hold(&mut bench);
    assert_eq!(crate::dialogs::set_test_answers(None), 0, "no file dialog was asked for");
    assert!(bench.window.app.video_insert.is_none(), "no video insert starts");
    assert!(bench.window.message().is_some_and(|message| message.contains("페이지 전환이 끝나지 않아")), "the wait says why: {:?}", bench.window.message());
    assert_page_and_selection_kept(&bench, &selected, "a video wait past five seconds");
}

/// 90 B1 3차 MAJOR 1: the draft recovery notice takes no keys and covers no work, so a video dropped on another page while it
/// is up opens the file dialog, and the insert starts there.
#[test]
fn a_video_dropped_on_another_page_under_the_draft_recovery_notice_opens_the_dialog() {
    let (mut bench, _) = two_pages_with_a_selection();
    let live = super::drafts::Live {
        artifact_id: Some(bench.artifact.clone()),
        revision: Some(1),
        body: super::drafts::Body::Ai { prompt: "보내지 않은 요청".into(), figure_refs: Vec::new() },
    };
    bench.window.app.drafts.keep_recovered(live, std::time::Instant::now());
    bench.frames(2);
    assert!(!bench.window.app.drafts.recovered().is_empty(), "the recovery notice is up");
    let (dir, file) = video_file("drop.mp4");
    let unanswered = drop_video_on_page_1(&mut bench, vec![file]);
    assert_eq!(unanswered, 0, "the drop opened the video dialog; notice: {:?}", bench.window.message());
    let pending = bench.window.app.video_insert.as_ref().expect("the video insert starts with the chosen file");
    assert_eq!(pending.intent.page_index, 1, "on the page of the drop");
    assert!(!bench.window.app.drafts.recovered().is_empty(), "the recovery notice stays");
    let _ = std::fs::remove_dir_all(dir);
}

/// 90 B1 3차 MINOR 2: a drag does not close an open menu (egui closes popups on a click), so a video drop on another page closes
/// it: the zoom menu left open does not hold the file dialog back.
#[test]
fn a_video_dropped_on_another_page_with_a_menu_open_closes_the_menu_and_opens_the_dialog() {
    let (mut bench, _) = two_pages_with_a_selection();
    eframe::egui::Popup::open_id(&bench.window.ctx, Id::new("zoom-menu"));
    bench.frames(2);
    assert!(eframe::egui::Popup::is_any_open(&bench.window.ctx), "the zoom menu is open");
    let (dir, file) = video_file("drop.mp4");
    let unanswered = drop_video_on_page_1(&mut bench, vec![file]);
    assert_eq!(unanswered, 0, "the drop opened the video dialog; notice: {:?}", bench.window.message());
    let pending = bench.window.app.video_insert.as_ref().expect("the video insert starts with the chosen file");
    assert_eq!(pending.intent.page_index, 1, "on the page of the drop");
    assert!(!eframe::egui::Popup::is_any_open(&bench.window.ctx), "the drop closed the menu");
    let _ = std::fs::remove_dir_all(dir);
}
