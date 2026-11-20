//! Tests of the WebCode file rows, the open file and the mapping list with its value edit (task 44). The edits go through the
//! window as the user makes them: a click on the file row, a click on a mapping, typing into the value field and a click on
//! its button, in drawn frames. A value edit made so is compared with the same edit by the MCP tool on a second project
//! built the same way (BRIEF 17, the `39` helper in `test_support`).

use designllm_canvas::WebEngine;
use designllm_contracts::{sha256, ArtifactBody, EngineAdapter, Intent, Operation, WebFile};
use eframe::egui::{epaint::Shape, Event, Key, Modifiers, Pos2};
use serde_json::{json, Value};

use super::test_support::{change_set_arguments, content_of, Host, HostBackend, Mcp, Window};

/// The entry page of every test project. Its heading and its paragraph are mappings, and the paragraph's `title` is one too.
const PAGE: &str = "<!doctype html>\n<html><head><meta charset=\"utf-8\"><title>웹</title></head>\n<body><h1 data-dlm-map=\"heading\">첫 제목</h1><p data-dlm-map=\"note\" title=\"설명\">본문</p></body></html>\n";

/// A WebCode project whose entry file, `index.html`, holds `page`. Returns its artifact id.
fn web_page(host: &mut Host, page: &str) -> String {
    let id = host.create("web_code", "웹 화면");
    let (entry, expected_hash) = match &host.artifact(&id).body {
        ArtifactBody::WebCode(document) => (document.entry.clone(), document.files[&document.entry].sha256.clone()),
        _ => panic!("a WebCode project"),
    };
    assert_eq!(entry, "index.html", "the blank project's entry file");
    replace(host, &id, expected_hash, page);
    id
}

/// Writes `page` over `index.html` of `artifact`, as another client would.
fn replace(host: &mut Host, artifact: &str, expected_hash: String, page: &str) {
    let operation = Operation::ReplaceWebSource { path: "index.html".into(), expected_hash, content: page.into() };
    host.apply(artifact, vec![operation], [Intent::Content].into_iter().collect()).expect("the page is written");
}

/// The text of the file `path` of the WebCode project `artifact`.
fn file_text(host: &mut Host, artifact: &str, path: &str) -> String {
    match &host.artifact(artifact).body {
        ArtifactBody::WebCode(document) => document.files[path].content.clone(),
        _ => panic!("a WebCode project"),
    }
}

/// The texts the window's last frame drew, with their centres.
fn shown(window: &Window) -> Vec<(String, Pos2)> {
    fn walk(shape: &Shape, found: &mut Vec<(String, Pos2)>) {
        match shape {
            Shape::Text(text) => found.push((text.galley.text().to_owned(), text.pos + text.galley.size() / 2.0)),
            Shape::Vec(shapes) => shapes.iter().for_each(|shape| walk(shape, found)),
            _ => {}
        }
    }
    let mut found = Vec::new();
    for clipped in &window.shapes {
        walk(&clipped.shape, &mut found);
    }
    found
}

/// The centre of the one text `label` the last frame drew in the left sidebar (the layers tab).
fn at(window: &Window, label: &str) -> Pos2 {
    let all = shown(window);
    let matches: Vec<Pos2> = all.iter().filter(|(text, centre)| text == label && centre.x < 400.0).map(|(_, centre)| *centre).collect();
    match matches.as_slice() {
        [centre] => *centre,
        _ => panic!("{} sidebar texts {label:?}; drawn: {:?}", matches.len(), all.iter().map(|(text, _)| text).collect::<Vec<_>>()),
    }
}

/// A click on the sidebar text `label`, then a frame for what it changed.
fn click(window: &mut Window, host: &mut Host, label: &str) {
    let position = at(window, label);
    window.click(host, 1.0, position, false);
    window.frame(host, 1.0, Vec::new());
}

/// Typed text, into the field that has the focus, then a frame that draws it (a button takes a click only in a frame after
/// one that drew it enabled).
fn type_text(window: &mut Window, host: &mut Host, text: &str) {
    window.frame(host, 1.0, vec![Event::Text(text.into())]);
    window.frame(host, 1.0, Vec::new());
}

fn press_enter(window: &mut Window, host: &mut Host) {
    let enter = Event::Key { key: Key::Enter, physical_key: Some(Key::Enter), pressed: true, repeat: false, modifiers: Modifiers::default() };
    window.frame(host, 1.0, vec![enter]);
}

/// A window on `artifact` whose user has opened `index.html` with a click on its file row.
fn opened(host: &mut Host, artifact: &str) -> Window {
    let mut window = Window::open(host, artifact);
    window.draw(host, 1.0);
    click(&mut window, host, "index.html");
    window.draw(host, 1.0);
    window
}

/// The `mapped_web_edit` change sets the window sent, with the host's answer (null for a refusal).
fn mapped_edits(window: &Window) -> Vec<&(String, Value, Value)> {
    window.calls.iter().filter(|(method, params, _)| method == "changes.apply" && params["operations"][0]["op"] == "mapped_web_edit").collect()
}

#[test]
fn a_value_typed_in_the_window_is_one_mapped_edit_and_one_undo_step() {
    let mut host = Host::start();
    let web = web_page(&mut host, PAGE);
    let mut window = opened(&mut host, &web);
    let texts: Vec<String> = shown(&window).into_iter().map(|(text, _)| text).collect();
    assert!(texts.iter().any(|text| text == "<h1> heading") && texts.iter().any(|text| text == "<p> note"), "the mappings are listed: {texts:?}");
    assert_eq!(window.app.active_web_path.as_deref(), Some("index.html"), "the file is the open one");
    let choice = window.app.ui.user_selection.clone().expect("the open file is the user's choice");
    assert_eq!(choice.2, vec!["index.html".to_owned()], "an AI request names the open file");

    click(&mut window, &mut host, "<h1> heading");
    click(&mut window, &mut host, "새 값");
    type_text(&mut window, &mut host, "새 제목");
    click(&mut window, &mut host, "적용");
    let sent = mapped_edits(&window);
    assert_eq!(sent.len(), 1, "one MappedWebEdit goes out");
    assert_eq!(sent[0].1["operations"][0]["mapping_id"], "heading");
    let edited = file_text(&mut host, &web, "index.html");
    assert!(edited.contains("<h1 data-dlm-map=\"heading\">새 제목</h1>"), "the mapped text changed: {edited}");
    assert!(edited.contains("<p data-dlm-map=\"note\" title=\"설명\">본문</p>"), "nothing else changed: {edited}");

    window.step(&mut host, "edit.undo", json!({})).expect("the undo is sent");
    assert_eq!(file_text(&mut host, &web, "index.html"), PAGE, "one undo takes the edit back");
}

#[test]
fn a_value_typed_for_one_mapping_is_not_sent_for_the_mapping_picked_next() {
    let mut host = Host::start();
    let web = web_page(&mut host, PAGE);
    let mut window = opened(&mut host, &web);

    click(&mut window, &mut host, "<h1> heading");
    click(&mut window, &mut host, "새 값");
    type_text(&mut window, &mut host, "제목 값");
    click(&mut window, &mut host, "<p> note");
    click(&mut window, &mut host, "적용");
    assert!(mapped_edits(&window).is_empty(), "the other mapping's value is not sent");
    assert_eq!(file_text(&mut host, &web, "index.html"), PAGE, "nothing changed");

    click(&mut window, &mut host, "새 값");
    type_text(&mut window, &mut host, "단락 값");
    click(&mut window, &mut host, "적용");
    let edited = file_text(&mut host, &web, "index.html");
    assert!(edited.contains("<p data-dlm-map=\"note\" title=\"설명\">단락 값</p>"), "the picked mapping changed: {edited}");
    assert!(edited.contains("<h1 data-dlm-map=\"heading\">첫 제목</h1>"), "the first mapping kept its text: {edited}");
}

#[test]
fn an_empty_value_cannot_be_applied_by_the_button_or_by_enter() {
    let mut host = Host::start();
    let web = web_page(&mut host, PAGE);
    let mut window = opened(&mut host, &web);

    click(&mut window, &mut host, "<h1> heading");
    click(&mut window, &mut host, "적용");
    click(&mut window, &mut host, "새 값");
    press_enter(&mut window, &mut host);
    window.draw(&mut host, 1.0);
    assert!(mapped_edits(&window).is_empty(), "an empty value is not sent");
    assert_eq!(file_text(&mut host, &web, "index.html"), PAGE, "the heading keeps its text");
}

#[test]
fn an_edit_from_a_list_the_file_has_moved_past_is_refused_and_changes_nothing() {
    let mut host = Host::start();
    let web = web_page(&mut host, PAGE);
    let mut window = opened(&mut host, &web);
    click(&mut window, &mut host, "<h1> heading");
    click(&mut window, &mut host, "새 값");
    type_text(&mut window, &mut host, "늦은 제목");

    // Another client rewrites the file; the window still shows the list it read before.
    let outside = PAGE.replace("첫 제목", "바깥 제목");
    let hash = match &host.artifact(&web).body {
        ArtifactBody::WebCode(document) => document.files["index.html"].sha256.clone(),
        _ => panic!("a WebCode project"),
    };
    replace(&mut host, &web, hash, &outside);
    let before = host.artifact(&web);

    click(&mut window, &mut host, "적용");
    let sent = mapped_edits(&window);
    assert_eq!(sent.len(), 1, "the edit is sent once");
    assert_eq!(sent[0].2, Value::Null, "the host refuses the edit");
    assert_eq!(window.message().as_deref(), Some("다른 수정이 먼저 저장됐습니다. 최신 내용을 다시 불러왔습니다"), "the refusal is named");
    let after = host.artifact(&web);
    assert_eq!(after.revision, before.revision, "a refused edit moves no revision");
    assert_eq!(content_of(&after), content_of(&before), "a refused edit changes nothing");
}

#[test]
fn a_stale_hash_or_an_engine_refusal_shows_the_reason_and_changes_nothing() {
    let mut host = Host::start();
    let web = web_page(&mut host, PAGE);
    let mut window = opened(&mut host, &web);
    let before = host.artifact(&web);

    // The command's own hash check, as the MCP tool is given it.
    window
        .step(
            &mut host,
            "web.mapped_edit",
            json!({ "path": "index.html", "mapping_id": "heading", "property": "text", "value": "x", "expected_hash": "0".repeat(64) }),
        )
        .expect("the edit is sent");
    assert_eq!(window.message().as_deref(), Some("원본 파일이 바뀌었습니다. 다시 열어 주세요"), "the stale hash is named");
    assert_eq!(content_of(&host.artifact(&web)), content_of(&before), "a stale edit changes nothing");

    // An attribute the mapping does not have, typed in the window.
    window.draw(&mut host, 1.0);
    click(&mut window, &mut host, "<p> note");
    click(&mut window, &mut host, "속성");
    click(&mut window, &mut host, "속성 이름");
    type_text(&mut window, &mut host, "alt");
    click(&mut window, &mut host, "새 값");
    type_text(&mut window, &mut host, "대체 글");
    click(&mut window, &mut host, "적용");
    assert_eq!(mapped_edits(&window).len(), 2, "the window sent the edit");
    assert_eq!(window.message().as_deref(), Some("이 매핑에는 해당 속성이 없습니다"), "the engine's refusal is named");
    assert_eq!(host.artifact(&web).revision, before.revision, "a refused edit moves no revision");
    assert_eq!(content_of(&host.artifact(&web)), content_of(&before), "a refused edit changes nothing");
}

/// Gives the window's project a second file `about.html` holding `content`, selects it, and puts the engine's inspect of it
/// in the window's context, as a context read after such a file was added would. (No command adds a file to a project.)
fn select_second_file(window: &mut Window, content: &str) {
    let artifact = window.app.artifact.as_mut().expect("the project is loaded");
    let ArtifactBody::WebCode(document) = &mut artifact.body else { panic!("a WebCode project") };
    document.files.insert("about.html".into(), WebFile { content: content.into(), sha256: sha256(content.as_bytes()) });
    let inspected = WebEngine.inspect(artifact, &["about.html".to_owned()]).expect("the engine inspects the file");
    let context = window.app.context.as_mut().expect("the context");
    context.selection.node_ids = vec!["about.html".into()];
    context.selected_properties = json!({ "nodes": inspected });
}

fn drawn_texts(window: &Window) -> Vec<String> {
    shown(window).into_iter().map(|(text, _)| text).collect()
}

#[test]
fn a_file_other_than_the_entry_that_cannot_be_scanned_says_so_instead_of_listing_no_mapping() {
    let mut host = Host::start();
    let web = web_page(&mut host, PAGE);
    let mut window = opened(&mut host, &web);
    select_second_file(&mut window, "<p>About</p><!-- never closed");
    window.frame(&mut host, 1.0, Vec::new());
    let texts = drawn_texts(&window);
    assert!(texts.iter().any(|text| text == "HTML을 읽지 못해 매핑을 찾을 수 없습니다"), "the reason is shown: {texts:?}");
    assert!(!texts.iter().any(|text| text == "매핑된 요소가 없습니다"), "the file is not called empty: {texts:?}");

    select_second_file(&mut window, "<p>About</p>");
    window.frame(&mut host, 1.0, Vec::new());
    assert!(drawn_texts(&window).iter().any(|text| text == "매핑된 요소가 없습니다"), "a file read without mappings is called empty");
}

#[test]
fn a_value_typed_for_a_mapping_does_not_come_back_after_its_list_was_empty() {
    let mut host = Host::start();
    let web = web_page(&mut host, PAGE);
    let mut window = opened(&mut host, &web);
    click(&mut window, &mut host, "<h1> heading");
    click(&mut window, &mut host, "새 값");
    type_text(&mut window, &mut host, "옛 초안");
    assert!(drawn_texts(&window).iter().any(|text| text == "옛 초안"), "the value is in its field");

    // The list of the open file is empty for a while (the engine read the file without its mappings), then has them again.
    let context = window.app.context.as_mut().expect("the context");
    let listed = std::mem::replace(&mut context.selected_properties, json!({ "nodes": { "items": [{ "path": "index.html", "mappings": [] }] } }));
    window.frame(&mut host, 1.0, Vec::new());
    assert!(drawn_texts(&window).iter().any(|text| text == "매핑된 요소가 없습니다"));
    window.app.context.as_mut().expect("the context").selected_properties = listed;
    window.frame(&mut host, 1.0, Vec::new());
    let texts = drawn_texts(&window);
    assert!(texts.iter().any(|text| text == "<h1> heading"), "the mappings are listed again: {texts:?}");
    assert!(!texts.iter().any(|text| text == "옛 초안"), "the old value does not come back: {texts:?}");
}

#[test]
fn a_page_the_engine_cannot_scan_says_so_instead_of_listing_no_mapping() {
    let mut host = Host::start();
    let web = web_page(&mut host, "<!doctype html>\n<html><body><h1 data-dlm-map=\"heading\">제목</h1><script>never closed\n");
    let window = opened(&mut host, &web);
    let texts: Vec<String> = shown(&window).into_iter().map(|(text, _)| text).collect();
    assert!(texts.iter().any(|text| text == "HTML을 읽지 못해 매핑을 찾을 수 없습니다"), "the reason is shown: {texts:?}");
    assert!(!texts.iter().any(|text| text == "매핑된 요소가 없습니다"), "the page is not called empty: {texts:?}");
}

#[test]
fn the_value_edit_gives_the_same_project_by_the_window_and_by_the_tool() {
    let mut host = Host::start();
    let gui = web_page(&mut host, PAGE);
    let mcp_web = web_page(&mut host, PAGE);
    let before = host.artifact(&gui);

    let mut window = opened(&mut host, &gui);
    click(&mut window, &mut host, "<p> note");
    click(&mut window, &mut host, "속성");
    click(&mut window, &mut host, "속성 이름");
    type_text(&mut window, &mut host, "title");
    click(&mut window, &mut host, "새 값");
    type_text(&mut window, &mut host, "새 설명");
    click(&mut window, &mut host, "적용");
    assert_eq!(mapped_edits(&window).len(), 1, "one MappedWebEdit goes out");
    let gui_after = host.artifact(&gui);
    drop(window);

    let backend = HostBackend::new(host);
    let mcp = Mcp::connect(backend.clone());
    let (workspace, revision, expected_hash) = backend.with(|host| {
        let artifact = host.artifact(&mcp_web);
        let hash = match &artifact.body {
            ArtifactBody::WebCode(document) => document.files["index.html"].sha256.clone(),
            _ => panic!("a WebCode project"),
        };
        (host.workspace_id.clone(), artifact.revision, hash)
    });
    let operation = Operation::MappedWebEdit {
        path: "index.html".into(),
        expected_hash,
        mapping_id: "note".into(),
        property: "attr:title".into(),
        value: "새 설명".into(),
    };
    mcp.call("changes_apply", change_set_arguments(&workspace, &mcp_web, revision, &[Intent::Content], vec![operation]));
    let mcp_after = backend.with(|host| host.artifact(&mcp_web));

    assert_ne!(content_of(&before), content_of(&gui_after), "the window changed the file");
    assert_eq!(content_of(&gui_after), content_of(&mcp_after), "the window and the MCP tool left different content");
}

/// Regression (backlog desktop 49(2), 53(b)): a double click on a file row opened the rename field, and its commit always
/// came back refused (the web engine refuses SetNodeInfo, OPERATION_UNSUPPORTED). A file row offers no rename now; the file
/// stays the open one.
#[test]
fn a_double_click_on_a_file_row_opens_no_rename() {
    let mut host = Host::start();
    let web = web_page(&mut host, PAGE);
    let mut window = opened(&mut host, &web);
    let position = at(&window, "index.html");
    window.click(&mut host, 1.0, position, true);
    window.frame(&mut host, 1.0, Vec::new());

    assert!(window.app.layer_rename.is_none(), "no rename field opens on a file row");
    assert_eq!(window.app.active_web_path.as_deref(), Some("index.html"), "the file is still the open one");
    assert!(!window.calls.iter().any(|(method, params, _)| method == "changes.apply" && params["operations"][0]["op"] == "set_node_info"), "no rename is sent");
}

/// The mapped elements of `page` as the engine lists them for the file `path` of the window's project (window side only).
fn inspected_as(window: &mut Window, path: &str, page: &str) -> Value {
    let artifact = window.app.artifact.as_mut().expect("the project is loaded");
    let ArtifactBody::WebCode(document) = &mut artifact.body else { panic!("a WebCode project") };
    document.files.insert(path.into(), WebFile { content: page.into(), sha256: sha256(page.as_bytes()) });
    WebEngine.inspect(artifact, &[path.to_owned()]).expect("the engine inspects the file")
}

/// Shows the mappings of the file `path` holding `page` as the selected file (window side only).
fn select_file_holding(window: &mut Window, path: &str, page: &str) {
    let inspected = inspected_as(window, path, page);
    let context = window.app.context.as_mut().expect("the context");
    context.selection.node_ids = vec![path.into()];
    context.selected_properties = json!({ "nodes": inspected });
}

/// Backlog desktop 49 (5): a mapping id that another file also has. The value typed for `heading` of `index.html` is that
/// file's own: the same id in `about.html` shows no value and is not picked, and the value is there again back in `index.html`.
#[test]
fn the_same_mapping_id_in_another_file_keeps_its_own_value() {
    let mut host = Host::start();
    let web = web_page(&mut host, PAGE);
    let mut window = opened(&mut host, &web);
    click(&mut window, &mut host, "<h1> heading");
    click(&mut window, &mut host, "새 값");
    type_text(&mut window, &mut host, "첫 파일 값");
    let entry = window.app.context.as_ref().expect("the context").selected_properties.clone();

    select_file_holding(&mut window, "about.html", PAGE);
    window.frame(&mut host, 1.0, Vec::new());
    let texts = drawn_texts(&window);
    assert!(texts.iter().any(|text| text == "<h1> heading"), "the other file lists the same id: {texts:?}");
    assert!(!texts.iter().any(|text| text == "첫 파일 값"), "not the first file's value: {texts:?}");

    let context = window.app.context.as_mut().expect("the context");
    context.selection.node_ids = vec!["index.html".into()];
    context.selected_properties = entry;
    window.frame(&mut host, 1.0, Vec::new());
    assert!(drawn_texts(&window).iter().any(|text| text == "첫 파일 값"), "the first file's value is back");
}

/// Backlog desktop 49 (5): the list comes back in another order (the file was rewritten). The picked mapping is picked by its
/// id, so the value goes to `heading` still.
#[test]
fn a_reordered_list_keeps_the_picked_mapping_by_its_id() {
    let mut host = Host::start();
    let web = web_page(&mut host, PAGE);
    let mut window = opened(&mut host, &web);
    click(&mut window, &mut host, "<h1> heading");
    click(&mut window, &mut host, "새 값");
    type_text(&mut window, &mut host, "새 제목");
    let context = window.app.context.as_mut().expect("the context");
    let items = &mut context.selected_properties["nodes"]["items"][0]["mappings"];
    let mut reordered = items.as_array().expect("mappings").clone();
    reordered.reverse();
    assert_eq!(reordered[0]["id"], "note", "the list is in the other order now");
    *items = Value::Array(reordered);
    window.frame(&mut host, 1.0, Vec::new());
    click(&mut window, &mut host, "적용");
    let sent = mapped_edits(&window);
    assert_eq!(sent.len(), 1, "one MappedWebEdit goes out");
    assert_eq!(sent[0].1["operations"][0]["mapping_id"], "heading", "the picked id, not the one now in its place");
    assert!(file_text(&mut host, &web, "index.html").contains(">새 제목</h1>"), "the heading changed");
}

/// Backlog desktop 49 (5): `web.mapped_edit` without a path edits the open file: the entry file until another file is opened
/// (`active_web_path`), then that file.
#[test]
fn a_mapped_edit_without_a_path_goes_to_the_open_file() {
    let mut host = Host::start();
    let web = web_page(&mut host, PAGE);
    let mut window = Window::open(&mut host, &web);
    assert_eq!(window.app.active_web_path, None, "no file opened yet");
    let edit = json!({ "mapping_id": "heading", "property": "text", "value": "입구" });
    window.step(&mut host, "web.mapped_edit", edit.clone()).expect("the edit is sent");
    assert_eq!(mapped_edits(&window).last().expect("sent").1["operations"][0]["path"], "index.html", "the entry file");

    let _ = inspected_as(&mut window, "about.html", PAGE);
    window.step(&mut host, "web.open_file", json!({ "path": "about.html" })).expect("the file opens");
    assert_eq!(window.app.active_web_path.as_deref(), Some("about.html"));
    // The host has no such file and refuses it; what matters here is the file the window names.
    let _ = window.step(&mut host, "web.mapped_edit", edit);
    assert_eq!(mapped_edits(&window).last().expect("sent").1["operations"][0]["path"], "about.html", "the open file");
}
