//! GUI <-> MCP parity of the five operations of task 39 and of the WebCode ZIP export (task 19). Each test sends one change through the registered command that
//! carries it (`operation_commands.rs`), with the window's requests answered by the real host. It then sends the same change
//! through the MCP `changes_apply` tool on a second artifact of the same host, built the same way, and compares the content
//! digest and the serialised body of the two results.

use std::path::{Path, PathBuf};

use designllm_contracts::{
    new_id, Artifact, ArtifactBody, ArtifactKind, ArtifactSummary, CanvasBoardMemo, CanvasPageSection, Editability,
    ErrorSpec, ErrorValues, ExportFileRequest, ExportRequest, Intent, Node, Operation, PanelBody, PixelRect, PlotDisplayMode, PlotErrorKind,
    PlotSpec,
};
use serde_json::json;

use super::{
    chrome::export_formats,
    figure_tools::chart_change,
    page_sections::SectionEditor,
    state::BoardMemoEditor,
    test_support::{canvas_with_page, change_set_arguments, content_of, Host, HostBackend, Mcp, Window},
};

/// The only page of the canvases these tests build.
const PAGE: &str = "page-1";
/// The picture node of the image tests.
const PICTURE: &str = "picture";

/// A canvas with one picture, `PICTURE`, that shows the asset `sha256` of `bytes` bytes.
fn canvas_with_picture(host: &mut Host, title: &str, sha256: &str, bytes: u64) -> String {
    let id = canvas_with_page(host, title);
    let node: Node = serde_json::from_value(json!({
        "id": PICTURE,
        "kind": "image",
        "bounds": { "x": 100_000, "y": 100_000, "width": 1_200_000, "height": 800_000 },
        "asset": { "sha256": sha256, "mime": "image/png", "bytes": bytes }
    }))
    .expect("a picture node");
    host.apply(&id, vec![Operation::AddNode { page_id: PAGE.into(), parent_id: None, node }], [Intent::Content].into_iter().collect())
        .expect("the picture");
    id
}

/// Writes a 4 x 4 PNG of one colour into the scratch folder `dir` and returns its path.
fn png(dir: &Path, name: &str, colour: [u8; 4]) -> PathBuf {
    let path = dir.join(name);
    image::RgbaImage::from_pixel(4, 4, image::Rgba(colour)).save(&path).expect("the picture is written");
    path
}

/// A scratch folder unique to one test (`std::env::temp_dir()` below a per-test folder, BRIEF 11).
fn scratch_dir(test: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("designllm-desktop-parity-{test}-{}", new_id()));
    std::fs::create_dir_all(&dir).expect("a scratch folder");
    dir
}

/// Imports a local file into the host's asset store: its hash and its size in bytes.
fn import(host: &mut Host, path: &Path) -> (String, u64) {
    let asset = host.call("asset.import", json!({ "path": path.to_string_lossy() })).expect("the picture imports");
    (asset["sha256"].as_str().expect("a hash").to_owned(), asset["bytes"].as_u64().expect("a size"))
}

/// The artifact after one change through MCP: `operations` sent through `changes_apply` at the artifact's current revision.
fn mcp_change(backend: &HostBackend, mcp: &Mcp, artifact: &str, intents: &[Intent], operations: Vec<Operation>) -> Artifact {
    let (workspace, revision) = backend.with(|host| (host.workspace_id.clone(), host.artifact(artifact).revision));
    mcp.call("changes_apply", change_set_arguments(&workspace, artifact, revision, intents, operations));
    backend.with(|host| host.artifact(artifact))
}

/// Both results are the same content, and the change did something: the body differs from the one before it.
fn assert_same_content(before: &Artifact, gui: &Artifact, mcp: &Artifact) {
    assert_ne!(content_of(before), content_of(gui), "the command changed nothing");
    assert_eq!(content_of(gui), content_of(mcp), "the command and the MCP tool left different content");
}

/// The error-key mode of the first panel of a figure.
fn plot_display(artifact: &Artifact) -> Option<PlotDisplayMode> {
    match &artifact.body {
        ArtifactBody::Figure(figure) => match &figure.panels[0].body {
            PanelBody::Plot(plot) => plot.display,
            _ => None,
        },
        _ => panic!("a figure"),
    }
}

/// The asset hash of the picture on the canvas's only page.
fn picture_sha(artifact: &Artifact) -> String {
    let ArtifactBody::Canvas(document) = &artifact.body else { panic!("a canvas") };
    document.pages[0]
        .nodes
        .iter()
        .find(|node| node.id == PICTURE)
        .and_then(|node| node.asset.as_ref())
        .map(|asset| asset.sha256.clone())
        .expect("the picture has an asset")
}

/// Gives the chart of `figure` error bars over its `sd` column: the error key is offered for a plot with error bars only.
fn with_error_bars(host: &mut Host, figure: &str) {
    let (document, _) = host.figure(figure);
    let panel = &document.panels[0];
    let PanelBody::Plot(plot) = &panel.body else { panic!("a chart") };
    let column = document.datasets[0].columns.iter().find(|column| column.name == "sd").expect("the sd column").id.clone();
    let error = ErrorSpec { kind: PlotErrorKind::Sd, values: ErrorValues::Symmetric { column }, sample_count_column: None, confidence_level: None };
    let plot = PlotSpec { error: Some(error), ..plot.clone() };
    let intents = [Intent::Data, Intent::Design].into_iter().collect();
    host.apply(figure, vec![Operation::SetPlot { panel_id: panel.id.clone(), plot }], intents).expect("the error bars");
}

/// Two figures built the same way, each one chart with error bars: their ids and the chart panel's id (the same in both).
fn two_charts_with_error_bars(host: &mut Host) -> (String, String, String) {
    let mut draft = host.declared_draft("id,value,sd\nA,12,1\nB,18,2\nC,25,1\nD,31,3\n", |_| {});
    draft.y_unit = "%".into();
    let first = host.create("figure", "그림");
    let second = host.create("figure", "그림");
    let (empty, _) = host.figure(&first);
    let (operations, intents) = chart_change(&empty, &draft, true).expect("a chart");
    host.apply(&first, operations.clone(), intents.clone()).expect("the chart");
    host.apply(&second, operations, intents).expect("the same chart");
    with_error_bars(host, &first);
    with_error_bars(host, &second);
    let panel = host.figure(&first).0.panels[0].id.clone();
    assert_eq!(host.figure(&second).0.panels[0].id, panel, "the panel ids agree");
    (first, second, panel)
}

#[test]
fn the_error_key_switch_gives_the_same_figure_by_command_and_by_tool() {
    let mut host = Host::start();
    let (gui, mcp_figure, panel) = two_charts_with_error_bars(&mut host);
    let before = host.artifact(&gui);
    assert_eq!(plot_display(&before), None, "the plot shows no key before the switch");

    let mut window = Window::open(&mut host, &gui);
    window.step(&mut host, "figure.plot_display", json!({ "panel_id": panel.clone(), "on": true })).expect("the switch is sent");
    let gui_after = host.artifact(&gui);
    drop(window);

    let backend = HostBackend::new(host);
    let mcp = Mcp::connect(backend.clone());
    let operation = Operation::SetPlotDisplay { panel_id: panel, expected_display: None, display: Some(PlotDisplayMode::ErrorKey) };
    let mcp_after = mcp_change(&backend, &mcp, &mcp_figure, &[Intent::Design], vec![operation]);
    assert_eq!(plot_display(&gui_after), Some(PlotDisplayMode::ErrorKey), "the command switched the key on");
    assert_same_content(&before, &gui_after, &mcp_after);
}

/// The centre of the one text `label` the window's last frame drew.
fn drawn_at(window: &Window, label: &str) -> eframe::egui::Pos2 {
    use eframe::egui::epaint::Shape;
    fn walk(shape: &Shape, label: &str, found: &mut Vec<eframe::egui::Pos2>) {
        match shape {
            Shape::Text(text) if text.galley.text() == label => found.push(text.pos + text.galley.size() / 2.0),
            Shape::Vec(shapes) => shapes.iter().for_each(|shape| walk(shape, label, found)),
            _ => {}
        }
    }
    let mut found = Vec::new();
    window.shapes.iter().for_each(|clipped| walk(&clipped.shape, label, &mut found));
    match found.as_slice() {
        [centre] => *centre,
        _ => panic!("{} texts {label:?} drawn", found.len()),
    }
}

/// The switch's own call site (backlog desktop 28: parity sent the command only): a click on the checkbox in a drawn window
/// leaves the figure the command leaves.
#[test]
fn a_click_on_the_error_key_checkbox_gives_the_figure_of_the_command() {
    let mut host = Host::start();
    let (clicked, commanded, panel) = two_charts_with_error_bars(&mut host);
    let before = host.artifact(&clicked);

    let mut window = Window::open(&mut host, &clicked);
    window.step(&mut host, "select.node", json!({ "node_id": panel.clone() })).expect("the chart panel is selected");
    window.draw(&mut host, 1.0);
    // The panel's head row (its close button, `chrome.rs`) puts the checkbox under the fold of the 800 px window: the panel is
    // scrolled first, as a user would.
    use eframe::egui::{Event, Modifiers, MouseWheelUnit, Pos2, Vec2};
    let wheel = Event::MouseWheel { unit: MouseWheelUnit::Point, delta: Vec2::new(0.0, -120.0), modifiers: Modifiers::NONE };
    window.frame(&mut host, 1.0, vec![Event::PointerMoved(Pos2::new(1180.0, 400.0)), wheel]);
    window.draw(&mut host, 1.0);
    let position = drawn_at(&window, "오차 기준 표시");
    window.click(&mut host, 1.0, position, false);
    let by_click = host.artifact(&clicked);
    drop(window);

    let mut window = Window::open(&mut host, &commanded);
    window.step(&mut host, "figure.plot_display", json!({ "panel_id": panel, "on": true })).expect("the switch is sent");
    let by_command = host.artifact(&commanded);
    assert_eq!(plot_display(&by_click), Some(PlotDisplayMode::ErrorKey), "the click switched the key on");
    assert_same_content(&before, &by_click, &by_command);
}

#[test]
fn the_image_replacement_gives_the_same_canvas_by_command_and_by_tool() {
    let mut host = Host::start();
    let dir = scratch_dir("replace");
    let first = png(&dir, "first.png", [200, 40, 40, 255]);
    let second = png(&dir, "second.png", [40, 200, 40, 255]);
    let (sha, bytes) = import(&mut host, &first);
    // The command takes an asset that is already in the store, as the MCP tool does.
    let (new_sha, _) = import(&mut host, &second);
    let gui = canvas_with_picture(&mut host, "그림판", &sha, bytes);
    let mcp_canvas = canvas_with_picture(&mut host, "그림판", &sha, bytes);
    let before = host.artifact(&gui);

    let mut window = Window::open(&mut host, &gui);
    window.step(&mut host, "select.node", json!({ "node_id": PICTURE })).expect("the picture is selected");
    window
        .step(&mut host, "canvas.image.replace", json!({ "node_id": PICTURE, "asset_sha256": new_sha.clone() }))
        .expect("the replacement is sent");
    let gui_after = host.artifact(&gui);
    drop(window);

    let by_run = canvas_with_picture(&mut host, "그림판", &sha, bytes);
    let mut window = Window::open(&mut host, &by_run);
    window.step(&mut host, "select.node", json!({ "node_id": PICTURE })).expect("picture selected");
    let run = window.agent_run(&mut host, "canvas.image.replace", json!({ "node_id": PICTURE, "asset_sha256": new_sha }));
    assert_eq!(run["status"], "completed", "{run}");
    assert_same_content(&before, &gui_after, &host.artifact(&by_run));

    let backend = HostBackend::new(host);
    let mcp = Mcp::connect(backend.clone());
    let imported = mcp.call("asset_import", json!({ "path": second.to_string_lossy() }));
    assert_eq!(imported["sha256"].as_str(), Some(new_sha.as_str()), "the tool imports the same asset");
    let operation = Operation::ReplaceImageAsset { node_id: PICTURE.into(), expected_asset_sha256: sha, asset_sha256: new_sha.clone() };
    let mcp_after = mcp_change(&backend, &mcp, &mcp_canvas, &[Intent::Content], vec![operation]);
    assert_eq!(picture_sha(&gui_after), new_sha, "the command put the second picture in");
    assert_same_content(&before, &gui_after, &mcp_after);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_image_crop_gives_the_same_canvas_by_command_and_by_tool() {
    let mut host = Host::start();
    let dir = scratch_dir("crop");
    let (sha, bytes) = import(&mut host, &png(&dir, "picture.png", [200, 40, 40, 255]));
    let gui = canvas_with_picture(&mut host, "그림판", &sha, bytes);
    let mcp_canvas = canvas_with_picture(&mut host, "그림판", &sha, bytes);
    let before = host.artifact(&gui);

    let mut window = Window::open(&mut host, &gui);
    window
        .step(
            &mut host,
            "canvas.image.crop",
            json!({ "node_id": PICTURE, "expected_asset_sha256": sha.clone(), "x": 1, "y": 1, "width": 2, "height": 2 }),
        )
        .expect("the crop is sent");
    let gui_after = host.artifact(&gui);
    drop(window);

    let backend = HostBackend::new(host);
    let mcp = Mcp::connect(backend.clone());
    let crop_pixel_rect = Some(PixelRect { x: 1, y: 1, width: 2, height: 2 });
    let operation = Operation::SetCanvasImageCrop { node_id: PICTURE.into(), expected_asset_sha256: sha, crop_pixel_rect };
    let mcp_after = mcp_change(&backend, &mcp, &mcp_canvas, &[Intent::Design], vec![operation]);
    assert_same_content(&before, &gui_after, &mcp_after);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_board_memos_give_the_same_canvas_by_command_and_by_tool() {
    let mut host = Host::start();
    let gui = canvas_with_page(&mut host, "보드");
    let mcp_canvas = canvas_with_page(&mut host, "보드");
    let before = host.artifact(&gui);
    let memo = json!({
        "id": "memo-1",
        "page_id": PAGE,
        "bounds": { "x": 100_000, "y": 100_000, "width": 1_200_000, "height": 600_000 },
        "text": "검토 필요",
        "color": "#FFF2AB"
    });

    let mut window = Window::open(&mut host, &gui);
    window.step(&mut host, "canvas.board_memos.set", json!({ "page_id": PAGE, "expected": [], "memos": [memo.clone()] })).expect("the memos are sent");
    let gui_after = host.artifact(&gui);
    drop(window);

    let backend = HostBackend::new(host);
    let mcp = Mcp::connect(backend.clone());
    let memos: Vec<CanvasBoardMemo> = serde_json::from_value(json!([memo])).expect("a memo");
    let operation = Operation::SetCanvasBoardMemos { page_id: PAGE.into(), expected: Vec::new(), memos };
    let mcp_after = mcp_change(&backend, &mcp, &mcp_canvas, &[Intent::Content], vec![operation]);
    assert_same_content(&before, &gui_after, &mcp_after);
}

#[test]
fn the_page_sections_give_the_same_canvas_by_command_and_by_tool() {
    let mut host = Host::start();
    let gui = canvas_with_page(&mut host, "섹션");
    let mcp_canvas = canvas_with_page(&mut host, "섹션");
    let before = host.artifact(&gui);
    let section = json!({ "id": "section-1", "name": "1장", "start_page_id": PAGE });

    let mut window = Window::open(&mut host, &gui);
    window.step(&mut host, "canvas.page_sections.set", json!({ "expected": [], "sections": [section.clone()] })).expect("the sections are sent");
    let gui_after = host.artifact(&gui);
    drop(window);

    let backend = HostBackend::new(host);
    let mcp = Mcp::connect(backend.clone());
    let sections: Vec<CanvasPageSection> = serde_json::from_value(json!([section])).expect("a section");
    let operation = Operation::SetCanvasPageSections { expected: Vec::new(), sections };
    let mcp_after = mcp_change(&backend, &mcp, &mcp_canvas, &[Intent::Content], vec![operation]);
    assert_same_content(&before, &gui_after, &mcp_after);
}

#[test]
fn the_replacement_command_refuses_an_image_that_is_not_the_selected_one() {
    let mut host = Host::start();
    let dir = scratch_dir("refuse-node");
    let (sha, bytes) = import(&mut host, &png(&dir, "first.png", [200, 40, 40, 255]));
    let (new_sha, _) = import(&mut host, &png(&dir, "second.png", [40, 200, 40, 255]));
    let gui = canvas_with_picture(&mut host, "그림판", &sha, bytes);
    let before = host.artifact(&gui);

    let mut window = Window::open(&mut host, &gui);
    window.step(&mut host, "select.node", json!({ "node_id": PICTURE })).expect("the picture is selected");
    let message = window
        .step(&mut host, "canvas.image.replace", json!({ "node_id": "other", "asset_sha256": new_sha.clone() }))
        .expect_err("an image that is not selected is refused");
    assert_eq!(message, "지정한 이미지가 선택한 이미지와 다릅니다. 그 이미지를 먼저 고르세요");
    assert_eq!(host.artifact(&gui).revision, before.revision, "no change was sent");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_replacement_command_leaves_the_picture_when_the_workspace_lacks_the_asset() {
    let mut host = Host::start();
    let dir = scratch_dir("refuse-asset");
    let (sha, bytes) = import(&mut host, &png(&dir, "first.png", [200, 40, 40, 255]));
    let gui = canvas_with_picture(&mut host, "그림판", &sha, bytes);
    let before = host.artifact(&gui);
    let missing = "f".repeat(64);

    let mut window = Window::open(&mut host, &gui);
    window.step(&mut host, "select.node", json!({ "node_id": PICTURE })).expect("the picture is selected");
    window
        .step(&mut host, "canvas.image.replace", json!({ "node_id": PICTURE, "asset_sha256": missing.clone() }))
        .expect("the change is sent, and the core answers it");
    assert!(window.app.refusal.is_some(), "the window shows the core's refusal");
    assert_eq!(content_of(&host.artifact(&gui)), content_of(&before), "the picture keeps its asset");

    // The same change sent to the core directly: the core does not hold the asset, so it answers NotFound.
    let operation = Operation::ReplaceImageAsset { node_id: PICTURE.into(), expected_asset_sha256: sha, asset_sha256: missing };
    let change = change_set_arguments(&host.workspace_id, &gui, before.revision, &[Intent::Content], vec![operation]);
    let error = host.call("changes.apply", change).expect_err("the core refuses an asset it does not hold");
    assert_eq!(error.code, designllm_contracts::ErrorCode::NotFound);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_replacement_command_is_refused_while_a_replacement_is_on_its_way() {
    let mut host = Host::start();
    let dir = scratch_dir("refuse-busy");
    let first = png(&dir, "first.png", [200, 40, 40, 255]);
    let (sha, bytes) = import(&mut host, &first);
    let (new_sha, _) = import(&mut host, &png(&dir, "second.png", [40, 200, 40, 255]));
    let gui = canvas_with_picture(&mut host, "그림판", &sha, bytes);

    let mut window = Window::open(&mut host, &gui);
    window.step(&mut host, "select.node", json!({ "node_id": PICTURE })).expect("the picture is selected");
    // A picture that the image button's dialog handed over is still being imported: the replacement is not finished.
    let intent = window.app.image_replacement_start().expect("the picture can start a replacement");
    window.app.start_canvas_image_replacement_import(intent, first).expect("the import starts");
    let message = window
        .app
        .invoke("canvas.image.replace", json!({ "node_id": PICTURE, "asset_sha256": new_sha.clone() }))
        .expect_err("a second replacement is refused while the first one is on its way");
    assert_eq!(message, "이미지 교체가 이미 진행 중입니다");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The crop of the picture on the canvas's only page.
fn picture_crop(artifact: &Artifact) -> Option<PixelRect> {
    let ArtifactBody::Canvas(document) = &artifact.body else { panic!("a canvas") };
    document.pages[0].nodes.iter().find(|node| node.id == PICTURE).and_then(|node| node.ext.image_crop_pixel_rect)
}

/// The crop button applies its crop through the command: the result is the one the MCP tool gives, and one undo takes it back.
#[test]
fn the_crop_button_gives_the_same_canvas_by_button_and_by_tool() {
    let mut host = Host::start();
    let dir = scratch_dir("crop-button");
    let (sha, bytes) = import(&mut host, &png(&dir, "picture.png", [200, 40, 40, 255]));
    let gui = canvas_with_picture(&mut host, "그림판", &sha, bytes);
    let mcp_canvas = canvas_with_picture(&mut host, "그림판", &sha, bytes);
    let before = host.artifact(&gui);
    let crop = PixelRect { x: 1, y: 1, width: 2, height: 2 };

    let mut window = Window::open(&mut host, &gui);
    window.step(&mut host, "select.node", json!({ "node_id": PICTURE })).expect("the picture is selected");
    window.app.begin_canvas_image_crop(PICTURE).expect("the crop starts");
    window.settle(&mut host);
    window.app.apply_canvas_image_crop(Some(crop)).expect("the button applies the crop");
    window.settle(&mut host);
    let gui_after = host.artifact(&gui);
    assert_eq!(picture_crop(&gui_after), Some(crop), "the button put the crop in");
    assert!(window.app.image_crop_editor.is_none(), "the crop editor closes when its crop lands");
    assert_eq!(window.app.selected_nodes, [PICTURE.to_owned()], "the picture stays selected");
    assert_eq!(gui_after.revision, before.revision + 1, "the crop is one change record");
    window.app.history(false).expect("the undo starts");
    window.settle(&mut host);
    assert_eq!(content_of(&host.artifact(&gui)), content_of(&before), "one undo takes the crop back");
    drop(window);

    let backend = HostBackend::new(host);
    let mcp = Mcp::connect(backend.clone());
    let operation = Operation::SetCanvasImageCrop { node_id: PICTURE.into(), expected_asset_sha256: sha, crop_pixel_rect: Some(crop) };
    let mcp_after = mcp_change(&backend, &mcp, &mcp_canvas, &[Intent::Design], vec![operation]);
    assert_same_content(&before, &gui_after, &mcp_after);
    let _ = std::fs::remove_dir_all(&dir);
}

/// The image button's replacement: its dialog's file is imported and applied through the command, with the result the MCP tool gives.
#[test]
fn the_replacement_button_gives_the_same_canvas_by_button_and_by_tool() {
    let mut host = Host::start();
    let dir = scratch_dir("replace-button");
    let first = png(&dir, "first.png", [200, 40, 40, 255]);
    let second = png(&dir, "second.png", [40, 200, 40, 255]);
    let (sha, bytes) = import(&mut host, &first);
    let (new_sha, _) = import(&mut host, &second);
    let gui = canvas_with_picture(&mut host, "그림판", &sha, bytes);
    let mcp_canvas = canvas_with_picture(&mut host, "그림판", &sha, bytes);
    let before = host.artifact(&gui);

    let mut window = Window::open(&mut host, &gui);
    window.step(&mut host, "select.node", json!({ "node_id": PICTURE })).expect("the picture is selected");
    // The dialog has handed over the file: its import, and then the replacement, follow.
    let intent = window.app.image_replacement_start().expect("the picture can start a replacement");
    window.app.start_canvas_image_replacement_import(intent, second.clone()).expect("the import starts");
    window.settle(&mut host);
    let gui_after = host.artifact(&gui);
    assert!(window.app.pending_image_replacement.is_none(), "the replacement finishes when its change lands");
    assert_eq!(window.app.selected_nodes, [PICTURE.to_owned()], "the picture stays selected");
    assert_eq!(gui_after.revision, before.revision + 1, "the replacement is one change record");
    window.app.history(false).expect("the undo starts");
    window.settle(&mut host);
    assert_eq!(content_of(&host.artifact(&gui)), content_of(&before), "one undo takes the replacement back");
    drop(window);

    let backend = HostBackend::new(host);
    let mcp = Mcp::connect(backend.clone());
    let operation = Operation::ReplaceImageAsset { node_id: PICTURE.into(), expected_asset_sha256: sha, asset_sha256: new_sha.clone() };
    let mcp_after = mcp_change(&backend, &mcp, &mcp_canvas, &[Intent::Content], vec![operation]);
    assert_eq!(picture_sha(&gui_after), new_sha, "the button put the second picture in");
    assert_same_content(&before, &gui_after, &mcp_after);
    let _ = std::fs::remove_dir_all(&dir);
}

/// The memo editor's save goes through the memo command: the result is the one the MCP tool gives.
#[test]
fn the_board_memo_button_gives_the_same_canvas_by_button_and_by_tool() {
    let mut host = Host::start();
    let gui = canvas_with_page(&mut host, "보드");
    let mcp_canvas = canvas_with_page(&mut host, "보드");
    let before = host.artifact(&gui);
    let memo = json!({
        "id": "memo-1",
        "page_id": PAGE,
        "bounds": { "x": 100_000, "y": 100_000, "width": 1_200_000, "height": 600_000 },
        "text": "검토 필요",
        "color": "#FFF2AB"
    });

    let mut window = Window::open_with_previews(&mut host, &gui);
    window.draw(&mut host, 1.0);
    let (artifact_id, revision, page_id, expected) = window.app.board_memo_page(0).expect("the page is shown");
    window.app.board_memo_editor = Some(BoardMemoEditor {
        artifact_id,
        revision,
        page_id,
        page_index: 0,
        expected,
        memo: serde_json::from_value(memo.clone()).expect("a memo"),
        is_new: true,
        message: None,
    });
    window.app.save_board_memo_editor();
    window.settle(&mut host);
    let gui_after = host.artifact(&gui);
    assert!(window.app.board_memo_mutation.is_none(), "the memo change finishes as its own mutation's change");
    assert_eq!(window.app.selected_board_memo.as_deref(), Some("memo-1"), "the edited memo is selected again");
    assert_eq!(gui_after.revision, before.revision + 1, "the memos are one change record");
    window.app.history(false).expect("the undo starts");
    window.settle(&mut host);
    assert_eq!(content_of(&host.artifact(&gui)), content_of(&before), "one undo takes the memos back");
    drop(window);

    let backend = HostBackend::new(host);
    let mcp = Mcp::connect(backend.clone());
    let memos: Vec<CanvasBoardMemo> = serde_json::from_value(json!([memo])).expect("a memo");
    let operation = Operation::SetCanvasBoardMemos { page_id: PAGE.into(), expected: Vec::new(), memos };
    let mcp_after = mcp_change(&backend, &mcp, &mcp_canvas, &[Intent::Content], vec![operation]);
    assert_same_content(&before, &gui_after, &mcp_after);
}

/// The section editor's save goes through the section command: the result is the one the MCP tool gives.
#[test]
fn the_page_section_button_gives_the_same_canvas_by_button_and_by_tool() {
    let mut host = Host::start();
    let gui = canvas_with_page(&mut host, "섹션");
    let mcp_canvas = canvas_with_page(&mut host, "섹션");
    let before = host.artifact(&gui);
    let section = json!({ "id": "section-1", "name": "1장", "start_page_id": PAGE });

    let mut window = Window::open(&mut host, &gui);
    window.app.page_section_editor = Some(SectionEditor {
        artifact_id: gui.clone(),
        revision: before.revision,
        expected: Vec::new(),
        section: serde_json::from_value(section.clone()).expect("a section"),
        page_index: 0,
        is_new: true,
        message: None,
    });
    window.app.save_page_section_editor();
    window.settle(&mut host);
    let gui_after = host.artifact(&gui);
    assert!(window.app.page_section_mutation.is_none(), "the section change finishes as its own mutation's change");
    assert_eq!(gui_after.revision, before.revision + 1, "the sections are one change record");
    window.app.history(false).expect("the undo starts");
    window.settle(&mut host);
    assert_eq!(content_of(&host.artifact(&gui)), content_of(&before), "one undo takes the sections back");
    drop(window);

    let backend = HostBackend::new(host);
    let mcp = Mcp::connect(backend.clone());
    let sections: Vec<CanvasPageSection> = serde_json::from_value(json!([section])).expect("a section");
    let operation = Operation::SetCanvasPageSections { expected: Vec::new(), sections };
    let mcp_after = mcp_change(&backend, &mcp, &mcp_canvas, &[Intent::Content], vec![operation]);
    assert_same_content(&before, &gui_after, &mcp_after);
}

/// While the crop button's crop is applied, the crop command refuses another crop of the same image: the guard of `apply_changes`
/// covers the command path too, and the first crop is the one that lands.
#[test]
fn the_crop_command_is_refused_while_the_crop_button_applies() {
    let mut host = Host::start();
    let dir = scratch_dir("crop-guard");
    let (sha, bytes) = import(&mut host, &png(&dir, "picture.png", [200, 40, 40, 255]));
    let gui = canvas_with_picture(&mut host, "그림판", &sha, bytes);

    let mut window = Window::open(&mut host, &gui);
    window.step(&mut host, "select.node", json!({ "node_id": PICTURE })).expect("the picture is selected");
    window.app.begin_canvas_image_crop(PICTURE).expect("the crop starts");
    window.settle(&mut host);
    let first = PixelRect { x: 1, y: 1, width: 2, height: 2 };
    // The button's crop is sent, and no answer has come yet: the window is applying it.
    window.app.apply_canvas_image_crop(Some(first)).expect("the button applies the crop");
    let second = json!({ "node_id": PICTURE, "expected_asset_sha256": sha.clone(), "x": 2, "y": 2, "width": 1, "height": 1 });
    assert!(window.app.invoke("canvas.image.crop", second).is_err(), "a second crop is refused while the first one is applied");
    window.settle(&mut host);
    assert_eq!(picture_crop(&host.artifact(&gui)), Some(first), "only the button's crop is applied");
    let _ = std::fs::remove_dir_all(&dir);
}

/// While the image button's replacement is applied, the replacement command is refused: a second replacement is not started.
#[test]
fn the_replacement_command_is_refused_while_the_button_replacement_applies() {
    let mut host = Host::start();
    let dir = scratch_dir("replace-guard");
    let first = png(&dir, "first.png", [200, 40, 40, 255]);
    let second = png(&dir, "second.png", [40, 200, 40, 255]);
    let (sha, bytes) = import(&mut host, &first);
    let (new_sha, _) = import(&mut host, &second);
    let gui = canvas_with_picture(&mut host, "그림판", &sha, bytes);

    let mut window = Window::open(&mut host, &gui);
    window.step(&mut host, "select.node", json!({ "node_id": PICTURE })).expect("the picture is selected");
    let intent = window.app.image_replacement_start().expect("the picture can start a replacement");
    window.app.start_canvas_image_replacement_import(intent, second.clone()).expect("the import starts");
    // The import's answer arrives: the button applies the replacement, and no answer to that change has come yet.
    let imported = host.call("asset.import", json!({ "path": second.to_string_lossy() })).expect("the picture imports");
    window.answer_queued("asset.import", imported);
    let message = window
        .app
        .invoke("canvas.image.replace", json!({ "node_id": PICTURE, "asset_sha256": new_sha.clone() }))
        .expect_err("a second replacement is refused while the button's one is applied");
    assert_eq!(message, "이미지 교체가 이미 진행 중입니다");
    window.settle(&mut host);
    assert_eq!(picture_sha(&host.artifact(&gui)), new_sha, "the button's replacement is the one that lands");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A WebCode project from the WebGL animation template: `animation.js` and `index.html`.
fn web_project(host: &mut Host, title: &str) -> String {
    let summary = host
        .call(
            "artifact.create",
            json!({ "request_id": new_id(), "actor": "desktop", "kind": "web_code", "title": title, "size": null, "template": "webgl_animation" }),
        )
        .expect("the WebGL template creates a WebCode project");
    serde_json::from_value::<ArtifactSummary>(summary).expect("a summary").id
}

/// The entry names of a ZIP archive, read from its central directory.
fn zip_entry_names(bytes: &[u8]) -> Vec<String> {
    /// The little-endian number of `width` bytes at `at`.
    fn number(bytes: &[u8], at: usize, width: usize) -> usize {
        bytes[at..at + width].iter().rev().fold(0, |value, byte| (value << 8) | usize::from(*byte))
    }
    let end = bytes.windows(4).rposition(|window| window == b"PK\x05\x06").expect("an end of central directory");
    let mut at = number(bytes, end + 16, 4);
    (0..number(bytes, end + 10, 2))
        .map(|_| {
            assert_eq!(&bytes[at..at + 4], b"PK\x01\x02", "a central directory header");
            let name_len = number(bytes, at + 28, 2);
            let name = String::from_utf8(bytes[at + 46..at + 46 + name_len].to_vec()).expect("a UTF-8 name");
            at += 46 + name_len + number(bytes, at + 30, 2) + number(bytes, at + 32, 2);
            name
        })
        .collect()
}

/// A name inside the project: relative, with no drive, no backslash and no empty, `.` or `..` segment.
fn project_relative(name: &str) -> bool {
    !name.contains(['\\', ':']) && name.split('/').all(|segment| !segment.is_empty() && segment != "." && segment != "..")
}

/// The WebCode ZIP export: the menu's ZIP, the command `file.export` and the tool `artifact_export` write the same file,
/// and its entries are the project's files under project-relative names.
#[test]
fn the_zip_export_gives_the_same_file_by_command_and_by_tool() {
    let mut host = Host::start();
    let dir = scratch_dir("zip");
    let gui = web_project(&mut host, "웹 작업물");
    let mcp_project = web_project(&mut host, "웹 작업물");
    assert!(export_formats(ArtifactKind::WebCode).iter().any(|(format, _, _)| *format == "zip"), "the WebCode menu offers ZIP");

    // The command, as the window runs it with the path the save dialog gave.
    let by_command = dir.join("command.zip");
    let mut window = Window::open(&mut host, &gui);
    window.step(&mut host, "file.export", json!({ "format": "zip", "target_path": by_command.to_string_lossy() })).expect("the export is sent");
    // The export menu's own path: the dialog's answer sets the path, then the dry run and the write follow.
    let by_menu = dir.join("menu.zip");
    window.app.export_format = "zip".into();
    window.app.export_path = by_menu.to_string_lossy().into_owned();
    window.app.export_active();
    window.settle(&mut host);
    drop(window);

    let backend = HostBackend::new(host);
    let mcp = Mcp::connect(backend.clone());
    let revision = backend.with(|host| host.artifact(&mcp_project).revision);
    let by_tool = dir.join("tool.zip");
    let request = ExportFileRequest {
        request_id: new_id(),
        dry_run: None,
        options: ExportRequest::new(mcp_project.as_str(), revision, "zip", Editability::NativeData),
        target_path: by_tool.to_string_lossy().into_owned(),
        overwrite: false,
        expected_target_hash: None,
    };
    let result = mcp.call("artifact_export", serde_json::to_value(request).expect("a request"));

    let tool = std::fs::read(&by_tool).expect("the tool wrote the file");
    let digest = designllm_contracts::sha256(&tool);
    assert_eq!(result["format"], "zip");
    assert_eq!(result["editability"], "native_data");
    assert_eq!(result["sha256"].as_str(), Some(digest.as_str()), "the result names the digest of the file");
    let names = zip_entry_names(&tool);
    assert_eq!(names, ["animation.js", "index.html"], "the project's files, by name");
    assert!(names.iter().all(|name| project_relative(name)), "every entry is a project-relative path: {names:?}");
    for path in [&by_command, &by_menu] {
        let bytes = std::fs::read(path).expect("the window wrote the file");
        assert_eq!(designllm_contracts::sha256(&bytes), digest, "the same digest");
        assert_eq!(bytes, tool, "the same body");
    }
    let _ = std::fs::remove_dir_all(&dir);
}
