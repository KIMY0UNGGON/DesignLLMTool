//! Node helpers over roxmltree, namespace scope capture, and wrapper-based patching of isolated fragments.
//!
//! Fragment patching exists because run and paragraph properties must be edited as bytes inside the
//! slide. A fragment copied out of a slide refers to prefixes declared on the slide root, so it is
//! parsed inside a wrapper that re-declares every in-scope namespace. Splices run on the wrapper, and
//! the wrapper is removed afterwards. The root start tag is fixed-length, so the fragment range is known.

use std::collections::BTreeSet;
use std::ops::Range;

use designllm_contracts::{DltError, DltResult, ErrorCode};
use designllm_package::xml::{self, Splice};
use roxmltree::{Document, Node};

pub type Elem<'a, 'i> = Node<'a, 'i>;

pub fn malformed(message: impl Into<String>) -> DltError {
    designllm_package::package::error(ErrorCode::InvalidArgument, "PPTX_MALFORMED", message)
}

/// True when the element has the given namespace URI and local name.
pub fn is_el(node: Elem<'_, '_>, ns: &str, local: &str) -> bool {
    node.is_element() && node.tag_name().namespace() == Some(ns) && node.tag_name().name() == local
}

/// First child element with the given namespace and local name.
pub fn child<'a, 'i>(node: Elem<'a, 'i>, ns: &str, local: &str) -> Option<Elem<'a, 'i>> {
    node.children().find(|c| is_el(*c, ns, local))
}

/// All child elements with the given namespace and local name, in document order.
pub fn children<'a, 'i>(node: Elem<'a, 'i>, ns: &str, local: &str) -> Vec<Elem<'a, 'i>> {
    node.children().filter(|c| is_el(*c, ns, local)).collect()
}

/// All child elements in document order.
pub fn elements<'a, 'i>(node: Elem<'a, 'i>) -> Vec<Elem<'a, 'i>> {
    node.children().filter(|c| c.is_element()).collect()
}

/// Concatenated character data of the direct text children (the shared `designllm_package` helper).
pub use designllm_package::xml::direct_text;

/// Integer attribute, when present and parseable.
pub fn attr_i64(node: Elem<'_, '_>, name: &str) -> Option<i64> {
    node.attribute(name).and_then(|v| v.trim().parse().ok())
}

/// Boolean attribute: `1`/`true` are true, `0`/`false` are false, anything else is absent.
pub fn bool_attr(node: Elem<'_, '_>, name: &str) -> Option<bool> {
    match node.attribute(name)? {
        "1" | "true" => Some(true),
        "0" | "false" => Some(false),
        _ => None,
    }
}

/// Namespace declarations in scope at `node`, rendered as attributes for a wrapper element.
/// The closest declaration of each prefix wins.
pub fn in_scope_declarations(node: Elem<'_, '_>) -> String {
    let mut seen: BTreeSet<Option<String>> = BTreeSet::new();
    let mut out = String::new();
    for ancestor in node.ancestors().filter(|n| n.is_element()) {
        for ns in ancestor.namespaces() {
            let prefix = ns.name().map(str::to_string);
            if seen.insert(prefix.clone()) {
                match &prefix {
                    Some(p) => out.push_str(&format!(" xmlns:{p}=\"{}\"", ns.uri())),
                    None => out.push_str(&format!(" xmlns=\"{}\"", ns.uri())),
                }
            }
        }
    }
    out
}

/// Parses `xml` (a whole part, with optional BOM) and returns the document plus the offset of its text.
pub fn parse(xml: &[u8]) -> DltResult<(Document<'_>, usize)> {
    let (text, offset) = xml::text_and_offset(xml)?;
    let doc = Document::parse(text).map_err(|e| malformed(format!("malformed XML: {e}")))?;
    Ok((doc, offset))
}

/// Absolute byte range of a node within the original part bytes.
pub fn span(node: Elem<'_, '_>, offset: usize) -> Range<usize> {
    let r = node.range();
    (r.start + offset)..(r.end + offset)
}

/// Applies `edit` to a fragment element in a wrapper that declares `decls`, then returns the patched
/// fragment bytes. `edit` receives the wrapper bytes and the range of the fragment inside them, and
/// returns splices on the wrapper. The fragment must be a single well-formed element.
pub fn patch_fragment(
    fragment: &[u8],
    decls: &str,
    edit: impl FnOnce(&[u8], Range<usize>) -> DltResult<Vec<Splice>>,
) -> DltResult<Vec<u8>> {
    let prefix = format!("<dlt:wrap xmlns:dlt=\"urn:designllm:fragment-wrapper\"{decls}>");
    let suffix = "</dlt:wrap>";
    let mut wrapper = Vec::with_capacity(prefix.len() + fragment.len() + suffix.len());
    wrapper.extend_from_slice(prefix.as_bytes());
    wrapper.extend_from_slice(fragment);
    wrapper.extend_from_slice(suffix.as_bytes());
    let fragment_range = prefix.len()..prefix.len() + fragment.len();
    // The fragment must be exactly one element, so the wrapper has one element child.
    {
        let (doc, _) = parse(&wrapper)?;
        let root = doc.root_element();
        let kids: Vec<_> = root.children().filter(|c| c.is_element()).collect();
        if kids.len() != 1 || root.children().any(|c| c.is_text() && !c.text().unwrap_or("").trim().is_empty()) {
            return Err(malformed("fragment is not a single element"));
        }
    }
    let splices = edit(&wrapper, fragment_range)?;
    let patched = xml::apply(&wrapper, splices)?;
    xml::check_well_formed(&patched)?;
    let end = patched.len() - suffix.len();
    Ok(patched[prefix.len()..end].to_vec())
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fragment_patch_keeps_prefixes_declared_on_the_slide_root() {
        let slide = br#"<p:sld xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:x1="urn:test"><a:rPr sz="1800"><x1:tag>keep</x1:tag></a:rPr></p:sld>"#;
        let (doc, offset) = parse(slide).unwrap();
        let rpr = doc.root_element().first_element_child().unwrap();
        let decls = in_scope_declarations(rpr);
        let fragment = &slide[span(rpr, offset)];
        let patched = patch_fragment(fragment, &decls, |wrapper, range| {
            let element = range.clone();
            Ok(vec![xml::set_attribute(wrapper, &element, "sz", "2400")?])
        })
        .unwrap();
        assert_eq!(patched, br#"<a:rPr sz="2400"><x1:tag>keep</x1:tag></a:rPr>"#.to_vec());
    }

    #[test]
    fn fragment_patch_refuses_more_than_one_element() {
        let decls = " xmlns:a=\"http://schemas.openxmlformats.org/drawingml/2006/main\"";
        let result = patch_fragment(b"<a:x/><a:y/>", decls, |_, _| Ok(vec![]));
        assert!(result.is_err());
    }

    #[test]
    fn bool_and_integer_attributes_parse_strictly() {
        let xml_bytes = br#"<a:rPr xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" b="1" i="0" u="maybe" sz="1800"/>"#;
        let (doc, _) = parse(xml_bytes).unwrap();
        let rpr = doc.root_element();
        assert_eq!(bool_attr(rpr, "b"), Some(true));
        assert_eq!(bool_attr(rpr, "i"), Some(false));
        assert_eq!(bool_attr(rpr, "u"), None);
        assert_eq!(attr_i64(rpr, "sz"), Some(1800));
        assert_eq!(attr_i64(rpr, "missing"), None);
    }
}
