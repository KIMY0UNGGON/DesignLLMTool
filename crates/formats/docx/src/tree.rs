//! Span-accurate view of one WordprocessingML part.
//!
//! The parser reads element structure through roxmltree. Every byte edit uses absolute offsets
//! into the original part, so unknown XML that is not changed is copied verbatim.

use std::ops::Range;

use designllm_contracts::{sha256, DltError, DltResult, ErrorCode};
use designllm_package::xml::{self as bx, Splice};
use roxmltree::{Document, Node};

pub use designllm_package::opc::{NS_A, NS_MC, NS_OFFICE_RELATIONSHIPS as NS_R};

pub const NS_W: &str = "http://schemas.openxmlformats.org/wordprocessingml/2006/main";
pub const NS_WP: &str = "http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing";
pub const NS_PIC: &str = "http://schemas.openxmlformats.org/drawingml/2006/picture";
pub const NS_W14: &str = "http://schemas.microsoft.com/office/word/2010/wordml";
pub const NS_W15: &str = "http://schemas.microsoft.com/office/word/2012/wordml";
pub const NS_M: &str = "http://schemas.openxmlformats.org/officeDocument/2006/math";

/// Canonical prefix used when the engine writes an element of this namespace.
pub fn canonical_prefix(uri: &str) -> Option<&'static str> {
    Some(match uri {
        NS_W => "w",
        NS_R => "r",
        NS_WP => "wp",
        NS_A => "a",
        NS_PIC => "pic",
        NS_MC => "mc",
        NS_W14 => "w14",
        NS_W15 => "w15",
        NS_M => "m",
        _ => return None,
    })
}

/// Stable element key: `w:p` for known namespaces, `{uri}local` otherwise.
pub fn qkey(node: Node<'_, '_>) -> String {
    let name = node.tag_name();
    let ns = name.namespace().unwrap_or("");
    match canonical_prefix(ns) {
        Some(prefix) => format!("{prefix}:{}", name.name()),
        None => format!("{{{ns}}}{}", name.name()),
    }
}

pub fn is_w(node: Node<'_, '_>, local: &str) -> bool {
    node.is_element() && node.tag_name().namespace() == Some(NS_W) && node.tag_name().name() == local
}

pub fn is_ns(node: Node<'_, '_>, ns: &str, local: &str) -> bool {
    node.is_element() && node.tag_name().namespace() == Some(ns) && node.tag_name().name() == local
}

pub fn w_attr<'a>(node: Node<'a, '_>, local: &str) -> Option<&'a str> {
    node.attribute((NS_W, local))
}

/// Element children, in document order.
pub fn elements<'a, 'i>(node: Node<'a, 'i>) -> Vec<Node<'a, 'i>> {
    node.children().filter(|c| c.is_element()).collect()
}

/// Navigates a path of element-child indices from `root`.
pub fn node_at<'a, 'i>(root: Node<'a, 'i>, path: &[usize]) -> Option<Node<'a, 'i>> {
    let mut current = root;
    for &index in path {
        current = *elements(current).get(index)?;
    }
    Some(current)
}

/// Concatenated text of all `w:t` descendants.
pub fn w_text(node: Node<'_, '_>) -> String {
    node.descendants().filter(|n| is_w(*n, "t")).filter_map(|n| n.text()).collect()
}

/// A parsed part: the roxmltree document plus the absolute offset of its text inside `bytes`.
pub struct Tree<'a> {
    pub bytes: &'a [u8],
    pub offset: usize,
    pub doc: Document<'a>,
}

impl<'a> Tree<'a> {
    pub fn parse(bytes: &'a [u8]) -> DltResult<Self> {
        let (text, offset) = bx::text_and_offset(bytes)?;
        let doc = Document::parse(text).map_err(|e| malformed(format!("malformed XML: {e}")))?;
        Ok(Self { bytes, offset, doc })
    }

    pub fn root(&self) -> Node<'_, '_> {
        self.doc.root_element()
    }

    /// Absolute byte range of an element.
    pub fn span(&self, node: Node<'_, '_>) -> Range<usize> {
        let r = node.range();
        (r.start + self.offset)..(r.end + self.offset)
    }

    pub fn raw(&self, range: &Range<usize>) -> &'a [u8] {
        &self.bytes[range.clone()]
    }

    /// Absolute range of the start tag (from `<` through `>`).
    pub fn start_tag(&self, element: &Range<usize>) -> DltResult<Range<usize>> {
        let end = bx::start_tag_end(self.bytes, element.start)?;
        Ok(element.start..end)
    }

    pub fn is_self_closing(&self, element: &Range<usize>) -> DltResult<bool> {
        bx::is_self_closing(self.bytes, element)
    }

    /// Element child keys and absolute spans, in document order (text and comments excluded).
    pub fn child_spans(&self, node: Node<'_, '_>) -> Vec<(String, Range<usize>)> {
        elements(node).into_iter().map(|c| (qkey(c), self.span(c))).collect()
    }
}

/// Malformed input is an invalid argument for the caller, not an internal failure.
pub fn malformed(message: impl Into<String>) -> DltError {
    DltError::new(ErrorCode::InvalidArgument, message.into())
}

/// SHA-256 hex of the bytes of an element.
pub fn span_sha(bytes: &[u8], range: &Range<usize>) -> String {
    sha256(&bytes[range.clone()])
}

/// Splices applied to a sub-range; `splices` use absolute offsets inside `range`.
pub fn apply_local(bytes: &[u8], range: &Range<usize>, splices: Vec<Splice>) -> DltResult<Vec<u8>> {
    let relative: Vec<Splice> = splices
        .into_iter()
        .map(|s| Splice { range: (s.range.start - range.start)..(s.range.end - range.start), replacement: s.replacement })
        .collect();
    bx::apply(&bytes[range.clone()], relative)
}

/// One attribute of a start tag, with its raw (still escaped) value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attr {
    pub name: String,
    pub raw_value: String,
    pub quote: char,
}

/// Editable view of a start tag. Unknown attributes keep their raw text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StartTag {
    pub name: String,
    pub attrs: Vec<Attr>,
    pub self_closing: bool,
}

impl StartTag {
    /// Parses a complete start tag (`<name ...>` or `<name .../>`).
    pub fn parse(raw: &[u8]) -> DltResult<Self> {
        let text = std::str::from_utf8(raw).map_err(|e| malformed(format!("start tag is not UTF-8: {e}")))?;
        if !text.starts_with('<') {
            return Err(malformed("start tag does not begin with '<'"));
        }
        let body = &text[1..];
        let name_end = body.find(|c: char| c.is_ascii_whitespace() || c == '/' || c == '>').unwrap_or(body.len());
        let name = body[..name_end].to_string();
        let mut rest = &body[name_end..];
        let mut attrs = Vec::new();
        let mut self_closing = false;
        loop {
            rest = rest.trim_start();
            if rest.starts_with("/>") {
                self_closing = true;
                break;
            }
            if rest.starts_with('>') {
                break;
            }
            let eq = rest.find('=').ok_or_else(|| malformed("attribute without '='"))?;
            let attr_name = rest[..eq].trim().to_string();
            rest = rest[eq + 1..].trim_start();
            let quote = rest.chars().next().ok_or_else(|| malformed("attribute without value"))?;
            if quote != '"' && quote != '\'' {
                return Err(malformed("unquoted attribute value"));
            }
            let close = rest[1..].find(quote).ok_or_else(|| malformed("unterminated attribute value"))?;
            let raw_value = rest[1..1 + close].to_string();
            attrs.push(Attr { name: attr_name, raw_value, quote });
            rest = &rest[1 + close + 1..];
        }
        Ok(Self { name, attrs, self_closing })
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.attrs.iter().find(|a| a.name == name).map(|a| a.raw_value.as_str())
    }

    /// Sets an attribute. `value` is unescaped text; it is escaped here.
    pub fn set(&mut self, name: &str, value: &str) -> DltResult<()> {
        let escaped = bx::escape_attr(value)?;
        match self.attrs.iter_mut().find(|a| a.name == name) {
            Some(attr) => attr.raw_value = escaped,
            None => self.attrs.push(Attr { name: name.to_string(), raw_value: escaped, quote: '"' }),
        }
        Ok(())
    }

    pub fn remove(&mut self, name: &str) {
        self.attrs.retain(|a| a.name != name);
    }

    pub fn render(&self) -> String {
        let mut out = String::from("<");
        out.push_str(&self.name);
        for attr in &self.attrs {
            out.push(' ');
            out.push_str(&attr.name);
            out.push('=');
            out.push(attr.quote);
            out.push_str(&attr.raw_value);
            out.push(attr.quote);
        }
        out.push_str(if self.self_closing { "/>" } else { ">" });
        out
    }
}

/// A child element of a rebuilt element.
#[derive(Clone, Debug)]
pub struct Part {
    pub key: String,
    /// Schema rank for known children. Unknown children use `u32::MAX` and keep their position.
    pub rank: u32,
    pub bytes: Vec<u8>,
}

/// Collects the children of `node` as raw parts. `rank_of` maps a key to its schema rank.
pub fn parts_of(tree: &Tree<'_>, node: Node<'_, '_>, rank_of: fn(&str) -> Option<u32>) -> Vec<Part> {
    tree.child_spans(node)
        .into_iter()
        .map(|(key, range)| Part {
            rank: rank_of(&key).unwrap_or(u32::MAX),
            bytes: tree.raw(&range).to_vec(),
            key,
        })
        .collect()
}

/// Sets a known child. The first existing child with this key is replaced in place and any further
/// duplicates are dropped. A missing child is inserted before the first known child whose rank is
/// greater; unknown children never move.
pub fn set_part(parts: &mut Vec<Part>, key: &str, rank: u32, bytes: Vec<u8>) {
    let mut pending = Some(bytes);
    let mut out: Vec<Part> = Vec::with_capacity(parts.len() + 1);
    for part in parts.drain(..) {
        if part.key == key {
            if let Some(b) = pending.take() {
                out.push(Part { key: key.to_string(), rank, bytes: b });
            }
        } else {
            out.push(part);
        }
    }
    if let Some(b) = pending {
        let position = out.iter().position(|p| p.rank != u32::MAX && p.rank > rank).unwrap_or(out.len());
        out.insert(position, Part { key: key.to_string(), rank, bytes: b });
    }
    *parts = out;
}

pub fn remove_part(parts: &mut Vec<Part>, key: &str) {
    parts.retain(|p| p.key != key);
}

pub fn has_part(parts: &[Part], key: &str) -> bool {
    parts.iter().any(|p| p.key == key)
}

/// Applies attribute edits to a child's start tag, keeping its content verbatim.
/// `None` removes the attribute; `Some(value)` sets it (value is unescaped text).
pub fn patch_part_attrs(part: &mut Part, edits: &[(&str, Option<String>)]) -> DltResult<()> {
    let start_end = start_tag_len(&part.bytes)?;
    let mut tag = StartTag::parse(&part.bytes[..start_end])?;
    for (name, value) in edits {
        match value {
            Some(v) => tag.set(name, v)?,
            None => tag.remove(name),
        }
    }
    let mut out = tag.render().into_bytes();
    out.extend_from_slice(&part.bytes[start_end..]);
    part.bytes = out;
    Ok(())
}

/// Length of the start tag at the beginning of `bytes`.
pub fn start_tag_len(bytes: &[u8]) -> DltResult<usize> {
    let mut quote: Option<u8> = None;
    for (i, &b) in bytes.iter().enumerate() {
        match quote {
            Some(q) => {
                if b == q {
                    quote = None;
                }
            }
            None => match b {
                b'"' | b'\'' => quote = Some(b),
                b'>' => return Ok(i + 1),
                _ => {}
            },
        }
    }
    Err(malformed("unterminated start tag"))
}

/// Builds an element from its start tag (`<name ...>` or `<name .../>`) and parts. The end tag uses
/// `end_name`. An element with no parts that was self-closing stays self-closing.
pub fn assemble(start_tag: &[u8], parts: &[Part], end_name: &str) -> Vec<u8> {
    let self_closing = start_tag.ends_with(b"/>");
    if parts.is_empty() && self_closing {
        return start_tag.to_vec();
    }
    let mut out = Vec::with_capacity(start_tag.len() + parts.iter().map(|p| p.bytes.len()).sum::<usize>() + 16);
    if self_closing {
        out.extend_from_slice(&start_tag[..start_tag.len() - 2]);
        out.push(b'>');
    } else {
        out.extend_from_slice(start_tag);
    }
    for part in parts {
        out.extend_from_slice(&part.bytes);
    }
    out.extend_from_slice(b"</");
    out.extend_from_slice(end_name.as_bytes());
    out.push(b'>');
    out
}

/// Qualified name of an element, read from the bytes at its start.
pub fn qname_at(bytes: &[u8], start: usize) -> DltResult<String> {
    bx::qname(bytes, start).map_err(|e| malformed(e.message))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn element_range_starts_at_the_tag_and_includes_the_end_tag() {
        let xml = b"<?xml version=\"1.0\"?><w:document xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\"><w:body><w:p><w:r/></w:p></w:body></w:document>";
        let tree = Tree::parse(xml).unwrap();
        let body = node_at(tree.root(), &[0]).unwrap();
        let span = tree.span(body);
        assert_eq!(&xml[span.clone()][..7], b"<w:body");
        assert!(xml[span].ends_with(b"</w:body>"));
    }

    #[test]
    fn start_tag_round_trips_and_edits_attributes() {
        let raw = b"<w:spacing w:before=\"120\" w:after='240' w:x=\"a&amp;b\"/>";
        let mut tag = StartTag::parse(raw).unwrap();
        assert!(tag.self_closing);
        assert_eq!(tag.get("w:after"), Some("240"));
        tag.set("w:before", "360").unwrap();
        tag.remove("w:after");
        tag.set("w:line", "240").unwrap();
        assert_eq!(tag.render(), "<w:spacing w:before=\"360\" w:x=\"a&amp;b\" w:line=\"240\"/>");
    }

    #[test]
    fn set_part_keeps_unknown_children_and_orders_known_ones() {
        let mut parts = vec![
            Part { key: "w:pStyle".into(), rank: 1, bytes: b"<w:pStyle w:val=\"A\"/>".to_vec() },
            Part { key: "x:ext".into(), rank: u32::MAX, bytes: b"<x:ext/>".to_vec() },
            Part { key: "w:jc".into(), rank: 27, bytes: b"<w:jc w:val=\"left\"/>".to_vec() },
        ];
        // spacing (rank 22) goes before jc (27); the unknown child keeps its place before jc.
        set_part(&mut parts, "w:spacing", 22, b"<w:spacing w:after=\"0\"/>".to_vec());
        let keys: Vec<&str> = parts.iter().map(|p| p.key.as_str()).collect();
        assert_eq!(keys, vec!["w:pStyle", "x:ext", "w:spacing", "w:jc"]);
        set_part(&mut parts, "w:jc", 27, b"<w:jc w:val=\"center\"/>".to_vec());
        assert_eq!(parts.iter().filter(|p| p.key == "w:jc").count(), 1);
        remove_part(&mut parts, "w:pStyle");
        let out = assemble(b"<w:pPr>", &parts, "w:pPr");
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "<w:pPr><x:ext/><w:spacing w:after=\"0\"/><w:jc w:val=\"center\"/></w:pPr>"
        );
    }

    #[test]
    fn empty_self_closing_element_stays_self_closing() {
        let out = assemble(b"<w:rPr/>", &[], "w:rPr");
        assert_eq!(out, b"<w:rPr/>".to_vec());
        let out = assemble(b"<w:rPr/>", &[Part { key: "w:b".into(), rank: 3, bytes: b"<w:b/>".to_vec() }], "w:rPr");
        assert_eq!(out, b"<w:rPr><w:b/></w:rPr>".to_vec());
    }
}
