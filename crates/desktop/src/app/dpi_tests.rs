//! Coordinates at display scales 1.0, 1.5 and 2.0 (SPEC.md:50, VALIDATION A04 and A10): the screen point a click sends
//! against the core's own answer, the caret a click places (logically, and as egui draws it), the move a drag commits for a
//! node and for a board memo, an IME composition across a scale change, and a hit reply without its `hit` field. The windows
//! run on a real host (`test_support::Window`): the core answers each request, and the window draws its own frames.

use designllm_contracts::{
    ArtifactBody, CanvasBoardMemo, CanvasBoardMemoKind, Intent, Node, Operation, Rect as EmuRect, RenderQuery, RenderQueryKind,
    StylePatch, TextPosition, EMU_PER_PT,
};
use eframe::egui::{self, epaint::Shape, Event, ImeEvent, Key, Modifiers, Pos2, Vec2};
use serde_json::{json, Value};

use super::{
    board::PageTexture,
    test_support::{preview_pixels, primary_button, screen, settle_work, Host, Window},
};
use crate::{
    board,
    rpc::{RpcPayload, RpcReply, Work},
};

/// The display scales under test.
const SCALES: [f32; 3] = [1.0, 1.5, 2.0];

/// The bounds of the text node `text-1` in the click, caret and IME tests.
const TEXT_BOUNDS: EmuRect = EmuRect { x: 1_000_000, y: 2_000_000, width: 6_000_000, height: 900_000 };

/// The page-0 texture the window last drew with.
fn page(window: &Window) -> &PageTexture {
    window.app.page_textures.get(&0).expect("page 0 is drawn")
}

/// Screen point (points) of a point in page EMU, from where the window last drew page 0.
fn screen_of(window: &Window, emu: (f64, f64)) -> Pos2 {
    let rect = window.app.page_rect.expect("page 0 is drawn");
    let size = page(window).page.size;
    Pos2::new(
        rect.left() + (emu.0 / size.width as f64 * f64::from(rect.width())) as f32,
        rect.top() + (emu.1 / size.height as f64 * f64::from(rect.height())) as f32,
    )
}

/// Page EMU per physical pixel on the x and y axes, at the page's drawn size and `pixels_per_point`.
fn emu_per_physical_px(window: &Window, pixels_per_point: f32) -> (f64, f64) {
    let rect = window.app.page_rect.expect("page 0 is drawn");
    let size = page(window).page.size;
    (
        size.width as f64 / (f64::from(rect.width()) * f64::from(pixels_per_point)),
        size.height as f64 / (f64::from(rect.height()) * f64::from(pixels_per_point)),
    )
}

/// The last JSON call of `method` the window sent, with the answer the host gave.
fn last_call(window: &Window, method: &str) -> (Value, Value) {
    let (_, params, answer) = window.calls.iter().rev().find(|(sent, _, _)| sent == method).expect("the window sent the request");
    (params.clone(), answer.clone())
}

/// A canvas with one text node `text-1` holding `text`, on the page the host creates.
fn canvas_with_text(host: &mut Host, text: &str, bounds: EmuRect) -> String {
    let canvas = host.create("canvas", "좌표 시험");
    let page_id = match &host.artifact(&canvas).body {
        ArtifactBody::Canvas(document) => document.pages[0].id.clone(),
        _ => panic!("a canvas"),
    };
    let node: Node = serde_json::from_value(json!({
        "id": "text-1", "kind": "text", "text": text,
        "bounds": { "x": bounds.x, "y": bounds.y, "width": bounds.width, "height": bounds.height },
    }))
    .expect("a text node parses");
    let intents = [Intent::Content, Intent::Design].into_iter().collect();
    host.apply(&canvas, vec![Operation::AddNode { page_id, parent_id: None, node }], intents).expect("the node is added");
    canvas
}

/// A canvas with one board memo `memo-1` on its page, holding the text "메모".
fn canvas_with_board_memo(host: &mut Host, bounds: EmuRect) -> String {
    let canvas = host.create("canvas", "메모 시험");
    let page_id = match &host.artifact(&canvas).body {
        ArtifactBody::Canvas(document) => document.pages[0].id.clone(),
        _ => panic!("a canvas"),
    };
    let memo = CanvasBoardMemo {
        kind: CanvasBoardMemoKind::Memo,
        id: "memo-1".into(),
        page_id: page_id.clone(),
        bounds,
        text: "메모".into(),
        color: "#FFF2A8".into(),
    };
    let intents = [Intent::Content].into_iter().collect();
    host.apply(&canvas, vec![Operation::SetCanvasBoardMemos { page_id, expected: Vec::new(), memos: vec![memo] }], intents)
        .expect("the memo is added");
    canvas
}

/// A canvas with one table of two cells, made by the desktop's table command on the first page. The cell texts are set
/// afterwards through the host, and a window opened on the table then edits them.
fn canvas_with_table_texts(host: &mut Host, texts: [&str; 2]) -> (String, [String; 2]) {
    let canvas = host.create("canvas", "표 시험");
    let mut window = Window::open_with_previews(host, &canvas);
    window.draw(host, 1.0);
    window.app.invoke("canvas.table.add", json!({ "rows": 1, "columns": 2 })).expect("the table is added");
    window.settle(host);
    let artifact = host.artifact(&canvas);
    let ArtifactBody::Canvas(document) = &artifact.body else { panic!("a canvas") };
    let table = document.pages[0].nodes.iter().find_map(|node| node.ext.table.as_ref()).expect("the table node");
    let cells = [table.rows[0].cell_ids[0].clone(), table.rows[0].cell_ids[1].clone()];
    let operations = cells.iter().zip(texts).map(|(id, text)| Operation::SetText { node_id: id.clone(), text: text.into() }).collect();
    host.apply(&canvas, operations, [Intent::Content].into_iter().collect()).expect("the cell texts are set");
    (canvas, cells)
}

/// Asks the core a render query as the window does: the window's page snapshot and its view transform.
fn core_query(host: &mut Host, window: &Window, canvas: &str, query: RenderQueryKind) -> Value {
    let method = match &query {
        RenderQueryKind::HitTest { .. } => "render.hit_test",
        RenderQueryKind::Caret { .. } => "render.caret_rect",
        RenderQueryKind::Selection { .. } => "render.selection_rects",
    };
    let transform = window.app.page_transforms.get(&0).expect("page 0 has a view transform");
    let params = serde_json::to_value(RenderQuery {
        artifact_id: canvas.into(),
        snapshot_id: page(window).snapshot_id.clone(),
        view_id: Some(window.app.view_id.clone()),
        transform_id: transform.id.clone(),
        page_index: 0,
        query,
    })
    .expect("a query");
    host.call(method, params).unwrap_or_else(|error| panic!("{method} answers: {error:?}"))
}

/// The core's caret rectangle (page EMU) in front of grapheme `index` of the text of `paragraph` (a node id for node text).
fn core_caret_in(host: &mut Host, window: &Window, canvas: &str, paragraph: &str, index: u32) -> EmuRect {
    let answer = core_query(host, window, canvas, RenderQueryKind::Caret {
        position: TextPosition { paragraph_id: paragraph.into(), grapheme_index: index, trailing: false },
    });
    serde_json::from_value(answer["rect_emu"].clone()).expect("the caret rectangle")
}

/// The core's caret rectangle (page EMU) in front of grapheme `index` of `text-1`.
fn core_caret(host: &mut Host, window: &Window, canvas: &str, index: u32) -> EmuRect {
    core_caret_in(host, window, canvas, "text-1", index)
}

/// A caret on the screen, in physical pixels.
#[derive(Clone, Copy, Debug)]
struct ScreenCaret {
    x: f64,
    top: f64,
    height: f64,
}

/// The core's caret for grapheme `index` of `paragraph` on the screen (`render.caret_rect`'s `rect_screen`), in physical
/// pixels, at the window's transform.
fn core_caret_screen(host: &mut Host, window: &Window, canvas: &str, paragraph: &str, index: u32) -> ScreenCaret {
    let answer = core_query(host, window, canvas, RenderQueryKind::Caret {
        position: TextPosition { paragraph_id: paragraph.into(), grapheme_index: index, trailing: false },
    });
    let rect = &answer["rect_screen"];
    let micropx = |key: &str| rect[key].as_i64().expect("a screen coordinate") as f64 / 1e6;
    ScreenCaret { x: micropx("x_micropx"), top: micropx("y_micropx"), height: micropx("height_micropx") }
}

/// The caret egui drew in the window's last frame, in physical pixels: the one vertical line within the expected caret's
/// height and within 8 px of its x. Any other count fails and names the x positions found; the caller then checks the x.
fn drawn_caret_px(window: &Window, pixels_per_point: f32, expected: ScreenCaret) -> f64 {
    let scale = f64::from(pixels_per_point);
    let carets: Vec<f64> = window
        .shapes
        .iter()
        .filter_map(|clipped| match &clipped.shape {
            Shape::LineSegment { points: [top, bottom], .. } if (top.x - bottom.x).abs() < 1e-3 => {
                let x = f64::from(top.x) * scale;
                let (y0, y1) = (f64::from(top.y.min(bottom.y)) * scale, f64::from(top.y.max(bottom.y)) * scale);
                let within_height = y0 < expected.top + expected.height && y1 > expected.top;
                (within_height && (x - expected.x).abs() <= 8.0).then_some(x)
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        carets.len(),
        1,
        "one caret is drawn beside the core's caret at x {} px (top {}, height {}); the lines found are at x {carets:?}",
        expected.x,
        expected.top,
        expected.height,
    );
    carets[0]
}

/// A point (page EMU) a quarter of the way into grapheme `index`, measured from its caret rectangles: the core places it on
/// `index`, and the caret of `index` is its left edge.
fn quarter_into(host: &mut Host, window: &Window, canvas: &str, index: u32) -> (f64, f64) {
    let left = core_caret(host, window, canvas, index);
    let right = core_caret(host, window, canvas, index + 1);
    (left.x as f64 + (right.x - left.x) as f64 / 4.0, left.y as f64 + left.height as f64 / 2.0)
}

/// The bounds of `text-1` and its text in the host's copy of the canvas.
fn stored_node(host: &mut Host, canvas: &str) -> (EmuRect, String) {
    let artifact = host.artifact(canvas);
    let ArtifactBody::Canvas(document) = &artifact.body else { panic!("a canvas") };
    let node = document.pages[0].nodes.iter().find(|node| node.id == "text-1").expect("the text node");
    (node.bounds, node.text.clone())
}

/// The bounds of the stored board memo `memo-1` (page EMU).
fn stored_memo_bounds(host: &mut Host, canvas: &str) -> EmuRect {
    let artifact = host.artifact(canvas);
    let ArtifactBody::Canvas(document) = &artifact.body else { panic!("a canvas") };
    document.board_memos.iter().find(|memo| memo.id == "memo-1").expect("the memo is stored").bounds
}

#[test]
fn dpi_click_on_canvas_text_reaches_the_core_at_the_clicked_point_at_each_scale() {
    for ppp in SCALES {
        let mut host = Host::start();
        let canvas = canvas_with_text(&mut host, "ABCDEFGH", TEXT_BOUNDS);
        let mut window = Window::open_with_previews(&mut host, &canvas);
        window.draw(&mut host, ppp);

        let target = quarter_into(&mut host, &window, &canvas, 3);
        let position = screen_of(&window, target);
        window.click(&mut host, ppp, position, false);

        // The point the window sent is the click's position in physical pixels. The click's f32 position carries a rounding
        // of about 1e-4 physical px at these coordinates, so the check allows 0.001 px.
        let (sent, answer) = last_call(&window, "render.hit_test");
        let x_micropx = sent["query"]["point"]["x_micropx"].as_i64().expect("an x point");
        let y_micropx = sent["query"]["point"]["y_micropx"].as_i64().expect("a y point");
        assert!(
            (x_micropx as f64 - f64::from(position.x) * f64::from(ppp) * 1e6).abs() <= 1000.0
                && (y_micropx as f64 - f64::from(position.y) * f64::from(ppp) * 1e6).abs() <= 1000.0,
            "at {ppp}: the window sent ({x_micropx}, {y_micropx}) micropx for the click at {position:?}",
        );

        // The core maps that point back to the page within one physical pixel (in page EMU at this transform),
        // and the node and grapheme it answers are the clicked ones.
        let transform = window.app.page_transforms.get(&0).expect("transform");
        let emu_per_px = 914_400.0 / (f64::from(transform.dpi) * f64::from(transform.zoom_milli) / 1000.0);
        let mapped = &answer["point_emu"];
        let (mx, my) = (mapped["x"].as_i64().expect("x") as f64, mapped["y"].as_i64().expect("y") as f64);
        assert!(
            (mx - target.0).abs() <= emu_per_px && (my - target.1).abs() <= emu_per_px,
            "at {ppp}: the core maps the click to ({mx}, {my}) EMU, the target is ({}, {}), one physical pixel is {emu_per_px} EMU",
            target.0,
            target.1,
        );
        assert_eq!(answer["hit"]["node_id"], "text-1", "at {ppp}: the click hits the text node");
        assert_eq!(answer["hit"]["position"]["grapheme_index"], 3, "at {ppp}: the core places the click on grapheme 3");
        // The window selects the node the core hit.
        assert_eq!(window.app.selected_nodes, vec!["text-1".to_owned()], "at {ppp}: the click selects the text the core hit");

        // Empty page space: the core answers no hit (`hit` is null), and the click clears the selection.
        let empty = screen_of(&window, (200_000.0, 200_000.0));
        window.click(&mut host, ppp, empty, false);
        let (_, empty_answer) = last_call(&window, "render.hit_test");
        assert!(empty_answer["hit"].is_null(), "at {ppp}: the core answers no hit on empty space");
        assert!(window.app.selected_nodes.is_empty(), "at {ppp}: a click on empty space clears the selection");
    }
}

#[test]
fn dpi_double_click_on_canvas_text_starts_editing_with_the_caret_on_the_clicked_grapheme_at_each_scale() {
    for ppp in SCALES {
        let mut host = Host::start();
        let canvas = canvas_with_text(&mut host, "ABCDEFGH", TEXT_BOUNDS);
        let mut window = Window::open_with_previews(&mut host, &canvas);
        window.draw(&mut host, ppp);

        let target = quarter_into(&mut host, &window, &canvas, 3);
        window.click(&mut host, ppp, screen_of(&window, target), true);

        assert!(window.app.inline_text_edit.is_some(), "at {ppp}: the double click starts inline editing");
        // The first frame that draws the editor requests its focus; the caret is painted from the next frame on.
        window.frame(&mut host, ppp, Vec::new());
        window.frame(&mut host, ppp, Vec::new());
        let state = egui::TextEdit::load_state(&window.ctx, window.app.text_editor_id).expect("the editor has a state");
        let caret = state.cursor.char_range().expect("the editor has a caret").primary.index;
        assert_eq!(caret, 3, "at {ppp}: the caret is in front of grapheme 3");

        // The caret egui draws is where the core puts grapheme 3 (the click lies a quarter into the grapheme, so the caret
        // is the boundary the core snaps it to, not the click point).
        let expected = core_caret_screen(&mut host, &window, &canvas, "text-1", 3);
        let drawn = drawn_caret_px(&window, ppp, expected);
        assert!(
            (drawn - expected.x).abs() <= 1.0,
            "at {ppp}: the caret is drawn at {drawn} px, the core's caret for grapheme 3 is at {} px",
            expected.x,
        );
    }
}

#[test]
fn dpi_editor_caret_lands_on_the_core_grapheme_it_is_given_at_each_scale() {
    for ppp in SCALES {
        let mut host = Host::start();
        let canvas = canvas_with_text(&mut host, "ABCDEFGH", TEXT_BOUNDS);
        let mut window = Window::open_with_previews(&mut host, &canvas);
        window.draw(&mut host, ppp);

        let position = TextPosition { paragraph_id: "text-1".into(), grapheme_index: 3, trailing: false };
        assert!(window.app.begin_inline_text_edit(&window.ctx, "text-1", 0, Some(position)), "at {ppp}: the text opens for editing");
        window.frame(&mut host, ppp, Vec::new());
        window.frame(&mut host, ppp, Vec::new());
        let state = egui::TextEdit::load_state(&window.ctx, window.app.text_editor_id).expect("the editor has a state");
        let caret = state.cursor.char_range().expect("the editor has a caret").primary.index;
        assert_eq!(caret, 3, "at {ppp}: the caret is in front of grapheme 3");

        let expected = core_caret_screen(&mut host, &window, &canvas, "text-1", 3);
        let drawn = drawn_caret_px(&window, ppp, expected);
        assert!(
            (drawn - expected.x).abs() <= 1.0,
            "at {ppp}: the caret is drawn at {drawn} px, the core's caret for grapheme 3 is at {} px",
            expected.x,
        );
    }
}

#[test]
fn dpi_text_node_editor_draws_the_page_font_size_so_the_caret_lands_on_the_core_grapheme_at_each_scale() {
    for ppp in SCALES {
        let mut host = Host::start();
        let canvas = canvas_with_text(&mut host, "ABCDEFGH", TEXT_BOUNDS);
        // 24 pt: the page draws it well above 8 px, where a wrong editor size shows as a caret miss.
        let patch = StylePatch { font_size: Some(24 * EMU_PER_PT), ..Default::default() };
        let intents = [Intent::Content, Intent::Design].into_iter().collect();
        host.apply(&canvas, vec![Operation::SetStyle { node_ids: vec!["text-1".into()], patch }], intents).expect("the font size is set");
        let mut window = Window::open_with_previews(&mut host, &canvas);
        window.draw(&mut host, ppp);
        let position = TextPosition { paragraph_id: "text-1".into(), grapheme_index: 6, trailing: false };
        assert!(window.app.begin_inline_text_edit(&window.ctx, "text-1", 0, Some(position)), "at {ppp}: the text opens for editing");
        window.frame(&mut host, ppp, Vec::new());
        window.frame(&mut host, ppp, Vec::new());
        let state = egui::TextEdit::load_state(&window.ctx, window.app.text_editor_id).expect("the editor has a state");
        let caret = state.cursor.char_range().expect("the editor has a caret").primary.index;
        assert_eq!(caret, 6, "at {ppp}: the caret is in front of grapheme 6");

        let expected = core_caret_screen(&mut host, &window, &canvas, "text-1", 6);
        let drawn = drawn_caret_px(&window, ppp, expected);
        assert!(
            (drawn - expected.x).abs() <= 1.0,
            "at {ppp}: the caret is drawn at {drawn} px, the core's caret for grapheme 6 is at {} px",
            expected.x,
        );
    }
}

#[test]
fn dpi_dragging_a_canvas_node_commits_the_pointer_distance_in_page_emu_at_each_scale() {
    for ppp in SCALES {
        let mut host = Host::start();
        let bounds = EmuRect { x: 1_000_000, y: 2_000_000, width: 3_000_000, height: 900_000 };
        let canvas = canvas_with_text(&mut host, "ABC", bounds);
        let mut window = Window::open_with_previews(&mut host, &canvas);
        window.draw(&mut host, ppp);
        // Snapping to guides moves a node by its own rule; this test measures the pointer distance alone.
        window.app.view().snap_guides = false;

        let (before, _) = stored_node(&mut host, &canvas);
        let area = window.app.page_textures.get(&0).expect("page 0 is drawn").page.source_map.iter()
            .find(|region| region.node_id == "text-1")
            .expect("the text is drawn")
            .bounds;
        let grab = screen_of(&window, (area.x as f64 + area.width as f64 / 2.0, area.y as f64 + area.height as f64 / 2.0));
        // 37 and 23 physical pixels, in four steps: the pointer moves on the way, as it does on a screen.
        let moved = Vec2::new(37.0 / ppp, 23.0 / ppp);
        window.frame(&mut host, ppp, vec![Event::PointerMoved(grab), primary_button(grab, true)]);
        for step in [0.25_f32, 0.5, 0.75, 1.0] {
            window.frame(&mut host, ppp, vec![Event::PointerMoved(grab + moved * step)]);
        }
        window.frame(&mut host, ppp, vec![primary_button(grab + moved, false)]);
        window.frame(&mut host, ppp, Vec::new());

        let (stored, _) = stored_node(&mut host, &canvas);
        let (per_x, per_y) = emu_per_physical_px(&window, ppp);
        let (dx, dy) = ((stored.x - before.x) as f64, (stored.y - before.y) as f64);
        assert!(
            (dx - 37.0 * per_x).abs() <= per_x,
            "at {ppp}: the node moved {dx} EMU in x; the pointer moved 37 physical px, {} EMU",
            37.0 * per_x,
        );
        assert!(
            (dy - 23.0 * per_y).abs() <= per_y,
            "at {ppp}: the node moved {dy} EMU in y; the pointer moved 23 physical px, {} EMU",
            23.0 * per_y,
        );
        assert_eq!((stored.width, stored.height), (before.width, before.height), "at {ppp}: the size is kept");
    }
}

#[test]
fn dpi_dragging_a_board_memo_moves_it_by_the_pointer_distance_at_each_scale() {
    for ppp in SCALES {
        let mut host = Host::start();
        let bounds = EmuRect { x: 1_000_000, y: 2_000_000, width: 3_000_000, height: 900_000 };
        let canvas = canvas_with_board_memo(&mut host, bounds);
        let mut window = Window::open_with_previews(&mut host, &canvas);
        window.settled_draw(&mut host, ppp);

        // The memo as the window draws it: its centre is the press point.
        let page_rect = window.app.page_rect.expect("page 0 is drawn");
        let grab = super::page_to_screen(bounds, page(&window).page.size, page_rect).center();
        let moved = Vec2::new(37.0 / ppp, 23.0 / ppp);
        window.frame(&mut host, ppp, vec![Event::PointerMoved(grab), primary_button(grab, true)]);
        for step in [0.25_f32, 0.5, 0.75, 1.0] {
            window.frame(&mut host, ppp, vec![Event::PointerMoved(grab + moved * step)]);
        }
        window.frame(&mut host, ppp, vec![primary_button(grab + moved, false)]);
        window.frame(&mut host, ppp, Vec::new());

        let stored = stored_memo_bounds(&mut host, &canvas);
        let (per_x, per_y) = emu_per_physical_px(&window, ppp);
        let (dx, dy) = ((stored.x - bounds.x) as f64, (stored.y - bounds.y) as f64);
        assert!(
            (dx - 37.0 * per_x).abs() <= per_x,
            "at {ppp}: the memo moved {dx} EMU in x; the pointer moved 37 physical px, {} EMU",
            37.0 * per_x,
        );
        assert!(
            (dy - 23.0 * per_y).abs() <= per_y,
            "at {ppp}: the memo moved {dy} EMU in y; the pointer moved 23 physical px, {} EMU",
            23.0 * per_y,
        );
        assert_eq!((stored.width, stored.height), (bounds.width, bounds.height), "at {ppp}: the size is kept");
    }
}

/// Makes the board's zoom fresh (not yet settled) for one frame, so no settled render is asked for in it.
fn unsettled_frame(window: &mut Window, host: &mut Host, ppp: f32, events: Vec<Event>) {
    window.app.view().zoom_changed = None;
    window.frame(host, ppp, events);
}

#[test]
fn dpi_a_settled_render_during_a_memo_drag_keeps_the_drag_and_moves_the_memo_at_each_scale() {
    for ppp in SCALES {
        let mut host = Host::start();
        let bounds = EmuRect { x: 1_000_000, y: 2_000_000, width: 3_000_000, height: 900_000 };
        let canvas = canvas_with_board_memo(&mut host, bounds);
        let mut window = Window::open_with_previews(&mut host, &canvas);
        window.settled_draw(&mut host, ppp);

        // A zoom made just now (1.1 is not a whole dpi at these scales, so its settle also snaps the zoom), and a memo drag
        // started while the zoom is still unsettled.
        let board_rect = window.app.board_rect.expect("the board is laid out");
        window.app.zoom_about(board_rect, board_rect.center(), 1.1);
        let zoomed = window.app.view_ref().zoom;
        let snapped = board::snapped_zoom(zoomed, ppp);
        assert_ne!(snapped, zoomed, "at {ppp}: the settle of this zoom snaps it");
        unsettled_frame(&mut window, &mut host, ppp, Vec::new());
        unsettled_frame(&mut window, &mut host, ppp, Vec::new());
        let page_rect = window.app.page_rect.expect("page 0 is drawn");
        let grab = super::page_to_screen(bounds, page(&window).page.size, page_rect).center();
        let moved = Vec2::new(37.0 / ppp, 23.0 / ppp);
        unsettled_frame(&mut window, &mut host, ppp, vec![Event::PointerMoved(grab), primary_button(grab, true)]);
        unsettled_frame(&mut window, &mut host, ppp, vec![Event::PointerMoved(grab + moved * 0.25)]);
        unsettled_frame(&mut window, &mut host, ppp, vec![Event::PointerMoved(grab + moved * 0.5)]);

        // The settle comes due while the drag goes on: the zoom is aged, and the frames ask for the settled render.
        window.app.view().zoom_changed = Some(f64::NEG_INFINITY);
        window.frame(&mut host, ppp, vec![Event::PointerMoved(grab + moved * 0.75)]);
        window.frame(&mut host, ppp, vec![Event::PointerMoved(grab + moved)]);
        let old_dpi = page(&window).dpi;
        assert_eq!(window.app.view_ref().zoom, zoomed, "at {ppp}: the drag holds the settle back");
        window.frame(&mut host, ppp, vec![primary_button(grab + moved, false)]);
        window.frame(&mut host, ppp, Vec::new());

        // The settle the drag held back happens once it ends: the zoom snaps, and the page is rendered again at its dpi.
        assert_eq!(window.app.view_ref().zoom, snapped, "at {ppp}: the settle snaps the zoom after the drag");
        let new_dpi = board::dpi_for(snapped, ppp);
        assert_ne!(new_dpi, old_dpi, "at {ppp}: the settled zoom has another dpi");
        let (asked, _) = last_call(&window, "render.get");
        assert_eq!(asked["profile"]["dpi"], new_dpi, "at {ppp}: the settle asks for the page at the settled dpi");
        assert_eq!(page(&window).dpi, new_dpi, "at {ppp}: the page texture is the settled render");

        let stored = stored_memo_bounds(&mut host, &canvas);
        let (per_x, per_y) = emu_per_physical_px(&window, ppp);
        let (dx, dy) = ((stored.x - bounds.x) as f64, (stored.y - bounds.y) as f64);
        assert!(
            (dx - 37.0 * per_x).abs() <= per_x,
            "at {ppp}: the memo moved {dx} EMU in x after a settle during the drag; the pointer moved 37 physical px, {} EMU",
            37.0 * per_x,
        );
        assert!(
            (dy - 23.0 * per_y).abs() <= per_y,
            "at {ppp}: the memo moved {dy} EMU in y after a settle during the drag; the pointer moved 23 physical px, {} EMU",
            23.0 * per_y,
        );
        assert_eq!((stored.width, stored.height), (bounds.width, bounds.height), "at {ppp}: the size is kept");
    }
}

/// A memo drag started (and moved half way) while a fresh zoom is unsettled. Returns the grab point, the pointer distance and
/// the zoom the settle snaps to.
fn memo_drag_after_a_zoom(window: &mut Window, host: &mut Host, ppp: f32, bounds: EmuRect) -> (Pos2, Vec2, f32) {
    window.settled_draw(host, ppp);
    let board_rect = window.app.board_rect.expect("the board is laid out");
    window.app.zoom_about(board_rect, board_rect.center(), 1.1);
    let snapped = board::snapped_zoom(window.app.view_ref().zoom, ppp);
    unsettled_frame(window, host, ppp, Vec::new());
    unsettled_frame(window, host, ppp, Vec::new());
    let page_rect = window.app.page_rect.expect("page 0 is drawn");
    let grab = super::page_to_screen(bounds, page(window).page.size, page_rect).center();
    let moved = Vec2::new(37.0 / ppp, 23.0 / ppp);
    unsettled_frame(window, host, ppp, vec![Event::PointerMoved(grab), primary_button(grab, true)]);
    unsettled_frame(window, host, ppp, vec![Event::PointerMoved(grab + moved * 0.5)]);
    assert!(window.app.board_memo_gesture.is_some(), "at {ppp}: the memo drag has started");
    (grab, moved, snapped)
}

/// After a memo drag whose release did not reach it: the drag is gone with its message, the memo is where it was, and the
/// settle the drag held back has happened (the zoom is snapped, the page is rendered at the settled dpi).
fn assert_lost_drag_ended_and_settled(window: &Window, host: &mut Host, canvas: &str, ppp: f32, bounds: EmuRect, snapped: f32) {
    assert!(window.app.board_memo_gesture.is_none(), "at {ppp}: the drag is ended");
    assert!(
        window.message().is_some_and(|message| message.contains("포인터 입력이 끊겨")),
        "at {ppp}: the window says the drag was cancelled: {:?}",
        window.message(),
    );
    assert_eq!(stored_memo_bounds(host, canvas), bounds, "at {ppp}: the memo keeps its place");
    assert_eq!(window.app.view_ref().zoom, snapped, "at {ppp}: the zoom settles");
    assert_eq!(page(window).dpi, board::dpi_for(snapped, ppp), "at {ppp}: the page is rendered at the settled dpi");
}

#[test]
fn dpi_a_memo_drag_left_with_the_button_up_ends_and_lets_the_zoom_settle_at_each_scale() {
    for ppp in SCALES {
        let mut host = Host::start();
        let bounds = EmuRect { x: 1_000_000, y: 2_000_000, width: 3_000_000, height: 900_000 };
        let canvas = canvas_with_board_memo(&mut host, bounds);
        let mut window = Window::open_with_previews(&mut host, &canvas);
        let (grab, moved, snapped) = memo_drag_after_a_zoom(&mut window, &mut host, ppp, bounds);

        // The release reaches egui but not the drag (it is taken out for that frame): the drag is left with the button up.
        let gesture = window.app.board_memo_gesture.take();
        unsettled_frame(&mut window, &mut host, ppp, vec![primary_button(grab + moved, false)]);
        window.app.board_memo_gesture = gesture;

        window.app.view().zoom_changed = Some(f64::NEG_INFINITY);
        window.frame(&mut host, ppp, Vec::new());
        assert_lost_drag_ended_and_settled(&window, &mut host, &canvas, ppp, bounds, snapped);
    }
}

#[test]
fn dpi_a_memo_drag_ends_when_the_window_loses_focus_and_lets_the_zoom_settle_at_each_scale() {
    for ppp in SCALES {
        let mut host = Host::start();
        let bounds = EmuRect { x: 1_000_000, y: 2_000_000, width: 3_000_000, height: 900_000 };
        let canvas = canvas_with_board_memo(&mut host, bounds);
        let mut window = Window::open_with_previews(&mut host, &canvas);
        let (grab, moved, snapped) = memo_drag_after_a_zoom(&mut window, &mut host, ppp, bounds);

        // The window loses focus with the button held (egui keeps it down), and the zoom's settle comes due.
        window.app.view().zoom_changed = Some(f64::NEG_INFINITY);
        window.frame(&mut host, ppp, vec![Event::WindowFocused(false), Event::PointerMoved(grab + moved)]);
        assert_lost_drag_ended_and_settled(&window, &mut host, &canvas, ppp, bounds, snapped);

        // The release that comes later commits nothing.
        window.frame(&mut host, ppp, vec![Event::WindowFocused(true), primary_button(grab + moved, false)]);
        window.frame(&mut host, ppp, Vec::new());
        assert_eq!(stored_memo_bounds(&mut host, &canvas), bounds, "at {ppp}: a late release does not move the memo");
    }
}

/// A whole-page `render.get` the host has not answered yet: its ticket, method and params.
type HeldRender = (u64, String, Value);

/// One frame as `Window::frame` draws it, at the frame time `clock` moved on by 0.05 s, whose whole-page renders
/// (`render.get` without a region) the host does not answer: they go to `held`, for `answer_held` to answer later, as a slow
/// render does. The host answers everything else at once.
fn frame_holding_page_renders(window: &mut Window, host: &mut Host, ppp: f32, clock: &mut f64, events: Vec<Event>, held: &mut Vec<HeldRender>) {
    *clock += 0.05;
    let mut input = screen(ppp, 1280.0, 800.0);
    input.time = Some(*clock);
    input.events = events;
    eframe::App::raw_input_hook(&mut window.app, &window.ctx, &mut input);
    let app = &mut window.app;
    let _ = window.ctx.run(input, |ctx| {
        app.draw_frame(ctx);
        app.observe_board_memo_gesture(ctx);
        app.finish_board_memo_gesture_if_released(ctx);
    });
    let (replies, calls) = (&window.replies, &mut window.calls);
    settle_work(&window.work, &mut window.app, &window.ctx, |item| match item {
        Work::Call { ticket, method, params, .. } if method == "render.get" && params["region"].is_null() => {
            held.push((ticket, method, params));
        }
        Work::Call { ticket, method, params, .. } => {
            let answer = host.call(&method, params.clone());
            calls.push((method, params, answer.as_ref().map_or(Value::Null, Value::clone)));
            replies.send(RpcReply { ticket, result: answer.map(RpcPayload::Json) }).expect("the window listens");
        }
        Work::ReadAsset { ticket, asset } => {
            replies.send(RpcReply { ticket, result: Ok(preview_pixels(host, &asset)) }).expect("the window listens");
        }
        _ => {}
    });
}

/// Answers the renders `frame_holding_page_renders` held, and the preview reads that follow them.
fn answer_held(window: &mut Window, host: &mut Host, held: &mut Vec<HeldRender>) {
    for (ticket, method, params) in held.drain(..) {
        let answer = host.call(&method, params.clone());
        window.calls.push((method, params, answer.as_ref().map_or(Value::Null, Value::clone)));
        window.replies.send(RpcReply { ticket, result: answer.map(RpcPayload::Json) }).expect("the window listens");
    }
    window.app.process_replies(&window.ctx);
    window.settle(host);
}

#[test]
fn dpi_a_sharp_render_asked_before_a_memo_drag_and_answered_during_it_keeps_the_drag_at_each_scale() {
    for ppp in SCALES {
        let mut host = Host::start();
        let bounds = EmuRect { x: 1_000_000, y: 2_000_000, width: 3_000_000, height: 900_000 };
        let canvas = canvas_with_board_memo(&mut host, bounds);
        let mut window = Window::open_with_previews(&mut host, &canvas);
        window.settled_draw(&mut host, ppp);
        let (mut clock, mut held) = (1_000.0, Vec::new());

        // A zoom whose settle has come due: the settle frame snaps the zoom and asks for the page at the new dpi, and that
        // render is slow (held).
        let board_rect = window.app.board_rect.expect("the board is laid out");
        window.app.zoom_about(board_rect, board_rect.center(), 1.1);
        let zoomed = window.app.view_ref().zoom;
        let old_dpi = page(&window).dpi;
        window.app.view().zoom_changed = Some(f64::NEG_INFINITY);
        frame_holding_page_renders(&mut window, &mut host, ppp, &mut clock, Vec::new(), &mut held);
        let snapped = board::snapped_zoom(zoomed, ppp);
        assert_eq!(window.app.view_ref().zoom, snapped, "at {ppp}: the settle snaps the zoom");
        let [(_, _, asked)] = held.as_slice() else { panic!("at {ppp}: the settle asks for one page render, held {held:?}") };
        let new_dpi = asked["profile"]["dpi"].as_u64().expect("a dpi") as u32;
        assert_eq!(new_dpi, board::dpi_for(snapped, ppp), "at {ppp}: the render is at the settled zoom's dpi");
        assert_ne!(new_dpi, old_dpi, "at {ppp}: the settled render changes the page's dpi");

        // The memo drag starts before the render arrives; the render arrives in the middle of it.
        let page_rect = window.app.page_rect.expect("page 0 is drawn");
        let grab = super::page_to_screen(bounds, page(&window).page.size, page_rect).center();
        let moved = Vec2::new(37.0 / ppp, 23.0 / ppp);
        frame_holding_page_renders(&mut window, &mut host, ppp, &mut clock, vec![Event::PointerMoved(grab), primary_button(grab, true)], &mut held);
        frame_holding_page_renders(&mut window, &mut host, ppp, &mut clock, vec![Event::PointerMoved(grab + moved * 0.25)], &mut held);
        frame_holding_page_renders(&mut window, &mut host, ppp, &mut clock, vec![Event::PointerMoved(grab + moved * 0.5)], &mut held);
        assert!(window.app.board_memo_gesture.is_some(), "at {ppp}: the memo drag has started");
        answer_held(&mut window, &mut host, &mut held);
        assert_eq!(page(&window).dpi, new_dpi, "at {ppp}: the page texture is the sharp render now");
        frame_holding_page_renders(&mut window, &mut host, ppp, &mut clock, vec![Event::PointerMoved(grab + moved * 0.75)], &mut held);
        frame_holding_page_renders(&mut window, &mut host, ppp, &mut clock, vec![Event::PointerMoved(grab + moved)], &mut held);
        assert!(held.is_empty(), "at {ppp}: no other page render is asked for during the drag, held {held:?}");
        frame_holding_page_renders(&mut window, &mut host, ppp, &mut clock, vec![primary_button(grab + moved, false)], &mut held);
        frame_holding_page_renders(&mut window, &mut host, ppp, &mut clock, Vec::new(), &mut held);
        answer_held(&mut window, &mut host, &mut held);

        assert!(
            window.message().is_none_or(|message| !message.contains("취소")),
            "at {ppp}: the drag is not cancelled: {:?}",
            window.message(),
        );
        let stored = stored_memo_bounds(&mut host, &canvas);
        let (per_x, per_y) = emu_per_physical_px(&window, ppp);
        let (dx, dy) = ((stored.x - bounds.x) as f64, (stored.y - bounds.y) as f64);
        assert!(
            (dx - 37.0 * per_x).abs() <= per_x,
            "at {ppp}: the memo moved {dx} EMU in x with a render answered during the drag; the pointer moved 37 physical px, {} EMU",
            37.0 * per_x,
        );
        assert!(
            (dy - 23.0 * per_y).abs() <= per_y,
            "at {ppp}: the memo moved {dy} EMU in y with a render answered during the drag; the pointer moved 23 physical px, {} EMU",
            23.0 * per_y,
        );
        assert_eq!((stored.width, stored.height), (bounds.width, bounds.height), "at {ppp}: the size is kept");
    }
}

#[test]
fn dpi_a_hit_reply_without_the_hit_field_is_refused_and_keeps_the_selection() {
    let mut host = Host::start();
    let canvas = canvas_with_text(&mut host, "ABCDEFGH", TEXT_BOUNDS);
    let mut window = Window::open_with_previews(&mut host, &canvas);
    window.draw(&mut host, 1.0);

    // The text is selected by a real click, and the core's answer carries `hit`.
    let on_text = screen_of(&window, quarter_into(&mut host, &window, &canvas, 3));
    window.click(&mut host, 1.0, on_text, false);
    assert_eq!(window.app.selected_nodes, vec!["text-1".to_owned()], "the click selects the text");

    // A successful reply without `hit` does not answer the click: it is refused with the message, and the selection stays.
    let empty = screen_of(&window, (200_000.0, 200_000.0));
    window.app.hit_test_page(0, empty, 1.0, false, false);
    window.answer_queued("render.hit_test", json!({ "point_emu": { "x": 200_000, "y": 200_000 } }));
    assert_eq!(window.app.selected_nodes, vec!["text-1".to_owned()], "a reply without `hit` keeps the selection");
    assert!(
        window.message().is_some_and(|message| message.contains("선택을 읽지 못했습니다")),
        "a reply without `hit` is refused with the message",
    );

    // A null `hit` is empty space: the click clears the selection.
    window.app.hit_test_page(0, empty, 1.0, false, false);
    window.answer_queued("render.hit_test", json!({ "hit": null }));
    assert!(window.app.selected_nodes.is_empty(), "a null `hit` clears the selection");
}

#[test]
fn dpi_ime_composition_inserts_one_syllable_when_the_scale_changes_during_composition() {
    let mut host = Host::start();
    let canvas = canvas_with_text(&mut host, "가나다라", TEXT_BOUNDS);
    let mut window = Window::open_with_previews(&mut host, &canvas);
    window.draw(&mut host, 1.0);

    // Editing starts with the caret in front of grapheme 2, the position a hit reply carries; no click is made.
    let position = TextPosition { paragraph_id: "text-1".into(), grapheme_index: 2, trailing: false };
    assert!(window.app.begin_inline_text_edit(&window.ctx, "text-1", 0, Some(position)), "the text opens for editing");
    window.frame(&mut host, 1.0, Vec::new());

    // Composition starts at 1.0, continues at 1.5, commits at 2.0, then editing ends.
    window.frame(&mut host, 1.0, vec![Event::Ime(ImeEvent::Enabled), Event::Ime(ImeEvent::Preedit("ㅎ".into()))]);
    window.frame(&mut host, 1.5, vec![Event::Ime(ImeEvent::Preedit("하".into()))]);
    window.frame(&mut host, 2.0, vec![Event::Ime(ImeEvent::Commit("하".into()))]);
    window.frame(&mut host, 2.0, Vec::new());
    let escape = Event::Key { key: Key::Escape, physical_key: Some(Key::Escape), pressed: true, repeat: false, modifiers: Modifiers::NONE };
    window.frame(&mut host, 2.0, vec![escape]);
    window.frame(&mut host, 2.0, Vec::new());

    let (_, text) = stored_node(&mut host, &canvas);
    assert_eq!(text, "가나하다라", "the syllable is inserted once, at the caret, and no composition text is left behind");
}

#[test]
fn dpi_double_click_on_a_canvas_table_cell_starts_editing_with_the_caret_on_the_clicked_grapheme_at_each_scale() {
    for ppp in SCALES {
        let mut host = Host::start();
        let (canvas, cells) = canvas_with_table_texts(&mut host, ["가나다", "라마"]);
        let mut window = Window::open_with_previews(&mut host, &canvas);
        window.draw(&mut host, ppp);
        let cell = cells[0].as_str();

        // A quarter into grapheme 1 of the first cell's text: the core places it on grapheme 1 of that cell.
        let left = core_caret_in(&mut host, &window, &canvas, cell, 1);
        let right = core_caret_in(&mut host, &window, &canvas, cell, 2);
        let target = (left.x as f64 + (right.x - left.x) as f64 / 4.0, left.y as f64 + left.height as f64 / 2.0);
        window.click(&mut host, ppp, screen_of(&window, target), true);

        let (_, answer) = last_call(&window, "render.hit_test");
        assert_eq!(answer["hit"]["node_id"], cell, "at {ppp}: the click hits the table cell");
        assert_eq!(answer["hit"]["position"]["grapheme_index"], 1, "at {ppp}: the core places the click on grapheme 1 of the cell");
        assert_eq!(window.app.selected_nodes, vec![cell.to_owned()], "at {ppp}: the click selects the cell");

        let edit = window.app.inline_text_edit.as_ref().expect("the double click starts editing the cell");
        assert_eq!(edit.node_id, cell, "at {ppp}: the editor is on the clicked cell");
        window.frame(&mut host, ppp, Vec::new());
        window.frame(&mut host, ppp, Vec::new());
        let state = egui::TextEdit::load_state(&window.ctx, window.app.text_editor_id).expect("the editor has a state");
        let caret = state.cursor.char_range().expect("the editor has a caret").primary.index;
        assert_eq!(caret, 1, "at {ppp}: the caret is in front of grapheme 1 of the cell");

        let expected = core_caret_screen(&mut host, &window, &canvas, cell, 1);
        let drawn = drawn_caret_px(&window, ppp, expected);
        assert!(
            (drawn - expected.x).abs() <= 1.0,
            "at {ppp}: the caret is drawn at {drawn} px, the core's caret for grapheme 1 of the cell is at {} px",
            expected.x,
        );
    }
}
