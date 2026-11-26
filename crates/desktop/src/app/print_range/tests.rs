use std::path::PathBuf;

use designllm_contracts::{
    new_id, sha256, Artifact, ArtifactBody, ArtifactKind, AssetRef, AssetStore, DltError, DltResult, Editability, ErrorCode,
    ExportFileRequest, ExportRequest, FlowBlock, FlowDocument, Intent, Operation, Page, PageSetup, Paragraph, ParagraphFormat,
    Section, SlideDocument, Size, TextRun, EMU_PER_MM,
};
use eframe::egui::{epaint::Shape, CentralPanel, Event, FullOutput, Id, Modifiers, PointerButton, Pos2};
use serde_json::{json, Value};

use super::{parse_page_range, range_supported, PageRangeError};
use crate::{
    app::test_support::{context, drawn_text, screen, summary, supported, Harness, Host, HostBackend, Mcp, Window},
    theme,
};

#[test]
fn pages_and_spans_become_zero_based_indices_in_order() {
    assert_eq!(parse_page_range("1,3-5", 5), Ok(vec![0, 2, 3, 4]));
    assert_eq!(parse_page_range("3", 3), Ok(vec![2]));
    assert_eq!(parse_page_range("1-3", 3), Ok(vec![0, 1, 2]));
    assert_eq!(parse_page_range("2-2", 2), Ok(vec![1]), "a span of one page is that page");
    assert_eq!(parse_page_range("  2 , 4 - 4 ", 4), Ok(vec![1, 3]), "spaces around items and dashes are ignored");
    assert_eq!(parse_page_range("1,2,3", 3), Ok(vec![0, 1, 2]), "adjacent pages are fine");
}

#[test]
fn an_empty_or_blank_input_is_refused() {
    assert_eq!(parse_page_range("", 5), Err(PageRangeError::Empty));
    assert_eq!(parse_page_range("   ", 5), Err(PageRangeError::Empty));
}

#[test]
fn a_non_number_is_refused() {
    for text in ["a", "1.5", "+1", "1,b", "1 2", "３", "2쪽", "1~3"] {
        assert_eq!(parse_page_range(text, 5), Err(PageRangeError::NotNumber), "{text:?}");
    }
}

#[test]
fn an_empty_item_or_a_dangling_dash_is_refused() {
    assert_eq!(parse_page_range("1,,2", 5), Err(PageRangeError::Malformed), "an empty item between commas");
    assert_eq!(parse_page_range("1,2,", 5), Err(PageRangeError::Malformed), "a trailing comma");
    assert_eq!(parse_page_range(",1", 5), Err(PageRangeError::Malformed), "a leading comma");
    assert_eq!(parse_page_range("-3", 5), Err(PageRangeError::Malformed), "a span without a start");
    assert_eq!(parse_page_range("1-", 5), Err(PageRangeError::Malformed), "a span without an end");
    assert_eq!(parse_page_range("1-2-3", 5), Err(PageRangeError::Malformed), "two dashes");
}

#[test]
fn page_zero_is_refused() {
    assert_eq!(parse_page_range("0", 5), Err(PageRangeError::Zero));
    assert_eq!(parse_page_range("0-2", 5), Err(PageRangeError::Zero));
    assert_eq!(parse_page_range("1,0", 5), Err(PageRangeError::Zero));
}

#[test]
fn a_page_past_the_end_of_the_document_is_refused() {
    assert_eq!(parse_page_range("6", 5), Err(PageRangeError::OutOfRange { count: 5 }));
    assert_eq!(parse_page_range("4-6", 5), Err(PageRangeError::OutOfRange { count: 5 }));
    assert_eq!(parse_page_range("99999999999999999999999", 5), Err(PageRangeError::OutOfRange { count: 5 }), "a number past u64 is past the end too");
    assert_eq!(parse_page_range("1", 0), Err(PageRangeError::OutOfRange { count: 0 }));
}

#[test]
fn a_span_that_runs_backwards_is_refused() {
    assert_eq!(parse_page_range("3-1", 5), Err(PageRangeError::ReversedSpan { start: 3, end: 1 }));
    assert_eq!(parse_page_range("1,4-2", 5), Err(PageRangeError::ReversedSpan { start: 4, end: 2 }));
}

#[test]
fn a_page_named_twice_is_refused() {
    assert_eq!(parse_page_range("2,2", 5), Err(PageRangeError::Repeated { page: 2 }));
    assert_eq!(parse_page_range("1-3,2", 5), Err(PageRangeError::Repeated { page: 2 }), "a page inside an earlier span");
    assert_eq!(parse_page_range("3,2-4", 5), Err(PageRangeError::Repeated { page: 3 }), "a span that starts inside an earlier page");
    assert_eq!(parse_page_range("1,1-2", 5), Err(PageRangeError::Repeated { page: 1 }));
}

#[test]
fn an_item_before_the_one_preceding_it_is_refused() {
    assert_eq!(parse_page_range("5,3", 5), Err(PageRangeError::Unordered { page: 3 }));
    assert_eq!(parse_page_range("3,1", 5), Err(PageRangeError::Unordered { page: 1 }));
    assert_eq!(parse_page_range("1-3,5-7,4", 7), Err(PageRangeError::Unordered { page: 4 }), "a page between two spans, given last");
}

#[test]
fn the_refusal_text_is_korean_and_names_the_problem() {
    assert_eq!(PageRangeError::Empty.to_string(), "쪽 번호를 입력하세요");
    assert_eq!(PageRangeError::OutOfRange { count: 5 }.to_string(), "이 문서는 5쪽입니다");
    assert_eq!(PageRangeError::Repeated { page: 2 }.to_string(), "2쪽이 두 번 적혔습니다");
    assert_eq!(PageRangeError::ReversedSpan { start: 3, end: 1 }.to_string(), "3-1은 거꾸로 된 범위입니다");
}

#[test]
fn a_page_count_that_shrank_refuses_a_range_that_was_valid_before() {
    // The popover checks the text against the count it sees; the request is checked again before it is sent.
    assert_eq!(parse_page_range("1,3", 3), Ok(vec![0, 2]));
    assert_eq!(parse_page_range("1,3", 2), Err(PageRangeError::OutOfRange { count: 2 }));
}

#[test]
fn full_width_separators_and_typographic_dashes_read_as_the_ascii_ones() {
    // The escapes are a full-width comma, an en dash, a full-width hyphen-minus and an ideographic comma.
    assert_eq!(parse_page_range("1\u{FF0C}3\u{2013}5", 5), Ok(vec![0, 2, 3, 4]), "a full-width comma and an en dash");
    assert_eq!(parse_page_range("2\u{FF0D}4", 5), Ok(vec![1, 2, 3]), "a full-width hyphen-minus");
    assert_eq!(parse_page_range("1\u{3001}3", 3), Ok(vec![0, 2]), "an ideographic comma");
    assert_eq!(parse_page_range("2\u{301C}4", 5), Ok(vec![1, 2, 3]), "a wave dash");
    assert_eq!(parse_page_range("1\u{FF5E}2", 5), Ok(vec![0, 1]), "a full-width tilde");
}

#[test]
fn a_range_is_offered_only_where_the_engine_applies_the_page_selection() {
    // Read from the engines: Canvas pdf and html, Slides pdf (its pptx ignores page_indices), Word, HWPX and HWP pdf.
    assert!(range_supported(ArtifactKind::Canvas, "pdf") && range_supported(ArtifactKind::Canvas, "html"));
    assert!(range_supported(ArtifactKind::Slides, "pdf"));
    assert!(!range_supported(ArtifactKind::Slides, "pptx"), "the pptx of a deck writes every slide");
    for kind in [ArtifactKind::Docx, ArtifactKind::Hwpx, ArtifactKind::Hwp] {
        assert!(range_supported(kind, "pdf"));
        assert!(!range_supported(kind, "docx"), "the Word export writes the whole document");
    }
    assert!(!range_supported(ArtifactKind::WebCode, "html") && !range_supported(ArtifactKind::WebCode, "zip"));
    assert!(!range_supported(ArtifactKind::Figure, "pdf"), "a figure has one page");
    assert!(!range_supported(ArtifactKind::Canvas, "png") && !range_supported(ArtifactKind::Canvas, "svg"), "one file per page");
}

// ---- The window: the command, the request, the popover and the section selector ----

/// One section of a Word document, A4, with one paragraph.
fn section(id: &str) -> Section {
    let setup = PageSetup {
        size: Size::a4(),
        margin_left: 25 * EMU_PER_MM,
        margin_top: 25 * EMU_PER_MM,
        margin_right: 25 * EMU_PER_MM,
        margin_bottom: 25 * EMU_PER_MM,
    };
    let paragraph = FlowBlock::Paragraph(Paragraph {
        id: format!("{id}-p"),
        runs: vec![TextRun::new(format!("{id}-r"), id)],
        style_ref: None,
        format: ParagraphFormat::default(),
    });
    Section { id: id.into(), setup, blocks: vec![paragraph] }
}

/// A window on the document `a1` as `artifact` (of the kind `kind` names), which the core reports as `pages` pages. The engine
/// can set page setups.
fn window_on(artifact: Artifact, kind: &str, pages: u32) -> Harness {
    let mut harness = Harness::new();
    theme::install(&harness.ctx);
    let startup = harness.sent();
    harness.answer(&startup, "workspace.list", Ok(json!({ "workspace_id": "w1", "artifacts": [summary("a1", kind, 1)] })));
    harness.app.invoke_ui("nav.open_project", json!({ "artifact_id": "a1" }));
    let opening = harness.sent();
    let view_id = opening
        .iter()
        .find(|request| request.1 == "context.read")
        .and_then(|request| request.2["view_id"].as_str())
        .expect("the context read names the view")
        .to_owned();
    let mut read = context("a1", &view_id, 1);
    read["capabilities"]["page_setup"] = supported();
    harness.answer(&opening, "context.read", Ok(read));
    harness.answer(&opening, "artifact.inspect", Ok(serde_json::to_value(&artifact).expect("the document")));
    set_pages(&mut harness, pages);
    harness.sent();
    harness
}

/// The document `a1` at revision 1 has `pages` pages, as a render of that revision reports them.
fn set_pages(harness: &mut Harness, pages: u32) {
    harness.app.total_pages = pages;
    harness.app.page_count_read = Some(("a1".into(), 1, pages));
}

/// A window on the Word document `a1` of `sections`, which the core reports as `pages` pages.
fn document_window(sections: Vec<Section>, pages: u32) -> Harness {
    let mut artifact = Artifact::new(ArtifactKind::Docx, "문서", "engine", ArtifactBody::Flow(FlowDocument { sections, styles: Default::default() }));
    artifact.id = "a1".into();
    artifact.revision = 1;
    window_on(artifact, "docx", pages)
}

/// A deck of `pages` slides, the document `a1`.
fn deck(pages: usize) -> Artifact {
    let pages = (1..=pages)
        .map(|number| Page::new(format!("slide-{number}"), Size::slide()))
        .collect();
    let mut artifact = Artifact::new(ArtifactKind::Slides, "덱", "engine", ArtifactBody::Slides(SlideDocument { pages, styles: Default::default(), theme: None }));
    artifact.id = "a1".into();
    artifact.revision = 1;
    artifact
}

/// Draws the properties panel of the window once, with `events`, and returns what the frame drew.
fn draw_properties(harness: &mut Harness, events: Vec<Event>) -> FullOutput {
    let ctx = harness.ctx.clone();
    let palette = theme::palette(&ctx);
    let mut input = screen(1.0, 1000.0, 800.0);
    input.events = events;
    ctx.run(input, |ctx| {
        CentralPanel::default().show(ctx, |ui| harness.app.draw_properties(ui, &palette));
    })
}

/// Draws the export popover once, with no events, and returns what the frame drew.
fn draw_export_menu(harness: &mut Harness) -> FullOutput {
    let ctx = harness.ctx.clone();
    let palette = theme::palette(&ctx);
    ctx.run(screen(1.0, 1000.0, 800.0), |ctx| {
        CentralPanel::default().show(ctx, |ui| harness.app.export_menu(ui, &palette));
    })
}

/// The centre of the first text the frame drew that reads `expected`.
fn text_center(output: &FullOutput, expected: &str) -> Pos2 {
    fn find(shape: &Shape, expected: &str) -> Option<Pos2> {
        match shape {
            Shape::Text(text) if text.galley.text() == expected => Some(text.pos + text.galley.size() / 2.0),
            Shape::Vec(shapes) => shapes.iter().find_map(|shape| find(shape, expected)),
            _ => None,
        }
    }
    output.shapes.iter().find_map(|clipped| find(&clipped.shape, expected)).unwrap_or_else(|| panic!("the panel did not draw {expected:?}"))
}

/// A primary click on the text `expected`, found in a frame drawn first.
fn click(harness: &mut Harness, expected: &str) {
    let at = text_center(&draw_properties(harness, Vec::new()), expected);
    let press = |pressed| Event::PointerButton { pos: at, button: PointerButton::Primary, pressed, modifiers: Modifiers::NONE };
    draw_properties(harness, vec![Event::PointerMoved(at), press(true), press(false)]);
}

/// The requests of one method among those sent, with their parameters.
fn requests_of<'a>(sent: &'a [(u64, String, Value)], method: &str) -> Vec<&'a Value> {
    sent.iter().filter(|request| request.1 == method).map(|request| &request.2).collect()
}

#[test]
fn a_page_range_past_the_end_or_repeated_is_refused_by_the_command_before_anything_is_sent() {
    let mut harness = document_window(vec![section("s1")], 4);
    let error = harness
        .app
        .invoke("file.export", json!({ "format": "pdf", "scope": "range", "pages": "2,2", "target_path": "C:/out/a.pdf" }))
        .expect_err("a repeated page is refused");
    assert_eq!(error, PageRangeError::Repeated { page: 2 }.to_string());
    let error = harness
        .app
        .invoke("file.export", json!({ "format": "pdf", "scope": "range", "pages": "5", "target_path": "C:/out/a.pdf" }))
        .expect_err("a page past the end is refused");
    assert_eq!(error, PageRangeError::OutOfRange { count: 4 }.to_string());
    // Without a target the command is the window's own export: the range is checked before the save dialog opens.
    let error = harness.app.invoke("file.export", json!({ "format": "pdf", "scope": "range", "pages": "0" })).expect_err("refused before the dialog");
    assert_eq!(error, PageRangeError::Zero.to_string());
    let error = harness
        .app
        .invoke("file.export", json!({ "format": "pdf", "scope": "all", "pages": "1", "target_path": "C:/out/a.pdf" }))
        .expect_err("pages without the range scope is refused");
    assert_eq!(error, "pages는 scope가 range일 때만 쓸 수 있습니다");
    assert!(requests_of(&harness.sent(), "artifact.export").is_empty(), "no export request is sent");
}

#[test]
fn the_request_carries_the_named_pages_as_zero_based_indices_and_none_for_a_page_past_the_end() {
    let harness = document_window(vec![section("s1")], 4);
    let named = json!({ "format": "pdf", "scope": "range", "pages": "2,4" });
    let request = harness.app.export_request("C:/out/a.pdf".into(), Some(&named)).expect("pages 2 and 4 of four");
    assert_eq!(request.options.page_indices, Some(vec![1, 3]));
    assert!(harness.app.export_request("C:/out/a.pdf".into(), Some(&json!({ "format": "pdf", "scope": "range", "pages": "5" }))).is_none());
    assert!(harness.app.export_request("C:/out/a.pdf".into(), Some(&json!({ "format": "pdf", "scope": "range" }))).is_none(), "no pages, no request");
}

#[test]
fn a_document_that_lost_pages_after_the_range_was_chosen_is_not_sent() {
    let mut harness = document_window(vec![section("s1")], 4);
    let request = harness
        .app
        .export_request("C:/out/a.pdf".into(), Some(&json!({ "format": "pdf", "scope": "range", "pages": "2,4" })))
        .expect("pages 2 and 4 of four");
    harness.app.export_scope = "range".into();
    set_pages(&mut harness, 2);
    harness.sent();
    harness.app.send_export(request);
    assert!(requests_of(&harness.sent(), "artifact.export").is_empty(), "page 4 is gone, so nothing is sent");
    assert_eq!(
        harness.message().map(|(_, text)| text),
        Some(String::from("문서 쪽 수가 바뀌어 쪽 범위를 쓸 수 없습니다. 쪽 번호를 다시 적어 주세요"))
    );
}

/// The words for pages that are gone follow the request, not the scope the menu shows when the write is sent: a range keeps
/// the range words after the menu moved to one page, and one page keeps its words while the menu shows a range.
#[test]
fn the_words_for_pages_that_are_gone_follow_the_request_not_the_menu() {
    let mut harness = document_window(vec![section("s1")], 4);
    let range = harness
        .app
        .export_request("C:/out/a.pdf".into(), Some(&json!({ "format": "pdf", "scope": "range", "pages": "2,4" })))
        .expect("pages 2 and 4 of four");
    harness.app.page_index = 3;
    harness.app.view().page_index = 3;
    let page = harness.app.export_request("C:/out/a.pdf".into(), Some(&json!({ "format": "pdf", "scope": "page" }))).expect("the fourth page");
    set_pages(&mut harness, 2);
    harness.app.export_scope = "page".into();
    harness.app.send_export(range);
    assert_eq!(
        harness.message().map(|(_, text)| text),
        Some(String::from("문서 쪽 수가 바뀌어 쪽 범위를 쓸 수 없습니다. 쪽 번호를 다시 적어 주세요"))
    );
    harness.app.export_scope = "range".into();
    harness.app.send_export(page);
    assert_eq!(harness.message().map(|(_, text)| text), Some(String::from("내보낼 쪽이 없어졌습니다. 다시 내보내 주세요")));
    assert!(requests_of(&harness.sent(), "artifact.export").is_empty(), "nothing is sent");
}

#[test]
fn a_page_that_is_gone_is_refused_under_its_own_message() {
    let mut harness = document_window(vec![section("s1")], 4);
    harness.app.page_index = 3;
    harness.app.view().page_index = 3;
    let request = harness.app.export_request("C:/out/a.pdf".into(), Some(&json!({ "format": "pdf", "scope": "page" }))).expect("the fourth page");
    assert_eq!(request.options.page_indices, Some(vec![3]));
    set_pages(&mut harness, 2);
    harness.sent();
    harness.app.send_export(request);
    assert!(requests_of(&harness.sent(), "artifact.export").is_empty(), "the page is gone, so nothing is sent");
    assert_eq!(harness.message().map(|(_, text)| text), Some(String::from("내보낼 쪽이 없어졌습니다. 다시 내보내 주세요")));
}

#[test]
fn a_deck_offers_no_page_range_for_pptx_and_refuses_one_before_anything_is_sent() {
    let mut harness = window_on(deck(3), "slides", 3);
    harness.app.export_format = "pptx".into();
    harness.app.export_scope = "all".into();
    let shown = drawn_text(&draw_export_menu(&mut harness));
    // The pptx writes the whole deck: there is no scope to choose, neither one slide nor a page range (S29).
    assert_eq!(harness.app.export_scopes(), [("all", "모든 페이지")]);
    assert!(!shown.iter().any(|text| text == "이 페이지"), "one slide is not offered");
    assert!(!shown.iter().any(|text| text == "쪽 범위"), "the pptx writes the whole deck, so no page range is offered");
    harness.sent();
    // The window's own export and the command with a target both refuse the range before any request is made (S30: as a
    // deck's PPTX, which writes every slide).
    let refused = super::super::save_export::DECK_PPTX_SCOPE;
    let error = harness.app.invoke("file.export", json!({ "format": "pptx", "scope": "range", "pages": "2" })).expect_err("refused before the dialog");
    assert_eq!(error, refused);
    let error = harness
        .app
        .invoke("file.export", json!({ "format": "pptx", "scope": "range", "pages": "2", "target_path": "C:/out/deck.pptx" }))
        .expect_err("refused by the command");
    assert_eq!(error, refused);
    assert!(requests_of(&harness.sent(), "artifact.export").is_empty(), "no export request is sent");
    // The same deck as pdf applies the selection, so the range is offered.
    harness.app.export_format = "pdf".into();
    let shown = drawn_text(&draw_export_menu(&mut harness));
    assert!(shown.iter().any(|text| text == "쪽 범위"), "pdf applies the page selection, so the range is offered");
}

#[test]
fn the_popover_shows_the_refused_range_under_its_field_and_asks_for_the_losses_of_a_valid_one() {
    let mut harness = document_window(vec![section("s1")], 3);
    harness.app.export_format = "pdf".into();
    harness.app.export_scope = "range".into();
    let field = Id::new(("export-page-range", "a1"));
    for (text, message) in [
        ("0", PageRangeError::Zero.to_string()),
        ("2,9", PageRangeError::OutOfRange { count: 3 }.to_string()),
        ("3,1", PageRangeError::Unordered { page: 1 }.to_string()),
    ] {
        harness.ctx.data_mut(|data| data.insert_temp(field, text.to_owned()));
        let output = draw_export_menu(&mut harness);
        assert!(drawn_text(&output).contains(&message), "{text:?} shows {message:?}");
        assert!(requests_of(&harness.sent(), "artifact.export").is_empty(), "{text:?} asks the host for nothing");
    }
    harness.ctx.data_mut(|data| data.insert_temp(field, "2".to_owned()));
    let output = draw_export_menu(&mut harness);
    assert!(!drawn_text(&output).iter().any(|text| text.starts_with("쪽 번호") || text.starts_with("이 문서는")), "a valid range shows no error");
    let sent = harness.sent();
    let preview = requests_of(&sent, "artifact.export");
    assert_eq!(preview.len(), 1, "the losses of the named page are asked for once");
    assert_eq!(preview[0]["dry_run"], json!(true));
    assert_eq!(preview[0]["options"]["page_indices"], json!([1]));
}

#[test]
fn the_section_picked_in_the_page_setup_gets_the_change_with_its_own_section_id() {
    let mut harness = document_window(vec![section("s1"), section("s2")], 2);
    // `쪽`, then the section row `구역` picks section 2, then Letter.
    click(&mut harness, "쪽");
    click(&mut harness, "구역 1");
    click(&mut harness, "구역 2");
    click(&mut harness, "Letter");
    let sent = harness.sent();
    let changes = requests_of(&sent, "changes.apply");
    assert_eq!(changes.len(), 1, "one change set for the paper");
    let operations = changes[0]["operations"].as_array().expect("the operations");
    assert_eq!(operations.len(), 1);
    assert_eq!(operations[0]["op"], json!("set_page_setup"));
    assert_eq!(operations[0]["section_id"], json!("s2"), "the change names the section picked, not the first");
}

// ---- The host: the same range from the command, the menu and the tool ----

/// A Word document with `extra` pages after its first one: each added paragraph starts a page.
fn paged_document(host: &mut Host, extra: usize) -> String {
    let document = host.create("docx", "보고서");
    let (section, end) = match &host.artifact(&document).body {
        ArtifactBody::Flow(flow) => (flow.sections[0].id.clone(), flow.sections[0].blocks.len() as u32),
        _ => panic!("a Word document"),
    };
    let operations = (0..extra)
        .map(|offset| Operation::InsertFlowBlock {
            section_id: section.clone(),
            index: end + offset as u32,
            block: FlowBlock::Paragraph(Paragraph {
                id: new_id(),
                runs: vec![TextRun::new(new_id(), format!("PAGE{}", offset + 2))],
                style_ref: None,
                format: ParagraphFormat { page_break_before: true, ..ParagraphFormat::default() },
            }),
        })
        .collect();
    host.apply(&document, operations, [Intent::Content].into_iter().collect()).expect("the pages");
    document
}

/// The one asset of the PDF reader: the bytes of a PDF under their own digest.
struct PdfAsset(AssetRef, Vec<u8>);

impl AssetStore for PdfAsset {
    fn put(&self, _bytes: &[u8], _mime: &str) -> DltResult<AssetRef> {
        Err(DltError::unsupported("the PDF is only read"))
    }
    fn read(&self, asset: &AssetRef) -> DltResult<Vec<u8>> {
        if asset.sha256 == self.0.sha256 {
            Ok(self.1.clone())
        } else {
            Err(DltError::new(ErrorCode::NotFound, asset.sha256.clone()))
        }
    }
    fn lookup(&self, hash: &str) -> DltResult<AssetRef> {
        if hash == self.0.sha256 {
            Ok(self.0.clone())
        } else {
            Err(DltError::new(ErrorCode::NotFound, hash.to_owned()))
        }
    }
}

/// The text of each page of a PDF, in order, read by the PDF reader of the sources (a page without text has an empty text).
fn pdf_page_texts(bytes: &[u8]) -> Vec<String> {
    let asset = AssetRef { sha256: sha256(bytes), mime: "application/pdf".into(), bytes: bytes.len() as u64 };
    let store = PdfAsset(asset.clone(), bytes.to_vec());
    let read = designllm_sources::extract_pdf_scope(&asset, &[], &store).expect("the PDF reads");
    (0..read.page_count)
        .map(|page| read.evidence.iter().filter(|item| item.physical_page_index == Some(page)).map(|item| item.text.as_str()).collect())
        .collect()
}

/// The pages of a PDF: its page objects, not the page tree.
fn pdf_pages(bytes: &[u8]) -> usize {
    let count = |needle: &[u8]| bytes.windows(needle.len()).filter(|window| *window == needle).count();
    let pages = count(b"/Type /Page");
    assert!(pages > count(b"/Type /Pages"), "the PDF has page objects");
    pages - count(b"/Type /Pages")
}

/// A scratch folder unique to this test (below the temp folder, BRIEF 11).
fn scratch_dir(test: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("designllm-desktop-print-range-{test}-{}", new_id()));
    std::fs::create_dir_all(&dir).expect("a scratch folder");
    dir
}

#[test]
fn a_named_range_of_a_word_document_gives_the_same_pdf_by_command_by_menu_and_by_tool() {
    let mut host = Host::start();
    let dir = scratch_dir("pdf");
    let gui = paged_document(&mut host, 4);
    let mcp_document = paged_document(&mut host, 4);

    // The command with a target (the agent's path), and the popover's own export (dry run first, then the write).
    let by_command = dir.join("command.pdf");
    let by_menu = dir.join("menu.pdf");
    let mut window = Window::open(&mut host, &gui);
    assert_eq!(window.app.total_pages, 5, "the blank first page and four more");
    window
        .step(&mut host, "file.export", json!({ "format": "pdf", "scope": "range", "pages": "2,4", "target_path": by_command.to_string_lossy() }))
        .expect("the export is sent");
    window.app.export_format = "pdf".into();
    window.app.export_scope = "range".into();
    window.app.export_path = by_menu.to_string_lossy().into_owned();
    let options = window.app.popover_export_args("2,4");
    window.app.export_active_with(Some(&options)).expect("the export is sent");
    window.settle(&mut host);
    drop(window);

    // The tool with the same two pages (0-based), on a document built the same way.
    let backend = HostBackend::new(host);
    let mcp = Mcp::connect(backend.clone());
    let revision = backend.with(|host| host.artifact(&mcp_document).revision);
    let by_tool = dir.join("tool.pdf");
    let mut options = ExportRequest::new(mcp_document.as_str(), revision, "pdf", Editability::Vector);
    options.page_indices = Some(vec![1, 3]);
    let request = ExportFileRequest {
        request_id: new_id(),
        dry_run: None,
        options,
        target_path: by_tool.to_string_lossy().into_owned(),
        overwrite: false,
        expected_target_hash: None,
    };
    let result = mcp.call("artifact_export", serde_json::to_value(request).expect("a request"));

    let tool = std::fs::read(&by_tool).expect("the tool wrote the file");
    assert_eq!(result["sha256"].as_str(), Some(designllm_contracts::sha256(&tool).as_str()), "the result names the digest of the file");
    assert_eq!(pdf_pages(&tool), 2, "only the two named pages");
    // Which pages, not only how many: the first output page is document page 2, the second is document page 4.
    let texts = pdf_page_texts(&tool);
    assert_eq!(texts.len(), 2, "two pages read back");
    assert!(texts[0].contains("PAGE2") && !texts[0].contains("PAGE3"), "the first output page is page 2: {:?}", texts[0]);
    assert!(texts[1].contains("PAGE4") && !texts[1].contains("PAGE5"), "the second output page is page 4: {:?}", texts[1]);
    for path in [&by_command, &by_menu] {
        let bytes = std::fs::read(path).expect("the window wrote the file");
        assert_eq!(designllm_contracts::sha256(&bytes), designllm_contracts::sha256(&tool), "the same digest");
        assert_eq!(bytes, tool, "the same body");
    }

    // The dry run of the same request reports the size of the file it would write, and no losses.
    let dry = backend.with(|host| {
        let mut options = ExportRequest::new(mcp_document.as_str(), revision, "pdf", Editability::Vector);
        options.page_indices = Some(vec![1, 3]);
        let request = ExportFileRequest { request_id: new_id(), dry_run: Some(true), options, target_path: String::new(), overwrite: false, expected_target_hash: None };
        host.call("artifact.export", serde_json::to_value(request).expect("a request")).expect("the dry run")
    });
    assert_eq!(dry["bytes"].as_u64(), Some(tool.len() as u64));
    assert_eq!(dry["losses"], json!([]));

    // Every page, for comparison: five.
    let by_tool_all = dir.join("all.pdf");
    let mut options = ExportRequest::new(mcp_document.as_str(), revision, "pdf", Editability::Vector);
    options.page_indices = None;
    let request = ExportFileRequest { request_id: new_id(), dry_run: None, options, target_path: by_tool_all.to_string_lossy().into_owned(), overwrite: false, expected_target_hash: None };
    mcp.call("artifact_export", serde_json::to_value(request).expect("a request"));
    let whole = std::fs::read(&by_tool_all).expect("the whole document");
    assert_eq!(pdf_pages(&whole), 5);
    let whole_texts = pdf_page_texts(&whole);
    assert_eq!(whole_texts.len(), 5);
    assert!(whole_texts[0].is_empty() && whole_texts[1].contains("PAGE2") && whole_texts[4].contains("PAGE5"), "{whole_texts:?}");
    let _ = std::fs::remove_dir_all(&dir);
}
