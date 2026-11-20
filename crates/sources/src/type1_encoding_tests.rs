//! Unit tests of the built-in encoding reader. Each program is written out here: its cleartext, the operator `eexec`, and a binary
//! tail that the reader never reads.

use super::{read_builtin_encoding, MAX_CLEARTEXT_BYTES};
use crate::content::{Budget, Meter, Meters, PAGE_BUDGET, REQUEST_BUDGET};
use crate::error::PResult;

/// A program whose cleartext is `cleartext`, then `eexec` and a binary tail.
fn program(cleartext: &str) -> Vec<u8> {
    let mut bytes = format!("{cleartext}\ncurrentfile eexec\n").into_bytes();
    bytes.extend_from_slice(&[0xD9, 0x4B, 0x12, 0x9A, 0x33, 0x7F, 0x00, 0x81]);
    bytes
}

/// A program whose cleartext is exactly `before_eexec` bytes long: a comment of x's that holds a StandardEncoding.
fn sized(before_eexec: usize) -> Vec<u8> {
    let head = "/Encoding StandardEncoding def\n%";
    let tail = "\ncurrentfile ";
    let fill = before_eexec - head.len() - tail.len();
    let mut bytes = format!("{head}{}{tail}", "x".repeat(fill)).into_bytes();
    bytes.extend_from_slice(b"eexec\n\x00\x01binary");
    bytes
}

/// Reads the encoding of `bytes` under `page_budget` and the default request budget.
fn read_with(bytes: &[u8], page_budget: Budget) -> PResult<Option<Vec<String>>> {
    let page = Meter::page(page_budget);
    let request = Meter::request(REQUEST_BUDGET);
    read_builtin_encoding(bytes, Meters { page: &page, request: &request })
}

fn read(bytes: &[u8]) -> Option<Vec<String>> {
    read_with(bytes, PAGE_BUDGET).expect("within the budget")
}

#[test]
fn the_standard_form_gives_the_standard_names() {
    let names = read(&program("/Encoding StandardEncoding def")).expect("read");
    assert_eq!(names.len(), 256);
    assert_eq!(names[0x27], "quoteright");
    assert_eq!(names[0x60], "quoteleft");
    // StandardEncoding leaves 0x80 undefined, so its name is empty.
    assert_eq!(names[0x80], "");
}

#[test]
fn the_array_form_reads_each_entry_and_leaves_the_other_codes_empty() {
    let names = read(&program("/Encoding 256 array\n0 1 255 {1 index exch /.notdef put} for\ndup 65 /A put\ndup 200 /Euro put\nreadonly def"))
        .expect("read");
    assert_eq!((names[65].as_str(), names[200].as_str(), names[66].as_str()), ("A", "Euro", ""));
}

#[test]
fn the_array_form_without_the_notdef_loop_and_ended_by_def_is_read() {
    let names = read(&program("/Encoding 256 array dup 65 /A put def")).expect("read");
    assert_eq!(names[65], "A");
}

#[test]
fn a_later_entry_for_a_code_replaces_the_earlier_one() {
    let names = read(&program("/Encoding 256 array dup 65 /A put dup 65 /B put readonly def")).expect("read");
    assert_eq!(names[65], "B");
}

#[test]
fn the_notdef_loop_is_accepted_in_its_exact_token_form_whatever_the_white_space() {
    for loop_text in [
        "0 1 255 {1 index exch /.notdef put} for",
        "0\t1  255\n{ 1 index exch /.notdef put }\nfor",
        "0 1 255{1 index exch /.notdef put}for",
        "0 1 255 {1 index exch /.notdef put} for % loop that sets every code to notdef",
    ] {
        let cleartext = format!("/Encoding 256 array\n{loop_text}\ndup 65 /A put\nreadonly def");
        let names = read(&program(&cleartext)).unwrap_or_else(|| panic!("refused: {loop_text}"));
        assert_eq!(names[65], "A", "{loop_text}");
    }
}

#[test]
fn a_notdef_loop_that_differs_by_one_token_or_is_another_procedure_is_refused() {
    // Each line changes one number, operator or name of the exact loop, or replaces the procedure. Each one makes the encoding
    // unreadable, so no code of it has a name. The exact form is read in the first test above.
    for loop_text in [
        "0 1 254 {1 index exch /.notdef put} for",
        "1 1 255 {1 index exch /.notdef put} for",
        "0 2 255 {1 index exch /.notdef put} for",
        "0 1 255 {2 index exch /.notdef put} for",
        "0 1 255 {1 index pop /.notdef put} for",
        "0 1 255 {1 index exch /.notdef def} for",
        "0 1 255 {1 index exch /space put} for",
        "0 1 255 {1 index exch /.notdef put} repeat",
        "0 1 255 {pop} for",
    ] {
        let cleartext = format!("/Encoding 256 array\n{loop_text}\ndup 65 /A put\nreadonly def");
        assert_eq!(read(&program(&cleartext)), None, "{loop_text}");
    }
}

#[test]
fn a_broken_encoding_gives_no_encoding() {
    let broken = [
        "/Encoding 256 array dup 65 /A put",
        "/Encoding 256 array dup 65 /A put readonly",
        "/Encoding 256 array dup -1 /A put readonly def",
        "/Encoding 256 array dup 256 /A put readonly def",
        "/Encoding 256 array dup 6.5 /A put readonly def",
        "/Encoding 256 array dup 65 A put readonly def",
        "/Encoding 256 array dup 65 /A get readonly def",
        "/Encoding 256 array dup 65 /A put 66 /B put readonly def",
        "/Encoding 256 array dup 65 /A put 0 1 255 {1 index exch /.notdef put} for readonly def",
        "/Encoding 128 array dup 65 /A put readonly def",
        "/Encoding /WinAnsiEncoding def",
        "/Encoding StandardEncoding readonly def",
        "/Encoding 256 array dup 65 /A put readonly def /Encoding StandardEncoding def",
    ];
    for cleartext in broken {
        assert_eq!(read(&program(cleartext)), None, "{cleartext}");
    }
}

#[test]
fn a_program_without_an_encoding_or_without_the_eexec_operator_gives_no_encoding() {
    assert_eq!(read(&program("/FontName /Sym def")), None);
    // The word eexec is not the operator when a letter or digit touches it.
    assert_eq!(read(b"/Encoding StandardEncoding def\ncurrentfile xeexec2\n\x00\x01"), None);
    assert_eq!(read(b"/Encoding StandardEncoding def"), None);
}

#[test]
fn a_cleartext_of_the_limit_is_read_and_one_byte_more_is_not() {
    assert_eq!(read(&sized(MAX_CLEARTEXT_BYTES)).map(|names| names[0x27].clone()), Some("quoteright".to_string()));
    assert_eq!(read(&sized(MAX_CLEARTEXT_BYTES + 1)), None);
}

#[test]
fn a_page_budget_refusal_is_an_error_and_not_an_absent_encoding() {
    let bytes = program("/Encoding StandardEncoding def");
    let tight = Budget { tokens: 2, ..PAGE_BUDGET };
    assert_eq!(read_with(&bytes, tight).unwrap_err().code(), "PDF_CONTENT_BUDGET");
}
