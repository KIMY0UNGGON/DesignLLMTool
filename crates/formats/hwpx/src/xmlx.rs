//! Byte-span XML index and override renderer for HWPX parts.
//!
//! Every element gets a stable anchor: its qualified name plus a bracket with its `id` attribute
//! (or its cell address for `hp:tc`), a `#k` occurrence index among identical siblings, and the
//! anchor of its nearest non-transparent ancestor. Paragraph `id` values repeat in real files, so
//! the occurrence index is what keeps anchors unique. Transparent containers (`hp:run`, `hp:t`,
//! `hp:tr`) do not appear in anchors, so run splits do not move anchors of objects inside them.
//!
//! Writing renders the original bytes with targeted overrides. Only elements on the path to an
//! override are rebuilt; every other byte, including unknown subtrees and comments, is copied
//! verbatim from the source.
use std::collections::{HashMap, HashSet};

use roxmltree::{Document, Node, NodeId};

pub const NS_HS: &str = "http://www.hancom.co.kr/hwpml/2011/section";
pub const NS_HP: &str = "http://www.hancom.co.kr/hwpml/2011/paragraph";
pub const NS_HH: &str = "http://www.hancom.co.kr/hwpml/2011/head";
pub const NS_HC: &str = "http://www.hancom.co.kr/hwpml/2011/core";
pub const NS_OPF: &str = "http://www.idpf.org/2007/opf/";

/// Element names whose anchor is skipped; their children attach to the enclosing anchor.
const TRANSPARENT: &[&str] = &["hp:run", "hp:t", "hp:tr"];

/// A parsed part with stable anchors for every element and comment.
pub struct Xml<'a> {
    pub src: &'a str,
    pub doc: Document<'a>,
    keys: HashMap<NodeId, String>,
    by_key: HashMap<String, NodeId>,
}

impl<'a> Xml<'a> {
    pub fn parse(src: &'a str) -> Result<Self, String> {
        let doc = Document::parse(src).map_err(|e| format!("XML parse failed: {e}"))?;
        let pairs = compute_keys(&doc, src);
        let mut keys = HashMap::with_capacity(pairs.len());
        let mut by_key = HashMap::with_capacity(pairs.len());
        for (id, key) in pairs {
            by_key.insert(key.clone(), id);
            keys.insert(id, key);
        }
        let xml = Xml { src, doc, keys, by_key };
        // Span assumptions: element ranges start at the tag name, one byte after '<'.
        let root = xml.doc.root_element();
        let (lt, _) = xml.span(root);
        if !xml.src[lt..].starts_with('<') {
            return Err("unexpected element span layout".into());
        }
        Ok(xml)
    }

    /// Anchor of an element, comment, or processing instruction. Text nodes have none.
    pub fn key(&self, node: Node<'_, 'a>) -> Option<&str> {
        self.keys.get(&node.id()).map(String::as_str)
    }

    pub fn node(&self, key: &str) -> Option<Node<'_, 'a>> {
        self.by_key.get(key).and_then(|id| self.doc.get_node(*id))
    }

    /// Byte span of a node: `[start, end)`. For elements the start is the `<` of the start tag
    /// and the end is one past the close tag (or the `/>` of an empty element).
    pub fn span(&self, node: Node<'_, 'a>) -> (usize, usize) {
        let r = node.range();
        (r.start, r.end)
    }

    /// Source bytes of a node exactly as written.
    pub fn raw(&self, node: Node<'_, 'a>) -> &'a str {
        let (s, e) = self.span(node);
        &self.src[s..e]
    }

    /// Qualified name as written (`prefix:local`) of an element.
    pub fn qname(&self, node: Node<'_, 'a>) -> &'a str {
        qname_at(self.src, node.range().start + 1)
    }

    /// Index of the `>` that ends an element start tag.
    pub fn start_tag_end(&self, node: Node<'_, 'a>) -> usize {
        let (lt, _) = self.span(node);
        let mut pos = lt + 1 + self.qname(node).len();
        for attr in node.attributes() {
            pos = pos.max(attr.range().end);
        }
        match self.src[pos..].find('>') {
            Some(off) => pos + off,
            None => pos,
        }
    }

    pub fn is_self_closing(&self, node: Node<'_, 'a>) -> bool {
        let gt = self.start_tag_end(node);
        gt > 0 && self.src.as_bytes()[gt - 1] == b'/'
    }

    /// Byte offset of the close tag `</qname>` of a non-empty element.
    pub fn close_tag_start(&self, node: Node<'_, 'a>) -> Option<usize> {
        if self.is_self_closing(node) {
            return None;
        }
        let (_, end) = self.span(node);
        let gt = self.start_tag_end(node);
        self.src[gt + 1..end].rfind("</").map(|off| gt + 1 + off)
    }

    /// Element children with a given qualified name, in document order.
    pub fn children_named<'n>(&self, node: Node<'n, 'a>, qname: &str) -> Vec<Node<'n, 'a>> {
        node.children().filter(|c| c.is_element() && self.qname(*c) == qname).collect()
    }

    pub fn attr_i64(&self, node: Node<'_, 'a>, name: &str) -> Option<i64> {
        node.attribute(name)?.trim().parse().ok()
    }

    pub fn attr_u32(&self, node: Node<'_, 'a>, name: &str) -> Option<u32> {
        node.attribute(name)?.trim().parse().ok()
    }
}

/// Raw text of the namespace declarations written in an element's start tag (`xmlns="…"` and
/// `xmlns:prefix="…"`), for the start tag at `start..end` (from its `<` to its closing `>`). roxmltree
/// reports the namespaces in scope, not only the ones an element declares, so the bytes are read here.
pub fn declared_namespace_attributes(src: &str, start: usize, end: usize) -> Vec<String> {
    let tag = src.get(start..end).unwrap_or("");
    let bytes = tag.as_bytes();
    let is_space = |b: u8| matches!(b, b' ' | b'\t' | b'\r' | b'\n');
    let mut out = Vec::new();
    // Skip the `<` and the element name.
    let mut i = 1;
    while i < bytes.len() && !is_space(bytes[i]) && bytes[i] != b'/' {
        i += 1;
    }
    loop {
        while i < bytes.len() && is_space(bytes[i]) {
            i += 1;
        }
        let name_start = i;
        while i < bytes.len() && !is_space(bytes[i]) && !matches!(bytes[i], b'=' | b'/') {
            i += 1;
        }
        if i == name_start {
            break;
        }
        let name = &tag[name_start..i];
        // The value starts at the first quote after the name (skipping `=` and spaces).
        while i < bytes.len() && bytes[i] != b'"' && bytes[i] != b'\'' {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        let quote = bytes[i];
        i += 1;
        while i < bytes.len() && bytes[i] != quote {
            i += 1;
        }
        i = (i + 1).min(bytes.len());
        if name == "xmlns" || name.starts_with("xmlns:") {
            out.push(tag[name_start..i].to_string());
        }
    }
    out
}

/// Qualified name starting at `pos` in `src`.
pub fn qname_at(src: &str, pos: usize) -> &str {
    let rest = &src[pos..];
    let end = rest
        .find(|c: char| matches!(c, ' ' | '\t' | '\r' | '\n' | '/' | '>'))
        .unwrap_or(rest.len());
    &rest[..end]
}

fn segment(node: Node<'_, '_>, src: &str) -> String {
    let name = qname_at(src, node.range().start + 1).to_string();
    if let Some(id) = node.attribute("id") {
        return format!("{name}[id={id}]");
    }
    if name == "hp:tc" {
        if let Some(addr) = node.children().find(|c| c.is_element() && c.tag_name().name() == "cellAddr") {
            let col = addr.attribute("colAddr").unwrap_or("?");
            let row = addr.attribute("rowAddr").unwrap_or("?");
            return format!("{name}[c={col},r={row}]");
        }
    }
    name
}

fn occurrence(counts: &mut HashMap<String, u32>, seg: &str, base: &str) -> String {
    let full = if base.is_empty() { seg.to_string() } else { format!("{base}/{seg}") };
    let counter = counts.entry(full.clone()).or_insert(0);
    let key = format!("{full}#{counter}");
    *counter += 1;
    key
}

fn compute_keys(doc: &Document<'_>, src: &str) -> Vec<(NodeId, String)> {
    let mut out = Vec::new();
    let mut counts: HashMap<String, u32> = HashMap::new();
    let root = doc.root_element();
    let root_key = occurrence(&mut counts, &segment(root, src), "");
    out.push((root.id(), root_key.clone()));
    let mut transparent = 0usize;
    walk(root, &root_key, src, &mut counts, &mut out, &mut transparent);
    out
}

fn walk(
    node: Node<'_, '_>,
    base: &str,
    src: &str,
    counts: &mut HashMap<String, u32>,
    out: &mut Vec<(NodeId, String)>,
    transparent: &mut usize,
) {
    for child in node.children() {
        if child.is_element() {
            let name = qname_at(src, child.range().start + 1);
            if TRANSPARENT.contains(&name) {
                *transparent += 1;
                out.push((child.id(), format!("~{transparent}")));
                walk(child, base, src, counts, out, transparent);
            } else {
                let key = occurrence(counts, &segment(child, src), base);
                out.push((child.id(), key.clone()));
                walk(child, &key, src, counts, out, transparent);
            }
        } else if child.is_comment() {
            let key = occurrence(counts, "#comment", base);
            out.push((child.id(), key));
        } else if child.is_pi() {
            let key = occurrence(counts, "#pi", base);
            out.push((child.id(), key));
        }
    }
}

/// Replacement rules applied while rendering a part. Keys are anchors from [`Xml::key`].
#[derive(Clone, Debug, Default)]
pub struct Overrides {
    /// (anchor, unqualified attribute name) -> new unescaped value. Missing attributes are added.
    pub attrs: HashMap<(String, String), String>,
    /// Anchors that are not written.
    pub drop: HashSet<String>,
    /// Paragraph anchors whose inline content (`hp:run` children) is rewritten from these runs.
    pub regen: HashMap<String, Vec<InlineRun>>,
    /// Raw bytes written immediately before an element.
    pub before: HashMap<String, Vec<Vec<u8>>>,
    /// Raw bytes written immediately after an element.
    pub after: HashMap<String, Vec<Vec<u8>>>,
    /// Raw bytes written immediately after the start tag of an element.
    pub prepend: HashMap<String, Vec<Vec<u8>>>,
    /// Raw bytes written before the close tag of an element.
    pub append: HashMap<String, Vec<Vec<u8>>>,
}

impl Overrides {
    pub fn set_attr(&mut self, key: &str, name: &str, value: impl Into<String>) {
        self.attrs.insert((key.to_string(), name.to_string()), value.into());
    }

    pub fn is_empty(&self) -> bool {
        self.attrs.is_empty()
            && self.drop.is_empty()
            && self.regen.is_empty()
            && self.before.is_empty()
            && self.after.is_empty()
            && self.prepend.is_empty()
            && self.append.is_empty()
    }

    /// True when the element itself (not a descendant) has an override.
    pub fn has_own_override(&self, key: &str) -> bool {
        self.attrs.keys().any(|(k, _)| k == key)
            || self.drop.contains(key)
            || self.regen.contains_key(key)
            || self.before.contains_key(key)
            || self.after.contains_key(key)
            || self.prepend.contains_key(key)
            || self.append.contains_key(key)
    }

    fn override_keys(&self) -> Vec<&str> {
        let mut keys: Vec<&str> = Vec::new();
        keys.extend(self.attrs.keys().map(|(k, _)| k.as_str()));
        keys.extend(self.drop.iter().map(String::as_str));
        keys.extend(self.regen.keys().map(String::as_str));
        keys.extend(self.before.keys().map(String::as_str));
        keys.extend(self.after.keys().map(String::as_str));
        keys.extend(self.prepend.keys().map(String::as_str));
        keys.extend(self.append.keys().map(String::as_str));
        keys
    }
}

/// One item of rewritten inline content, in document order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Item {
    /// Plain text; consecutive text and `TAtom` items share one `hp:t` element.
    Text(String),
    /// A child rendered inside `hp:t` (tab, line break, or an in-text marker). Anchor key.
    TAtom(String),
    /// A child rendered directly inside `hp:run` (picture, control, table, floating object). Anchor key.
    RAtom(String),
    /// Raw XML bytes rendered directly inside `hp:run` (used when relocating section head properties).
    Raw(Vec<u8>),
}

/// One rewritten `hp:run`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InlineRun {
    pub char_pr: u32,
    pub items: Vec<Item>,
}

pub fn esc_text(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

pub fn esc_attr(s: &str) -> String {
    esc_text(s).replace('"', "&quot;").replace('\n', "&#10;").replace('\r', "&#13;").replace('\t', "&#9;")
}

/// Renders a whole part with overrides applied.
///
/// Each changed child of the root is rendered into one replacement of its byte range. The shared
/// package helper applies those replacements to the source and checks that the result is
/// well-formed; every other byte is copied from the source. A change to the root element itself
/// replaces the root's range.
pub fn render_part(xml: &Xml<'_>, ov: &Overrides) -> Result<Vec<u8>, String> {
    let dirty = dirty_set(xml, ov)?;
    let root = xml.doc.root_element();
    let root_key = xml.key(root).unwrap_or_default().to_string();
    let source = xml.src.as_bytes();
    if !dirty.contains(&root_key) {
        return Ok(source.to_vec());
    }
    let root_changed = ov.has_own_override(&root_key);
    let mut splices = Vec::new();
    if root_changed {
        let mut buffer = Vec::new();
        let mut r = Renderer { xml, ov, dirty: &dirty, out: &mut buffer };
        r.render(root)?;
        splices.push(designllm_package::xml::Splice { range: xml.span(root).0..xml.span(root).1, replacement: buffer });
    } else {
        for child in root.children() {
            let Some(key) = xml.key(child) else { continue };
            if !dirty.contains(key) {
                continue;
            }
            let mut buffer = Vec::new();
            let mut r = Renderer { xml, ov, dirty: &dirty, out: &mut buffer };
            r.render(child)?;
            let (start, end) = xml.span(child);
            splices.push(designllm_package::xml::Splice { range: start..end, replacement: buffer });
        }
    }
    designllm_package::xml::apply_checked(source, splices)
        .map_err(|e| format!("rendered part is not well-formed: {}", e.message))
}

/// Renders one element (by anchor) with overrides, without any surrounding document bytes.
pub fn render_key(xml: &Xml<'_>, key: &str, ov: &Overrides) -> Result<Vec<u8>, String> {
    let dirty = dirty_set(xml, ov)?;
    let node = xml.node(key).ok_or_else(|| format!("no element with anchor {key}"))?;
    let mut out = Vec::new();
    let mut r = Renderer { xml, ov, dirty: &dirty, out: &mut out };
    r.render(node)?;
    Ok(out)
}

fn dirty_set(xml: &Xml<'_>, ov: &Overrides) -> Result<HashSet<String>, String> {
    let mut dirty = HashSet::new();
    for key in ov.override_keys() {
        let node = xml.node(key).ok_or_else(|| format!("override targets unknown anchor {key}"))?;
        let mut current = Some(node);
        while let Some(n) = current {
            if let Some(k) = xml.key(n) {
                if !dirty.insert(k.to_string()) {
                    break;
                }
            }
            current = n.parent();
        }
    }
    Ok(dirty)
}

struct Renderer<'r, 'a> {
    xml: &'r Xml<'a>,
    ov: &'r Overrides,
    dirty: &'r HashSet<String>,
    out: &'r mut Vec<u8>,
}

impl Renderer<'_, '_> {
    fn render(&mut self, node: Node<'_, '_>) -> Result<(), String> {
        if !node.is_element() && !node.is_comment() && !node.is_pi() {
            // Text nodes are copied exactly.
            self.out.extend_from_slice(self.xml.raw(node).as_bytes());
            return Ok(());
        }
        let key = self.xml.key(node).map(str::to_string).unwrap_or_default();
        if self.ov.drop.contains(&key) {
            return Ok(());
        }
        if let Some(parts) = self.ov.before.get(&key) {
            for bytes in parts {
                self.out.extend_from_slice(bytes);
            }
        }
        if node.is_element() {
            if let Some(runs) = self.ov.regen.get(&key) {
                self.render_regen(node, &key, runs)?;
            } else if self.dirty.contains(&key) {
                self.render_dirty(node, &key)?;
            } else {
                self.out.extend_from_slice(self.xml.raw(node).as_bytes());
            }
        } else {
            self.out.extend_from_slice(self.xml.raw(node).as_bytes());
        }
        if let Some(parts) = self.ov.after.get(&key) {
            for bytes in parts {
                self.out.extend_from_slice(bytes);
            }
        }
        Ok(())
    }

    fn start_tag(&mut self, node: Node<'_, '_>, key: &str) -> Result<bool, String> {
        let (lt, _) = self.xml.span(node);
        let gt = self.xml.start_tag_end(node);
        let mut edits: Vec<(usize, usize, String)> = Vec::new();
        let mut present: HashSet<&str> = HashSet::new();
        for attr in node.attributes() {
            present.insert(attr.name());
            if let Some(v) = self.ov.attrs.get(&(key.to_string(), attr.name().to_string())) {
                let r = attr.range_value();
                edits.push((r.start, r.end, esc_attr(v)));
            }
        }
        let mut missing: Vec<(&String, &String)> = self
            .ov
            .attrs
            .iter()
            .filter(|((k, name), _)| k.as_str() == key && !present.contains(name.as_str()))
            .map(|((_, name), v)| (name, v))
            .collect();
        missing.sort();
        if !missing.is_empty() {
            let insert_at = match node.attributes().last() {
                Some(last) => last.range_value().end + 1,
                None => lt + 1 + self.xml.qname(node).len(),
            };
            let mut text = String::new();
            for (name, value) in missing {
                text.push_str(&format!(" {name}=\"{}\"", esc_attr(value)));
            }
            edits.push((insert_at, insert_at, text));
        }
        edits.sort_by_key(|e| (e.0, e.1));
        let mut cursor = lt;
        for (start, end, text) in edits {
            if start < cursor {
                return Err(format!("overlapping attribute edits on {key}"));
            }
            self.out.extend_from_slice(self.xml.src[cursor..start].as_bytes());
            self.out.extend_from_slice(text.as_bytes());
            cursor = end;
        }
        let self_closing = self.xml.is_self_closing(node);
        if self_closing {
            // Everything up to the `/` of `/>`.
            self.out.extend_from_slice(self.xml.src[cursor..gt - 1].as_bytes());
        } else {
            self.out.extend_from_slice(self.xml.src[cursor..=gt].as_bytes());
        }
        Ok(self_closing)
    }

    fn render_dirty(&mut self, node: Node<'_, '_>, key: &str) -> Result<(), String> {
        let self_closing = self.start_tag(node, key)?;
        let has_prepends = self.ov.prepend.get(key).is_some_and(|p| !p.is_empty());
        let has_appends = self.ov.append.get(key).is_some_and(|a| !a.is_empty());
        if self_closing {
            if !has_prepends && !has_appends {
                self.out.extend_from_slice(b"/>");
                return Ok(());
            }
            self.out.push(b'>');
            self.write_parts(&self.ov.prepend, key);
            self.write_parts(&self.ov.append, key);
            let name = self.xml.qname(node);
            self.out.extend_from_slice(format!("</{name}>").as_bytes());
            return Ok(());
        }
        // `start_tag` already wrote the `>` of a non-empty start tag.
        self.write_parts(&self.ov.prepend, key);
        for child in node.children() {
            self.render(child)?;
        }
        self.write_parts(&self.ov.append, key);
        if let Some(close) = self.xml.close_tag_start(node) {
            let (_, end) = self.xml.span(node);
            self.out.extend_from_slice(self.xml.src[close..end].as_bytes());
        }
        Ok(())
    }

    fn write_parts(&mut self, map: &HashMap<String, Vec<Vec<u8>>>, key: &str) {
        if let Some(parts) = map.get(key) {
            for bytes in parts {
                self.out.extend_from_slice(bytes);
            }
        }
    }

    fn render_regen(&mut self, node: Node<'_, '_>, key: &str, runs: &[InlineRun]) -> Result<(), String> {
        let self_closing = self.start_tag(node, key)?;
        let qname = self.xml.qname(node);
        let prefix = qname.split_once(':').map(|(p, _)| p).unwrap_or("");
        let run_name = if prefix.is_empty() { "run".to_string() } else { format!("{prefix}:run") };
        let t_name = if prefix.is_empty() { "t".to_string() } else { format!("{prefix}:t") };
        if self_closing {
            // A self-closing paragraph gains content, so it becomes an open/close pair.
            self.out.push(b'>');
        }
        for run in runs {
            self.out.extend_from_slice(format!("<{run_name} charPrIDRef=\"{}\">", run.char_pr).as_bytes());
            let mut t_open = false;
            for item in &run.items {
                match item {
                    Item::Text(text) => {
                        if !t_open {
                            self.out.extend_from_slice(format!("<{t_name}>").as_bytes());
                            t_open = true;
                        }
                        self.out.extend_from_slice(esc_text(text).as_bytes());
                    }
                    Item::TAtom(atom) => {
                        if !t_open {
                            self.out.extend_from_slice(format!("<{t_name}>").as_bytes());
                            t_open = true;
                        }
                        self.render_by_key(atom)?;
                    }
                    Item::RAtom(atom) => {
                        if t_open {
                            self.out.extend_from_slice(format!("</{t_name}>").as_bytes());
                            t_open = false;
                        }
                        self.render_by_key(atom)?;
                    }
                    Item::Raw(bytes) => {
                        if t_open {
                            self.out.extend_from_slice(format!("</{t_name}>").as_bytes());
                            t_open = false;
                        }
                        self.out.extend_from_slice(bytes);
                    }
                }
            }
            if t_open {
                self.out.extend_from_slice(format!("</{t_name}>").as_bytes());
            }
            self.out.extend_from_slice(format!("</{run_name}>").as_bytes());
        }
        for child in node.children() {
            if child.is_element() && self.xml.qname(child) == run_name {
                continue;
            }
            self.render(child)?;
        }
        self.write_parts(&self.ov.append, key);
        self.out.extend_from_slice(format!("</{qname}>").as_bytes());
        Ok(())
    }

    fn render_by_key(&mut self, key: &str) -> Result<(), String> {
        let node = self.xml.node(key).ok_or_else(|| format!("inline item refers to unknown anchor {key}"))?;
        self.render(node)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"<?xml version="1.0" encoding="UTF-8"?><hs:sec xmlns:hs="http://www.hancom.co.kr/hwpml/2011/section" xmlns:hp="http://www.hancom.co.kr/hwpml/2011/paragraph" xmlns:hx="urn:x"><hp:p id="0" paraPrIDRef="1"><hp:run charPrIDRef="0"><hp:t>a &amp; b</hp:t></hp:run><hx:node k="v"/><!-- c --></hp:p><hp:p id="0"><hp:run charPrIDRef="2"><hp:t>x</hp:t></hp:run></hp:p></hs:sec>"#;

    #[test]
    fn spans_and_anchors_are_exact() {
        let xml = Xml::parse(SAMPLE).expect("parse");
        let root = xml.doc.root_element();
        assert_eq!(xml.key(root), Some("hs:sec#0"));
        let paras = xml.children_named(root, "hp:p");
        assert_eq!(xml.key(paras[0]), Some("hs:sec#0/hp:p[id=0]#0"));
        assert_eq!(xml.key(paras[1]), Some("hs:sec#0/hp:p[id=0]#1"));
        assert!(xml.raw(paras[0]).starts_with("<hp:p "));
        assert!(xml.raw(paras[0]).ends_with("</hp:p>"));
        assert!(xml.is_self_closing(xml.node("hs:sec#0/hp:p[id=0]#0/hx:node#0").expect("node")));
    }

    #[test]
    fn identity_render_reproduces_source_bytes() {
        let xml = Xml::parse(SAMPLE).expect("parse");
        let out = render_part(&xml, &Overrides::default()).expect("render");
        assert_eq!(out, SAMPLE.as_bytes());
    }

    #[test]
    fn attribute_override_changes_only_the_value() {
        let xml = Xml::parse(SAMPLE).expect("parse");
        let mut ov = Overrides::default();
        ov.set_attr("hs:sec#0/hp:p[id=0]#1", "paraPrIDRef", "7");
        let out = String::from_utf8(render_part(&xml, &ov).expect("render")).expect("utf8");
        assert_eq!(out, SAMPLE.replace(r#"<hp:p id="0"><hp:run charPrIDRef="2">"#, r#"<hp:p id="0" paraPrIDRef="7"><hp:run charPrIDRef="2">"#));
    }

    #[test]
    fn regen_rewrites_runs_and_keeps_unknown_children() {
        let xml = Xml::parse(SAMPLE).expect("parse");
        let mut ov = Overrides::default();
        let key = "hs:sec#0/hp:p[id=0]#0".to_string();
        ov.regen.insert(
            key.clone(),
            vec![InlineRun { char_pr: 3, items: vec![Item::Text("new <x>".into())] }],
        );
        let out = String::from_utf8(render_part(&xml, &ov).expect("render")).expect("utf8");
        assert!(out.contains(r#"<hp:p id="0" paraPrIDRef="1"><hp:run charPrIDRef="3"><hp:t>new &lt;x&gt;</hp:t></hp:run><hx:node k="v"/><!-- c --></hp:p>"#), "{out}");
    }

    #[test]
    fn unknown_anchor_is_an_error() {
        let xml = Xml::parse(SAMPLE).expect("parse");
        let mut ov = Overrides::default();
        ov.set_attr("nope#0", "a", "b");
        assert!(render_part(&xml, &ov).is_err());
    }

    #[test]
    fn append_and_after_are_written_in_order() {
        let xml = Xml::parse(SAMPLE).expect("parse");
        let mut ov = Overrides::default();
        ov.append.insert("hs:sec#0".into(), vec![b"<!--A-->".to_vec()]);
        ov.after.insert("hs:sec#0/hp:p[id=0]#0".into(), vec![b"<hp:p id=\"9\"/>".to_vec()]);
        let out = String::from_utf8(render_part(&xml, &ov).expect("render")).expect("utf8");
        assert!(out.contains("</hp:p><hp:p id=\"9\"/><hp:p id=\"0\">"), "{out}");
        assert!(out.ends_with("<!--A--></hs:sec>"), "{out}");
    }
}
