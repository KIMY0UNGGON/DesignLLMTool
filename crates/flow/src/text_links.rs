//! Hyperlinks of paragraph text, laid out as link regions of the scene (PDF link annotations).
//!
//! The spans are a side table: the flow model, its digest and the native bytes do not carry them, and a layout without
//! spans is unchanged. An adapter reads the links from its native records and passes them to
//! [`crate::layout_flow_with_links`]; the layout gives each span one rectangle per line it covers.
use designllm_contracts::{Id, MAX_INTERACTION_URL_BYTES};
use std::ops::Range;

use crate::text::{LogicalParagraph, OBJECT_REPLACEMENT};

/// Bytes of the object replacement character (U+FFFC). The logical text stands for each inline atom with it, so a native
/// position counts an atom as this many bytes.
pub const OBJECT_REPLACEMENT_BYTES: usize = OBJECT_REPLACEMENT.len_utf8();

/// A hyperlink over the graphemes `[start, end)` of a paragraph (end exclusive, in the paragraph's grapheme units).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextLinkSpan {
    pub paragraph_id: Id,
    pub start: u32,
    pub end: u32,
    pub url: String,
}

/// The grapheme range `[start, end)` of `paragraph` whose graphemes lie inside the logical text bytes `bytes` (see
/// [`LogicalParagraph::text`]). A grapheme that is only partly inside is left out. `None` when no grapheme is inside.
pub fn grapheme_span(paragraph: &LogicalParagraph, bytes: Range<usize>) -> Option<(u32, u32)> {
    let inside: Vec<usize> = paragraph
        .graphemes
        .iter()
        .enumerate()
        .filter(|(_, g)| g.logical.start >= bytes.start && g.logical.end <= bytes.end)
        .map(|(index, _)| index)
        .collect();
    let first = *inside.first()?;
    let last = *inside.last()?;
    Some((first as u32, (last + 1) as u32))
}

/// The start of the example targets below.
const EXAMPLE_PREFIX: &str = "https://example.com/";

/// Hyperlink targets that the layout never turns into link regions: one example for each kind of refusal of
/// `check_interaction_url` (an empty target, a scheme other than https, mailto or tel, no scheme, whitespace, a control
/// character, a missing host, a mailto or tel with nothing after the scheme) and one target a byte over the limit. The
/// format tests run this list through their own formats (`crate::test_support`), so the examples are kept here, once.
/// Test support, not a format rule. The control character is DEL (U+007F): XML 1.0 can carry it as written, so the
/// DOCX and HWPX samples hold it too.
pub fn refused_link_targets() -> Vec<String> {
    vec![
        String::new(),
        "http://example.com/refused".into(),
        "javascript:alert(1)".into(),
        "file:///C:/secret.txt".into(),
        "#bookmark".into(),
        "https://example.com/a b".into(),
        "https://example.com/a\u{7f}b".into(),
        "https:///no-host".into(),
        "mailto:".into(),
        "tel:".into(),
        format!("{EXAMPLE_PREFIX}{}", "a".repeat(MAX_INTERACTION_URL_BYTES + 1 - EXAMPLE_PREFIX.len())),
    ]
}

/// The longest target the layout accepts (`MAX_INTERACTION_URL_BYTES` bytes): the positive control of the refusal tests.
pub fn longest_accepted_link_target() -> String {
    format!("{EXAMPLE_PREFIX}{}", "a".repeat(MAX_INTERACTION_URL_BYTES - EXAMPLE_PREFIX.len()))
}
