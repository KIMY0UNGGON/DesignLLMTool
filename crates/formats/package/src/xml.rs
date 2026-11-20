//! Byte-span XML patching and element indexing.
//!
//! An edit is a list of [`Splice`] values. Each splice replaces one byte range of the original
//! document. Bytes outside the replaced ranges are copied verbatim, so an unknown subtree, a comment,
//! an attribute, or a namespace declaration that an edit does not touch stays byte-identical.
//!
//! Element ranges come from roxmltree: an element range runs from the `<` of its start tag through
//! the `>` of its end tag, or of `/>` for an empty element.
//!
//! Anchors identify an element by its path of namespace-prefixed names with a zero-based occurrence
//! index among same-named siblings, for example `p:sld[0]/p:cSld[0]/p:spTree[0]/mc:AlternateContent[0]`.
//! Format engines can supply their own keyer (for example, to add a shape id).

use std::collections::BTreeMap;
use std::ops::Range;

use designllm_contracts::{sha256, DltError, DltResult, ErrorCode};
use roxmltree::{Document, Node};

use crate::package::{error, DIAG_MALFORMED};

/// One replacement of a byte range of the original document.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Splice {
    pub range: Range<usize>,
    pub replacement: Vec<u8>,
}

/// An element found by [`element_spans_with`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ElementSpan {
    /// Anchor path, for example `p:sld[0]/p:cSld[0]/p:spTree[0]`.
    pub anchor: String,
    /// Byte range of the whole element in the original document.
    pub range: Range<usize>,
    /// Nesting depth; the root element has depth 0.
    pub depth: usize,
}

/// Applies non-overlapping splices. Overlapping or out-of-range splices are rejected.
/// Two insertions at the same offset keep their input order.
pub fn apply(xml: &[u8], mut splices: Vec<Splice>) -> DltResult<Vec<u8>> {
    splices.sort_by_key(|s| (s.range.start, s.range.end));
    let mut out = Vec::with_capacity(xml.len() + 256);
    let mut cursor = 0usize;
    for splice in &splices {
        if splice.range.start < cursor || splice.range.end < splice.range.start || splice.range.end > xml.len() {
            return Err(error(
                ErrorCode::Internal,
                "XML_SPLICE_INVALID",
                format!("invalid splice {:?} (cursor {cursor}, length {})", splice.range, xml.len()),
            ));
        }
        out.extend_from_slice(&xml[cursor..splice.range.start]);
        out.extend_from_slice(&splice.replacement);
        cursor = splice.range.end;
    }
    out.extend_from_slice(&xml[cursor..]);
    Ok(out)
}

/// Applies splices and checks that the result is well-formed XML.
pub fn apply_checked(xml: &[u8], splices: Vec<Splice>) -> DltResult<Vec<u8>> {
    let out = apply(xml, splices)?;
    check_well_formed(&out)?;
    Ok(out)
}

/// Returns the document text and the byte offset where it starts. A UTF-8 BOM is skipped;
/// callers add the offset to every range that the parser reports.
pub fn text_and_offset(xml: &[u8]) -> DltResult<(&str, usize)> {
    let (bytes, offset) = match xml {
        [0xEF, 0xBB, 0xBF, rest @ ..] => (rest, 3),
        _ => (xml, 0),
    };
    let text = std::str::from_utf8(bytes).map_err(|e| malformed(format!("XML is not UTF-8: {e}")))?;
    Ok((text, offset))
}

/// Fails unless the bytes are well-formed UTF-8 XML.
pub fn check_well_formed(xml: &[u8]) -> DltResult<()> {
    let (text, _) = text_and_offset(xml)?;
    Document::parse(text).map(|_| ()).map_err(|e| malformed(format!("malformed XML: {e}")))
}

/// Index just past the `>` that closes the start tag at `start` (which must be `<`).
/// Quoted attribute values are skipped, so a `>` inside a value does not end the tag.
pub fn start_tag_end(xml: &[u8], start: usize) -> DltResult<usize> {
    if xml.get(start) != Some(&b'<') {
        return Err(malformed(format!("no '<' at offset {start}")));
    }
    let mut quote: Option<u8> = None;
    for (offset, &byte) in xml[start + 1..].iter().enumerate() {
        match quote {
            Some(q) => {
                if byte == q {
                    quote = None;
                }
            }
            None => match byte {
                b'"' | b'\'' => quote = Some(byte),
                b'>' => return Ok(start + 1 + offset + 1),
                _ => {}
            },
        }
    }
    Err(malformed("unterminated start tag"))
}

fn is_ws(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\r' | b'\n')
}

fn qname_end(xml: &[u8], start: usize) -> usize {
    let mut i = start + 1;
    while i < xml.len() && !is_ws(xml[i]) && xml[i] != b'/' && xml[i] != b'>' {
        i += 1;
    }
    i
}

/// Qualified name (prefix:local) of the element whose `<` is at `start`.
pub fn qname(xml: &[u8], start: usize) -> DltResult<String> {
    let end = qname_end(xml, start);
    std::str::from_utf8(&xml[start + 1..end])
        .map(str::to_string)
        .map_err(|e| malformed(format!("qualified name is not UTF-8: {e}")))
}

/// True for an empty element (`<x .../>`), where the start tag is the whole element.
pub fn is_self_closing(xml: &[u8], element: &Range<usize>) -> DltResult<bool> {
    Ok(start_tag_end(xml, element.start)? == element.end)
}

/// Byte range of the element content between the start and end tags, or `None` when it is empty.
pub fn content_range(xml: &[u8], element: &Range<usize>) -> DltResult<Option<Range<usize>>> {
    let tag_end = start_tag_end(xml, element.start)?;
    if tag_end == element.end {
        return Ok(None);
    }
    let body = &xml[tag_end..element.end];
    let relative = body
        .windows(2)
        .rposition(|w| w == b"</")
        .ok_or_else(|| malformed("element without end tag"))?;
    Ok(Some(tag_end..tag_end + relative))
}

/// Value range (between the quotes) of attribute `name` in the start tag at `start`, if present.
pub fn attr_value_range(xml: &[u8], start: usize, name: &str) -> DltResult<Option<Range<usize>>> {
    let tag_end = start_tag_end(xml, start)?;
    let mut i = qname_end(xml, start);
    loop {
        while i < tag_end && is_ws(xml[i]) {
            i += 1;
        }
        if i >= tag_end || xml[i] == b'>' || xml[i] == b'/' {
            return Ok(None);
        }
        let name_start = i;
        while i < tag_end && xml[i] != b'=' && !is_ws(xml[i]) {
            i += 1;
        }
        let attribute = std::str::from_utf8(&xml[name_start..i]).map_err(|e| malformed(format!("attribute name: {e}")))?;
        while i < tag_end && is_ws(xml[i]) {
            i += 1;
        }
        if i >= tag_end || xml[i] != b'=' {
            return Err(malformed(format!("malformed attribute {attribute}")));
        }
        i += 1;
        while i < tag_end && is_ws(xml[i]) {
            i += 1;
        }
        let quote = match xml.get(i) {
            Some(&q) if q == b'"' || q == b'\'' => q,
            _ => return Err(malformed(format!("unquoted attribute {attribute}"))),
        };
        i += 1;
        let value_start = i;
        while i < tag_end && xml[i] != quote {
            i += 1;
        }
        let value_end = i;
        i += 1;
        if attribute == name {
            return Ok(Some(value_start..value_end));
        }
    }
}

/// Rejects characters that XML 1.0 cannot represent.
pub fn check_xml_chars(text: &str) -> DltResult<()> {
    for ch in text.chars() {
        let code = ch as u32;
        let allowed = matches!(code, 0x9 | 0xA | 0xD | 0x20..=0xD7FF | 0xE000..=0xFFFD | 0x10000..=0x10FFFF);
        if !allowed {
            return Err(malformed(format!("character U+{code:04X} cannot be written to XML")));
        }
    }
    Ok(())
}

/// Escapes text content. Carriage returns are written as `&#13;` so they survive line-end normalization.
pub fn escape_text(text: &str) -> DltResult<String> {
    check_xml_chars(text)?;
    Ok(text
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('\r', "&#13;"))
}

/// Escapes an attribute value for double-quoted output. Whitespace controls are written as character
/// references so attribute-value normalization cannot change them.
pub fn escape_attr(value: &str) -> DltResult<String> {
    escape_attr_quoted(value, b'"')
}

/// `escape_attr` for a value inside `quote` quotes: in single quotes `'` is escaped as well.
fn escape_attr_quoted(value: &str, quote: u8) -> DltResult<String> {
    check_xml_chars(value)?;
    let escaped = value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\n', "&#10;")
        .replace('\r', "&#13;")
        .replace('\t', "&#9;");
    Ok(if quote == b'\'' { escaped.replace('\'', "&apos;") } else { escaped })
}

/// Sets attribute `name` on the element at `element`, keeping the quotes the attribute has. Inserts the attribute,
/// double-quoted, when it is missing.
pub fn set_attribute(xml: &[u8], element: &Range<usize>, name: &str, value: &str) -> DltResult<Splice> {
    if let Some(range) = attr_value_range(xml, element.start, name)? {
        let escaped = escape_attr_quoted(value, xml[range.start - 1])?;
        return Ok(Splice { range, replacement: escaped.into_bytes() });
    }
    let escaped = escape_attr(value)?;
    let at = qname_end(xml, element.start);
    Ok(Splice { range: at..at, replacement: format!(" {name}=\"{escaped}\"").into_bytes() })
}

/// Inserts `fragment` markup at byte `position`. The position is an offset into the original bytes.
pub fn insert_at(position: usize, fragment: &str) -> Splice {
    Splice { range: position..position, replacement: fragment.as_bytes().to_vec() }
}

/// Replaces a whole element with `replacement` markup. The markup is written as given.
pub fn replace_element(element: &Range<usize>, replacement: &str) -> Splice {
    Splice { range: element.clone(), replacement: replacement.as_bytes().to_vec() }
}

/// Deletes a whole element.
pub fn remove_element(element: &Range<usize>) -> Splice {
    Splice { range: element.clone(), replacement: Vec::new() }
}

/// Inserts `fragment` markup as a child of the element at `parent`.
/// `before` is the start offset of the successor child when one exists; otherwise the fragment goes
/// at the end of the content. An empty element is expanded to an open/close pair.
pub fn insert_child(xml: &[u8], parent: &Range<usize>, before: Option<usize>, fragment: &str) -> DltResult<Splice> {
    if let Some(position) = before {
        return Ok(Splice { range: position..position, replacement: fragment.as_bytes().to_vec() });
    }
    match content_range(xml, parent)? {
        Some(content) => Ok(Splice { range: content.end..content.end, replacement: fragment.as_bytes().to_vec() }),
        None => expand_empty(xml, parent, fragment),
    }
}

fn expand_empty(xml: &[u8], element: &Range<usize>, body: &str) -> DltResult<Splice> {
    if !is_self_closing(xml, element)? {
        return Err(malformed("expand_empty called on a non-empty element"));
    }
    let name = qname(xml, element.start)?;
    let head_end = element.end - 2;
    let head = std::str::from_utf8(&xml[element.start..head_end]).map_err(|e| malformed(format!("start tag: {e}")))?;
    Ok(Splice {
        range: element.clone(),
        replacement: format!("{head}>{body}</{name}>").into_bytes(),
    })
}

/// Replaces the text content of an element that holds text only. Refuses elements with markup.
pub fn set_text(xml: &[u8], element: &Range<usize>, value: &str) -> DltResult<Splice> {
    let escaped = escape_text(value)?;
    match content_range(xml, element)? {
        Some(content) => {
            if xml[content.clone()].contains(&b'<') {
                return Err(malformed("refusing to replace markup inside a text element"));
            }
            Ok(Splice { range: content, replacement: escaped.into_bytes() })
        }
        None => expand_empty(xml, element, &escaped),
    }
}

/// Default anchor segment: `prefix:local` for the usual OOXML namespaces, otherwise `{uri}local`.
pub fn default_key(node: Node<'_, '_>) -> String {
    let namespace = node.tag_name().namespace().unwrap_or("");
    let local = node.tag_name().name();
    if namespace.is_empty() {
        return local.to_string();
    }
    match known_prefix(namespace) {
        Some(prefix) => format!("{prefix}:{local}"),
        None => format!("{{{namespace}}}{local}"),
    }
}

fn known_prefix(namespace: &str) -> Option<&'static str> {
    Some(match namespace {
        "http://schemas.openxmlformats.org/presentationml/2006/main" => "p",
        "http://schemas.openxmlformats.org/drawingml/2006/main" => "a",
        "http://schemas.openxmlformats.org/officeDocument/2006/relationships" => "r",
        "http://schemas.openxmlformats.org/markup-compatibility/2006" => "mc",
        "http://schemas.openxmlformats.org/wordprocessingml/2006/main" => "w",
        "http://schemas.openxmlformats.org/package/2006/relationships" => "rel",
        "http://schemas.openxmlformats.org/package/2006/content-types" => "ct",
        "http://schemas.microsoft.com/office/powerpoint/2010/main" => "p14",
        "http://schemas.microsoft.com/office/drawing/2014/main" => "a16",
        "http://schemas.openxmlformats.org/drawingml/2006/picture" => "pic",
        "http://purl.org/dc/elements/1.1/" => "dc",
        _ => return None,
    })
}

/// Indexes every element of a document with [`default_key`] anchors, in document order.
pub fn element_spans(xml: &[u8]) -> DltResult<Vec<ElementSpan>> {
    element_spans_with(xml, default_key)
}

/// Indexes every element of a document. `keyer` produces each anchor segment. A keyer that includes a
/// stable identifier, such as a shape id, makes anchors survive insertions of sibling elements.
pub fn element_spans_with(xml: &[u8], keyer: impl Fn(Node<'_, '_>) -> String) -> DltResult<Vec<ElementSpan>> {
    let (text, offset) = text_and_offset(xml)?;
    let doc = Document::parse(text).map_err(|e| malformed(format!("malformed XML: {e}")))?;
    let mut out = Vec::new();
    walk(doc.root(), "", 0, offset, &keyer, &mut out);
    Ok(out)
}

fn walk(
    node: Node<'_, '_>,
    prefix: &str,
    depth: usize,
    offset: usize,
    keyer: &impl Fn(Node<'_, '_>) -> String,
    out: &mut Vec<ElementSpan>,
) {
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for child in node.children().filter(|c| c.is_element()) {
        let key = keyer(child);
        let slot = counts.entry(key.clone()).or_insert(0);
        let occurrence = *slot;
        *slot += 1;
        let segment = format!("{key}[{occurrence}]");
        let anchor = if prefix.is_empty() { segment } else { format!("{prefix}/{segment}") };
        let range = child.range();
        out.push(ElementSpan { anchor: anchor.clone(), range: (range.start + offset)..(range.end + offset), depth });
        walk(child, &anchor, depth + 1, offset, keyer, out);
    }
}

/// SHA-256 (lowercase hex) of the bytes of one range.
pub fn range_sha256(xml: &[u8], range: &Range<usize>) -> String {
    sha256(&xml[range.clone()])
}

/// Concatenated character data of the direct text children of `node`, in document order. A comment adds nothing, so a
/// comment inside a text run does not cut it. An element child adds nothing either, so `a<x/>b` gives `ab`. A CDATA section
/// is a text child and counts as text. Text of grandchildren is not included. `roxmltree` `Node::text` reads only the first
/// child, so it is not used here.
pub fn direct_text(node: Node<'_, '_>) -> String {
    node.children().filter(|c| c.is_text()).filter_map(|c| c.text()).collect()
}

fn malformed(message: impl Into<String>) -> DltError {
    error(ErrorCode::InvalidArgument, DIAG_MALFORMED, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_tag_end_skips_quoted_gt() {
        let xml = b"<a x=\"1>2\" y='3'>body</a>";
        let end = start_tag_end(xml, 0).unwrap();
        assert_eq!(&xml[..end], b"<a x=\"1>2\" y='3'>");
    }

    #[test]
    fn attr_value_range_finds_quoted_values() {
        let xml = b"<a:rPr lang=\"en-US\" sz='2000'/>";
        let r = attr_value_range(xml, 0, "sz").unwrap().unwrap();
        assert_eq!(&xml[r], b"2000");
        assert!(attr_value_range(xml, 0, "missing").unwrap().is_none());
    }

    #[test]
    fn set_text_expands_empty_element_and_escapes() {
        let xml = b"<a:t/>";
        let out = apply(xml, vec![set_text(xml, &(0..xml.len()), "x & y <z>").unwrap()]).unwrap();
        assert_eq!(out, b"<a:t>x &amp; y &lt;z&gt;</a:t>".to_vec());
    }

    #[test]
    fn insert_child_before_successor_keeps_bytes_outside() {
        let xml = b"<a:rPr sz=\"1\"><a:solidFill/><a:extLst>E</a:extLst></a:rPr>";
        let parent = 0..xml.len();
        let before = std::str::from_utf8(xml).unwrap().find("<a:extLst>");
        let s = insert_child(xml, &parent, before, "<a:latin typeface=\"Arial\"/>").unwrap();
        let out = apply(xml, vec![s]).unwrap();
        assert_eq!(
            out,
            b"<a:rPr sz=\"1\"><a:solidFill/><a:latin typeface=\"Arial\"/><a:extLst>E</a:extLst></a:rPr>".to_vec()
        );
    }

    #[test]
    fn insert_child_appends_and_expands_empty_parent() {
        let xml = b"<a:rPr lang=\"en-US\"><a:solidFill/></a:rPr>";
        let s = insert_child(xml, &(0..xml.len()), None, "<a:latin typeface=\"Arial\"/>").unwrap();
        assert_eq!(
            apply(xml, vec![s]).unwrap(),
            b"<a:rPr lang=\"en-US\"><a:solidFill/><a:latin typeface=\"Arial\"/></a:rPr>".to_vec()
        );
        let empty = b"<a:rPr lang=\"en-US\"/>";
        let s = insert_child(empty, &(0..empty.len()), None, "<a:ea typeface=\"X\"/>").unwrap();
        assert_eq!(apply(empty, vec![s]).unwrap(), b"<a:rPr lang=\"en-US\"><a:ea typeface=\"X\"/></a:rPr>".to_vec());
    }

    #[test]
    fn set_attribute_inserts_when_missing_and_escapes_values() {
        let xml = b"<a:rPr lang=\"en-US\"/>";
        let s = set_attribute(xml, &(0..xml.len()), "sz", "2400").unwrap();
        assert_eq!(apply(xml, vec![s]).unwrap(), b"<a:rPr sz=\"2400\" lang=\"en-US\"/>".to_vec());
        let s = set_attribute(xml, &(0..xml.len()), "lang", "a\"b\n").unwrap();
        assert_eq!(apply(xml, vec![s]).unwrap(), b"<a:rPr lang=\"a&quot;b&#10;\"/>".to_vec());
    }

    #[test]
    fn apply_rejects_overlapping_and_out_of_range_splices() {
        let xml = b"0123456789";
        let overlap = apply(
            xml,
            vec![
                Splice { range: 2..6, replacement: b"x".to_vec() },
                Splice { range: 4..8, replacement: b"y".to_vec() },
            ],
        );
        assert!(overlap.is_err());
        assert!(apply(xml, vec![Splice { range: 5..50, replacement: vec![] }]).is_err());
    }

    #[test]
    fn invalid_xml_characters_are_refused_not_written() {
        assert!(escape_text("bad \u{0001} char").is_err());
        assert!(escape_attr("bad \u{000B} char").is_err());
        assert_eq!(escape_text("a\rb").unwrap(), "a&#13;b");
    }

    #[test]
    fn an_apostrophe_is_escaped_only_inside_single_quotes() {
        // Double-quoted output, as every escape_attr caller writes it, keeps the apostrophe as it was.
        assert_eq!(escape_attr("O'Reilly \"Sans\"").unwrap(), "O'Reilly &quot;Sans&quot;");
        for (xml, expected) in [
            (&b"<a v='x'/>"[..], "<a v='O&apos;Reilly &quot;Sans&quot;'/>"),
            (&b"<a v=\"x\"/>"[..], "<a v=\"O'Reilly &quot;Sans&quot;\"/>"),
            (&b"<a/>"[..], "<a v=\"O'Reilly &quot;Sans&quot;\"/>"),
        ] {
            let splice = set_attribute(xml, &(0..xml.len()), "v", "O'Reilly \"Sans\"").unwrap();
            let out = apply_checked(xml, vec![splice]).unwrap();
            let text = std::str::from_utf8(&out).unwrap();
            assert_eq!(text, expected);
            let doc = Document::parse(text).unwrap();
            assert_eq!(doc.root_element().attribute("v"), Some("O'Reilly \"Sans\""), "{text}");
        }
    }

    #[test]
    fn apply_checked_refuses_splices_that_break_well_formedness() {
        let xml = b"<a><b/></a>";
        let broken = apply_checked(xml, vec![Splice { range: 3..7, replacement: b"<c>".to_vec() }]);
        assert!(broken.is_err());
    }

    #[test]
    fn element_anchors_use_occurrence_indices_and_ranges_cover_the_element() {
        let xml = b"<p:sld xmlns:p=\"http://schemas.openxmlformats.org/presentationml/2006/main\"><p:sp/><p:sp><p:x/></p:sp></p:sld>";
        let spans = element_spans(xml).unwrap();
        let anchors: Vec<&str> = spans.iter().map(|s| s.anchor.as_str()).collect();
        assert_eq!(
            anchors,
            vec!["p:sld[0]", "p:sld[0]/p:sp[0]", "p:sld[0]/p:sp[1]", "p:sld[0]/p:sp[1]/p:x[0]"]
        );
        assert_eq!(&xml[spans[2].range.clone()], b"<p:sp><p:x/></p:sp>");
        assert_eq!(spans[0].depth, 0);
        assert_eq!(spans[3].depth, 2);
    }

    #[test]
    fn custom_keyer_and_bom_offsets_keep_ranges_absolute() {
        let mut xml = vec![0xEF, 0xBB, 0xBF];
        xml.extend_from_slice(b"<r><s id=\"7\"/></r>");
        let spans = element_spans_with(&xml, |node| match node.attribute("id") {
            Some(id) => format!("{}#{id}", node.tag_name().name()),
            None => node.tag_name().name().to_string(),
        })
        .unwrap();
        assert_eq!(spans[1].anchor, "r[0]/s#7[0]");
        assert_eq!(&xml[spans[1].range.clone()], b"<s id=\"7\"/>");
    }

    #[test]
    fn remove_element_deletes_only_its_bytes() {
        let xml = b"<r><keep/><drop>x</drop><keep/></r>";
        let spans = element_spans(xml).unwrap();
        let drop = spans.iter().find(|s| s.anchor == "r[0]/drop[0]").unwrap();
        assert_eq!(&xml[drop.range.clone()], b"<drop>x</drop>");
        let out = apply(xml, vec![remove_element(&drop.range)]).unwrap();
        assert_eq!(out, b"<r><keep/><keep/></r>".to_vec());
    }

    #[test]
    fn direct_text_joins_the_text_children_and_skips_comments_and_elements() {
        let doc = Document::parse("<a>one<!-- c -->two<b>grand</b><![CDATA[<three>]]></a>").unwrap();
        assert_eq!(direct_text(doc.root_element()), "onetwo<three>");
    }

    #[test]
    fn direct_text_of_an_element_without_text_is_empty() {
        let doc = Document::parse("<a><b>x</b><!-- c --></a>").unwrap();
        assert_eq!(direct_text(doc.root_element()), "");
    }
}
