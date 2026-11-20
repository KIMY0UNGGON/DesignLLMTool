//! Work limits of one page and of one request, and the cache of loaded fonts and form streams. Each test builds a small document in
//! memory and gives the page small limits, so a limit trips after a few steps, not after the production count.

use super::*;

const FONT: &str = "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>";

/// Objects of a PDF under construction. Object n is `objects[n - 1]`.
struct Doc {
    objects: Vec<Vec<u8>>,
}

impl Doc {
    fn new() -> Self {
        Doc { objects: Vec::new() }
    }

    fn reserve(&mut self) -> u32 {
        self.objects.push(Vec::new());
        self.objects.len() as u32
    }

    fn set(&mut self, id: u32, body: &str) {
        self.objects[(id - 1) as usize] = body.as_bytes().to_vec();
    }

    fn add(&mut self, body: &str) -> u32 {
        let id = self.reserve();
        self.set(id, body);
        id
    }

    fn stream(&mut self, id: u32, dict: &str, data: &[u8]) {
        let mut body = format!("<< {dict} /Length {} >>\nstream\n", data.len()).into_bytes();
        body.extend_from_slice(data);
        body.extend_from_slice(b"\nendstream");
        self.objects[(id - 1) as usize] = body;
    }

    /// The bytes of the file with `root` as its catalog, with a cross-reference table that has the exact offsets.
    fn finish(&self, root: u32) -> Vec<u8> {
        let mut out = b"%PDF-1.5\n".to_vec();
        let mut offsets = Vec::new();
        for (i, body) in self.objects.iter().enumerate() {
            offsets.push(out.len());
            out.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
            out.extend_from_slice(body);
            out.extend_from_slice(b"\nendobj\n");
        }
        let xref = out.len();
        out.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", self.objects.len() + 1).as_bytes());
        for offset in offsets {
            out.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
        }
        out.extend_from_slice(
            format!("trailer\n<< /Size {} /Root {root} 0 R >>\nstartxref\n{xref}\n%%EOF\n", self.objects.len() + 1).as_bytes(),
        );
        out
    }
}

/// A one-page document. `resources` is the body of the page's /Resources dictionary, and `content` is the page content.
/// Returns the parsed document and the page's object id.
fn one_page(doc: &mut Doc, resources: &str, content: &[u8]) -> (Document, ObjectId) {
    let pages = doc.reserve();
    let contents = doc.reserve();
    doc.stream(contents, "", content);
    let page = doc.add(&format!("<< /Type /Page /Parent {pages} 0 R /MediaBox [0 0 612 792] /Resources {resources} /Contents {contents} 0 R >>"));
    doc.set(pages, &format!("<< /Type /Pages /Kids [{page} 0 R] /Count 1 >>"));
    let catalog = doc.add(&format!("<< /Type /Catalog /Pages {pages} 0 R >>"));
    let loaded = Document::load_mem(&doc.finish(catalog)).expect("the synthetic document parses");
    let page_id = *loaded.get_pages().values().next().expect("one page");
    (loaded, page_id)
}

/// A document of pages that share one page tree. Each page is given as its /Resources body and its content. Returns the parsed
/// document and the page ids in page order.
fn pages(doc: &mut Doc, specs: &[(String, Vec<u8>)]) -> (Document, Vec<ObjectId>) {
    let root = doc.reserve();
    let mut kids = Vec::new();
    for (resources, content) in specs {
        let contents = doc.reserve();
        doc.stream(contents, "", content);
        kids.push(doc.add(&format!(
            "<< /Type /Page /Parent {root} 0 R /MediaBox [0 0 612 792] /Resources {resources} /Contents {contents} 0 R >>"
        )));
    }
    let list: Vec<String> = kids.iter().map(|k| format!("{k} 0 R")).collect();
    doc.set(root, &format!("<< /Type /Pages /Kids [{}] /Count {} >>", list.join(" "), kids.len()));
    let catalog = doc.add(&format!("<< /Type /Catalog /Pages {root} 0 R >>"));
    let loaded = Document::load_mem(&doc.finish(catalog)).expect("the synthetic document parses");
    let ids = loaded.get_pages().values().copied().collect();
    (loaded, ids)
}

/// Interprets a page under the given page limits, in a request of its own.
fn read(loaded: &Document, page_id: ObjectId, budget: Budget) -> PResult<Interpreted> {
    interpret_with_budget(loaded, page_id, budget, &Request::new())
}

/// The limits of production, with one number changed.
fn with_form_runs(form_runs: u64) -> Budget {
    Budget { form_runs, ..PAGE_BUDGET }
}

/// A chain of `levels` forms in which form k runs form k+1 `factor` times, and the last form is empty. The page runs form 1 once.
/// Form k runs at depth k. The number of form runs is 1 + factor + factor^2 + ... + factor^(levels - 1).
fn fan_out(factor: usize, levels: usize) -> (Document, ObjectId) {
    let mut doc = Doc::new();
    let forms: Vec<u32> = (0..levels).map(|_| doc.reserve()).collect();
    for k in 0..levels {
        let (dict, data) = if k + 1 < levels {
            let calls = format!("/F{} Do ", k + 2).repeat(factor);
            (format!("/Type /XObject /Subtype /Form /BBox [0 0 612 792] /Resources << /XObject << /F{} {} 0 R >> >>", k + 2, forms[k + 1]), calls)
        } else {
            ("/Type /XObject /Subtype /Form /BBox [0 0 612 792] /Resources << >>".to_string(), "q Q".to_string())
        };
        doc.stream(forms[k], &dict, data.as_bytes());
    }
    one_page(&mut doc, &format!("<< /XObject << /F1 {} 0 R >> >>", forms[0]), b"/F1 Do")
}

#[test]
fn a_form_run_budget_stops_a_fan_out_at_its_limit() {
    // 3^0 + ... + 3^5 = 364 form runs in all. A limit of 50 stops the page before the tree is walked.
    let (loaded, page) = fan_out(3, 6);
    let code = read(&loaded, page, with_form_runs(50)).err().expect("the budget trips").code();
    assert_eq!(code, "PDF_FORM_BUDGET");
    assert!(read(&loaded, page, with_form_runs(1_000)).is_ok(), "the same page is read within a larger budget");
}

#[test]
fn a_form_that_calls_itself_is_refused_by_the_depth_limit_before_the_budget() {
    let mut doc = Doc::new();
    let form = doc.reserve();
    doc.stream(form, &format!("/Type /XObject /Subtype /Form /BBox [0 0 612 792] /Resources << /XObject << /Self {form} 0 R >> >>"), b"/Self Do");
    let (loaded, page) = one_page(&mut doc, &format!("<< /XObject << /Self {form} 0 R >> >>"), b"/Self Do");
    assert_eq!(read(&loaded, page, PAGE_BUDGET).err().expect("the depth limit trips").code(), "PDF_FORM_DEPTH");
}

#[test]
fn an_operator_budget_stops_a_page_with_too_many_operators() {
    let mut doc = Doc::new();
    let content = "q Q ".repeat(1_000);
    let (loaded, page) = one_page(&mut doc, "<< >>", content.as_bytes());
    let tight = Budget { work: 100, ..PAGE_BUDGET };
    assert_eq!(read(&loaded, page, tight).err().expect("the budget trips").code(), "PDF_CONTENT_BUDGET");
    assert!(read(&loaded, page, PAGE_BUDGET).is_ok());
}

#[test]
fn a_glyph_budget_stops_a_page_with_too_much_text() {
    let mut doc = Doc::new();
    let font = doc.add(FONT);
    let text = "abcdefghijklmnopqrstuvwxyzabcdefghijklmnop";
    let (loaded, page) = one_page(&mut doc, &format!("<< /Font << /F1 {font} 0 R >> >>"), format!("BT /F1 10 Tf ({text}) Tj ET").as_bytes());
    let tight = Budget { glyphs: 10, ..PAGE_BUDGET };
    assert_eq!(read(&loaded, page, tight).err().expect("the budget trips").code(), "PDF_CONTENT_BUDGET");
    let interpreted = read(&loaded, page, PAGE_BUDGET).expect("the page reads within the production budget");
    assert_eq!(interpreted.glyphs.len(), text.len());
}

#[test]
fn a_form_that_runs_many_times_is_decoded_once() {
    let mut doc = Doc::new();
    let form = doc.reserve();
    doc.stream(form, "/Type /XObject /Subtype /Form /BBox [0 0 612 792] /Resources << >>", b"q Q");
    let calls = "/F1 Do ".repeat(100);
    let (loaded, page) = one_page(&mut doc, &format!("<< /XObject << /F1 {form} 0 R >> >>"), calls.as_bytes());
    let interpreted = read(&loaded, page, PAGE_BUDGET).expect("the page reads");
    assert_eq!(interpreted.decodes, 1, "the form stream is decoded once and reused for every run");
}

#[test]
fn a_form_reached_through_two_names_that_hold_references_is_decoded_once() {
    // H5: the cache was keyed by the reference that each name held, so two names of one form decoded it twice. Here the second
    // name is a chain of two references, and both are followed to the same stream.
    let mut doc = Doc::new();
    let form = doc.reserve();
    doc.stream(form, "/Type /XObject /Subtype /Form /BBox [0 0 612 792] /Resources << >>", b"q Q");
    let alias_a = doc.add(&format!("{form} 0 R"));
    let alias_b = doc.add(&format!("{alias_a} 0 R"));
    let resources = format!("<< /XObject << /A {alias_a} 0 R /B {alias_b} 0 R >> >>");
    let (loaded, page) = one_page(&mut doc, &resources, b"/A Do /B Do /A Do");
    let interpreted = read(&loaded, page, PAGE_BUDGET).expect("the page reads");
    assert_eq!(interpreted.decodes, 1);
}

#[test]
fn a_content_stream_listed_twice_in_contents_is_decoded_and_counted_twice() {
    // H1: each /Contents entry used to be decoded under its own stream limit, so a repeated stream multiplied the page's bytes.
    let mut doc = Doc::new();
    let content = "q Q ".repeat(100);
    let stream = doc.reserve();
    doc.stream(stream, "", content.as_bytes());
    let root = doc.reserve();
    let page = doc.add(&format!(
        "<< /Type /Page /Parent {root} 0 R /MediaBox [0 0 612 792] /Resources << >> /Contents [{stream} 0 R {stream} 0 R] >>"
    ));
    doc.set(root, &format!("<< /Type /Pages /Kids [{page} 0 R] /Count 1 >>"));
    let catalog = doc.add(&format!("<< /Type /Catalog /Pages {root} 0 R >>"));
    let loaded = Document::load_mem(&doc.finish(catalog)).expect("parses");
    let page_id = *loaded.get_pages().values().next().expect("one page");
    // The stream is 400 bytes. Two decodes need 800, so a budget of 600 refuses the second one.
    assert_eq!(read(&loaded, page_id, Budget { decoded: 600, ..PAGE_BUDGET }).err().expect("refused").code(), "PDF_DECODE_BUDGET");
    assert!(read(&loaded, page_id, Budget { decoded: 900, ..PAGE_BUDGET }).is_ok());
}

#[test]
fn a_form_run_many_times_is_decoded_once_so_its_bytes_are_counted_once() {
    // The page is 20 bytes and the form 1000. Decoding the form for each of the three runs would need 3020 bytes.
    let mut doc = Doc::new();
    let form = doc.reserve();
    let body = format!("{}q Q", " ".repeat(1000 - 3));
    doc.stream(form, "/Type /XObject /Subtype /Form /BBox [0 0 612 792] /Resources << >>", body.as_bytes());
    let (loaded, page) = one_page(&mut doc, &format!("<< /XObject << /F1 {form} 0 R >> >>"), b"/F1 Do /F1 Do /F1 Do");
    assert!(read(&loaded, page, Budget { decoded: 1030, ..PAGE_BUDGET }).is_ok());
    assert_eq!(read(&loaded, page, Budget { decoded: 1000, ..PAGE_BUDGET }).err().expect("refused").code(), "PDF_DECODE_BUDGET");
}

#[test]
fn each_tj_item_is_one_unit_of_work() {
    // H6: the items of a TJ array are shown one by one, so each one is counted.
    let mut doc = Doc::new();
    let font = doc.add(FONT);
    let items: String = "(a) 1 ".repeat(50);
    let content = format!("BT /F1 10 Tf [{items}] TJ ET");
    let (loaded, page) = one_page(&mut doc, &format!("<< /Font << /F1 {font} 0 R >> >>"), content.as_bytes());
    assert_eq!(read(&loaded, page, Budget { work: 40, ..PAGE_BUDGET }).err().expect("refused").code(), "PDF_CONTENT_BUDGET");
    assert!(read(&loaded, page, PAGE_BUDGET).is_ok());
}

#[test]
fn a_content_stream_over_the_token_limit_is_refused_before_it_is_parsed() {
    // H2: the tokens are counted first. The operators are never built, so the operator limit is not what refuses it here.
    let mut doc = Doc::new();
    // Balanced pairs keep the graphics-state depth at one, so only the token count is in question here.
    let content = "q Q ".repeat(2_000);
    let (loaded, page) = one_page(&mut doc, "<< >>", content.as_bytes());
    let tight = Budget { tokens: 1_000, work: u64::MAX, ..PAGE_BUDGET };
    assert_eq!(read(&loaded, page, tight).err().expect("refused").code(), "PDF_CONTENT_BUDGET");
    assert!(read(&loaded, page, PAGE_BUDGET).is_ok());
}

#[test]
fn tokens_count_each_object_and_operator_and_a_word_that_lopdf_could_split_per_byte() {
    assert_eq!(count_tokens(b"1 0 0 1 10 10 cm"), 7);
    assert_eq!(count_tokens(b"[(a) -3 (b)] TJ"), 5);
    assert_eq!(count_tokens(b"<< /A 1 >> BDC"), 4);
    assert_eq!(count_tokens(b"% 1 2 3\n q"), 1);
    assert_eq!(count_tokens(b"true false null"), 3);
    // A word that lopdf could split into several objects counts each of its bytes, so such a run is never cheap.
    assert_eq!(count_tokens(b"1a1a1a1a"), 8);
    assert_eq!(count_tokens(b"trueQ"), 5);
}

#[test]
fn the_pages_of_one_request_share_one_work_budget() {
    // H-M1: each page is within its own limit. Together they are not, so the second page fails under the request's limit.
    let mut doc = Doc::new();
    let content = "q Q ".repeat(30);
    let (loaded, ids) = pages(&mut doc, &[("<< >>".to_string(), content.clone().into_bytes()), ("<< >>".to_string(), content.into_bytes())]);
    let request = Request::with_budget(Budget { work: 100, ..REQUEST_BUDGET });
    assert!(interpret_with_budget(&loaded, ids[0], PAGE_BUDGET, &request).is_ok());
    let error = interpret_with_budget(&loaded, ids[1], PAGE_BUDGET, &request).err().expect("the request limit trips");
    assert_eq!(error.code(), "PDF_REQUEST_BUDGET");
    let fresh = Request::with_budget(Budget { work: 100, ..REQUEST_BUDGET });
    assert!(interpret_with_budget(&loaded, ids[1], PAGE_BUDGET, &fresh).is_ok());
}

#[test]
fn a_font_object_is_loaded_once_for_the_whole_request() {
    let mut doc = Doc::new();
    let font = doc.add(FONT);
    let resources = format!("<< /Font << /F1 {font} 0 R >> >>");
    let (loaded, ids) = pages(
        &mut doc,
        &[
            (resources.clone(), b"BT /F1 10 Tf 1 0 0 1 20 700 Tm (A) Tj ET".to_vec()),
            (resources, b"BT /F1 10 Tf 1 0 0 1 20 700 Tm (B) Tj ET".to_vec()),
        ],
    );
    let request = Request::new();
    interpret_with_budget(&loaded, ids[0], PAGE_BUDGET, &request).expect("page 1 reads");
    assert_eq!(request.loaded_fonts(), 1);
    interpret_with_budget(&loaded, ids[1], PAGE_BUDGET, &request).expect("page 2 reads");
    assert_eq!(request.loaded_fonts(), 1, "the second page uses the font that the first page loaded");
}

#[test]
fn missing_xobjects_are_counted_and_their_names_are_not_kept() {
    let mut doc = Doc::new();
    let (loaded, page) = one_page(&mut doc, "<< >>", b"/Secret1 Do /Secret2 Do /Secret1 Do");
    let interpreted = read(&loaded, page, PAGE_BUDGET).expect("the page reads");
    assert_eq!(interpreted.warnings, vec![("PDF_XOBJECT_MISSING", 3)]);
}

#[test]
fn a_request_past_its_deadline_fails_at_its_next_charge() {
    // The deadline is checked by every charge, so a request whose time has run out fails the first step it takes.
    let mut doc = Doc::new();
    let (loaded, page) = one_page(&mut doc, "<< >>", b"q Q");
    let request = Request::with_limits(REQUEST_BUDGET, std::time::Instant::now());
    let error = interpret_with_budget(&loaded, page, PAGE_BUDGET, &request).err().expect("the deadline has passed");
    assert_eq!(error.code(), "PDF_REQUEST_BUDGET");
    assert!(interpret_with_budget(&loaded, page, PAGE_BUDGET, &Request::new()).is_ok());
}

#[test]
fn a_form_font_is_the_one_its_own_resources_name_not_the_page_font_of_the_same_name() {
    // The page shows F1 first, so F1 resolves to the page's font, which maps code 0x41 to X. The form names its own F1, which
    // maps 0x41 to Y. The form's glyph must come from its own font.
    let mut doc = Doc::new();
    let cmap_x = doc.reserve();
    doc.stream(cmap_x, "", b"1 begincodespacerange <00> <FF> endcodespacerange 1 beginbfchar <41> <0058> endbfchar");
    let cmap_y = doc.reserve();
    doc.stream(cmap_y, "", b"1 begincodespacerange <00> <FF> endcodespacerange 1 beginbfchar <41> <0059> endbfchar");
    let font_x = doc.add(&format!("<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding /ToUnicode {cmap_x} 0 R >>"));
    let font_y = doc.add(&format!("<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding /ToUnicode {cmap_y} 0 R >>"));
    let form = doc.reserve();
    doc.stream(form, &format!("/Type /XObject /Subtype /Form /BBox [0 0 612 792] /Resources << /Font << /F1 {font_y} 0 R >> >>"), b"BT /F1 10 Tf (A) Tj ET");
    let resources = format!("<< /Font << /F1 {font_x} 0 R >> /XObject << /Fm {form} 0 R >> >>");
    let (loaded, page) = one_page(&mut doc, &resources, b"BT /F1 10 Tf (A) Tj ET /Fm Do");
    let texts: Vec<String> = read(&loaded, page, PAGE_BUDGET).expect("the page reads").glyphs.into_iter().map(|g| g.text).collect();
    assert_eq!(texts, vec!["X", "Y"]);
}

#[test]
fn a_form_that_leaves_a_q_open_does_not_change_the_graphics_state_of_its_page() {
    // The form pushes a state and never pops it. The page's own Q then has to pop the page's q, so the text after it is at the origin.
    let mut doc = Doc::new();
    let font = doc.add(FONT);
    let form = doc.reserve();
    doc.stream(form, "/Type /XObject /Subtype /Form /BBox [0 0 612 792] /Resources << >>", b"q 1 0 0 1 300 0 cm");
    let resources = format!("<< /Font << /F1 {font} 0 R >> /XObject << /Fm {form} 0 R >> >>");
    let (loaded, page) = one_page(&mut doc, &resources, b"q 1 0 0 1 50 0 cm /Fm Do Q BT /F1 10 Tf 1 0 0 1 0 0 Tm (A) Tj ET");
    let glyphs = read(&loaded, page, PAGE_BUDGET).expect("the page reads").glyphs;
    assert_eq!(glyphs.len(), 1);
    assert_eq!(glyphs[0].origin_x, 0.0);
}

#[test]
fn a_form_with_fonts_of_its_own_builds_its_font_map_once_for_all_its_runs() {
    // The form has 1,000 fonts of its own, and the page runs it 100 times. The map is built on the first run and shared by the rest, so
    // the page costs about 1,300 units of work. Building the map for every run would cost 100,000 units.
    let mut doc = Doc::new();
    let fonts: String = (0..1_000).map(|i| format!("/Fa{i} << >> ")).collect();
    let form = doc.reserve();
    doc.stream(form, &format!("/Type /XObject /Subtype /Form /BBox [0 0 612 792] /Resources << /Font << {fonts} >> >>"), b"q Q");
    let (loaded, page) = one_page(&mut doc, &format!("<< /XObject << /F1 {form} 0 R >> >>"), "/F1 Do ".repeat(100).as_bytes());
    assert!(read(&loaded, page, Budget { work: 2_000, ..PAGE_BUDGET }).is_ok());
    // The first build alone is 1,000 units, so a smaller page limit refuses it. The build is charged.
    assert_eq!(read(&loaded, page, Budget { work: 500, ..PAGE_BUDGET }).err().expect("refused").code(), "PDF_CONTENT_BUDGET");
}

#[test]
fn nesting_graphics_states_past_the_limit_fails_the_page() {
    // 28 nested q operators are the limit. The 29th fails the page. Balanced q Q pairs never nest.
    let mut doc = Doc::new();
    let (loaded, page) = one_page(&mut doc, "<< >>", "q ".repeat(28).as_bytes());
    assert!(read(&loaded, page, PAGE_BUDGET).is_ok());
    let (loaded, page) = one_page(&mut doc, "<< >>", "q ".repeat(29).as_bytes());
    assert_eq!(read(&loaded, page, PAGE_BUDGET).err().expect("refused").code(), "PDF_GSTATE_DEPTH");
    let (loaded, page) = one_page(&mut doc, "<< >>", "q Q ".repeat(1_000).as_bytes());
    assert!(read(&loaded, page, PAGE_BUDGET).is_ok());
}

#[test]
fn a_contents_array_with_more_entries_than_one_page_may_list_fails_the_page() {
    // Each entry names one empty stream, which decodes to no bytes. Only the entry count can refuse the array.
    let mut doc = Doc::new();
    let empty = doc.reserve();
    doc.stream(empty, "", b"");
    let entries: String = (0..5_000).map(|_| format!("{empty} 0 R ")).collect();
    let root = doc.reserve();
    let page = doc.add(&format!("<< /Type /Page /Parent {root} 0 R /MediaBox [0 0 612 792] /Resources << >> /Contents [{entries}] >>"));
    doc.set(root, &format!("<< /Type /Pages /Kids [{page} 0 R] /Count 1 >>"));
    let catalog = doc.add(&format!("<< /Type /Catalog /Pages {root} 0 R >>"));
    let loaded = Document::load_mem(&doc.finish(catalog)).expect("the synthetic document parses");
    let page_id = *loaded.get_pages().values().next().expect("one page");
    assert_eq!(read(&loaded, page_id, PAGE_BUDGET).err().expect("refused").code(), "PDF_CONTENT_BUDGET");
}

#[test]
fn a_direct_entry_in_contents_fails_the_page_instead_of_being_skipped() {
    let mut doc = Doc::new();
    let stream = doc.reserve();
    doc.stream(stream, "", b"q Q");
    let root = doc.reserve();
    let page = doc.add(&format!("<< /Type /Page /Parent {root} 0 R /MediaBox [0 0 612 792] /Resources << >> /Contents [{stream} 0 R 42] >>"));
    doc.set(root, &format!("<< /Type /Pages /Kids [{page} 0 R] /Count 1 >>"));
    let catalog = doc.add(&format!("<< /Type /Catalog /Pages {root} 0 R >>"));
    let loaded = Document::load_mem(&doc.finish(catalog)).expect("the synthetic document parses");
    let page_id = *loaded.get_pages().values().next().expect("one page");
    assert_eq!(read(&loaded, page_id, PAGE_BUDGET).err().expect("refused").code(), "PDF_CONTENTS_INVALID");
}

#[test]
fn a_text_or_xobject_operator_with_an_operand_of_the_wrong_type_fails_the_page() {
    // An operand of the wrong type would skip text or a form, so the page is not read.
    let mut doc = Doc::new();
    let font = doc.add(FONT);
    for content in ["BT /F1 10 Tf 1 Tj ET", "BT /F1 10 Tf [(A) /B] TJ ET", "1 Do"] {
        let (loaded, page) = one_page(&mut doc, &format!("<< /Font << /F1 {font} 0 R >> >>"), content.as_bytes());
        assert_eq!(read(&loaded, page, PAGE_BUDGET).err().expect("refused").code(), "PDF_CONTENT_OPERAND", "{content}");
    }
}
