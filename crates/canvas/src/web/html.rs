//! Minimal HTML tag scanner for WebCode. It locates tags, attributes, and element bodies, and nothing more.
//! Comments and declarations are skipped. `script` and `style` bodies are raw text and never scanned for tags.
//! Entity decoding, CDATA, and `<template>` are not supported. Anything the scanner cannot resolve is an error,
//! never a guess.

use crate::common::{invalid, unsupported};
use designllm_contracts::{DltResult, WebFile};
use std::collections::BTreeMap;

pub(crate) const MAP_ATTR: &str = "data-dlm-map";
const VOID: [&str; 14] = [
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source", "track", "wbr",
];

#[derive(Debug, Clone)]
pub(crate) struct Attr {
    pub name: String,
    pub value: String,
    /// Byte range of the value (inside the quotes when quoted).
    pub vstart: usize,
    pub vend: usize,
    pub quote: Option<u8>,
}

#[derive(Debug, Clone)]
pub(crate) struct Tag {
    pub start: usize,
    pub end: usize,
    pub name: String,
    pub closing: bool,
    pub self_closing: bool,
    pub attrs: Vec<Attr>,
}

impl Tag {
    pub(crate) fn attr(&self, name: &str) -> Option<&Attr> {
        self.attrs.iter().find(|a| a.name == name)
    }
}

fn malformed(message: &str) -> designllm_contracts::DltError {
    unsupported("WEB_HTML_UNSUPPORTED", format!("HTML cannot be scanned: {message}"))
}

/// Scans start and end tags. Quoted attribute values may contain `>`. Raw-text bodies are skipped.
pub(crate) fn tokenize(html: &str) -> DltResult<Vec<Tag>> {
    let b = html.as_bytes();
    let mut tags = Vec::new();
    let mut i = 0usize;
    while let Some(off) = html[i..].find('<') {
        let p = i + off;
        if html[p..].starts_with("<!--") {
            let close = html[p + 4..].find("-->").ok_or_else(|| malformed("unterminated comment"))?;
            i = p + 4 + close + 3;
            continue;
        }
        if html[p..].starts_with("<!") || html[p..].starts_with("<?") {
            let gt = html[p..].find('>').ok_or_else(|| malformed("unterminated declaration"))?;
            i = p + gt + 1;
            continue;
        }
        if html[p..].starts_with("</") {
            let name_start = p + 2;
            let mut q = name_start;
            while q < b.len() && !b[q].is_ascii_whitespace() && b[q] != b'>' {
                q += 1;
            }
            let name = html[name_start..q].to_ascii_lowercase();
            let gt = html[q..].find('>').ok_or_else(|| malformed("unterminated end tag"))?;
            let end = q + gt + 1;
            tags.push(Tag { start: p, end, name, closing: true, self_closing: false, attrs: Vec::new() });
            i = end;
            continue;
        }
        if p + 1 < b.len() && b[p + 1].is_ascii_alphabetic() {
            let mut q = p + 1;
            while q < b.len() && !b[q].is_ascii_whitespace() && b[q] != b'>' && b[q] != b'/' {
                q += 1;
            }
            let name = html[p + 1..q].to_ascii_lowercase();
            let mut attrs = Vec::new();
            let mut self_closing = false;
            loop {
                while q < b.len() && b[q].is_ascii_whitespace() {
                    q += 1;
                }
                if q >= b.len() {
                    return Err(malformed("unterminated start tag"));
                }
                if b[q] == b'/' {
                    if b.get(q + 1) == Some(&b'>') {
                        self_closing = true;
                    }
                    q += 1;
                    continue;
                }
                if b[q] == b'>' {
                    q += 1;
                    break;
                }
                let an_start = q;
                while q < b.len() && !b[q].is_ascii_whitespace() && !matches!(b[q], b'=' | b'>' | b'/') {
                    q += 1;
                }
                let aname = html[an_start..q].to_ascii_lowercase();
                let mut j = q;
                while j < b.len() && b[j].is_ascii_whitespace() {
                    j += 1;
                }
                if j < b.len() && b[j] == b'=' {
                    j += 1;
                    while j < b.len() && b[j].is_ascii_whitespace() {
                        j += 1;
                    }
                    if j < b.len() && (b[j] == b'"' || b[j] == b'\'') {
                        let quote = b[j];
                        let vs = j + 1;
                        let rel = b[vs..].iter().position(|c| *c == quote).ok_or_else(|| malformed("unterminated attribute"))?;
                        let ve = vs + rel;
                        attrs.push(Attr {
                            name: aname,
                            value: html[vs..ve].to_string(),
                            vstart: vs,
                            vend: ve,
                            quote: Some(quote),
                        });
                        q = ve + 1;
                    } else {
                        let vs = j;
                        while j < b.len() && !b[j].is_ascii_whitespace() && b[j] != b'>' {
                            j += 1;
                        }
                        attrs.push(Attr { name: aname, value: html[vs..j].to_string(), vstart: vs, vend: j, quote: None });
                        q = j;
                    }
                } else {
                    attrs.push(Attr { name: aname, value: String::new(), vstart: q, vend: q, quote: None });
                }
            }
            let end = q;
            let raw = (name == "script" || name == "style") && !self_closing;
            tags.push(Tag { start: p, end, name: name.clone(), closing: false, self_closing, attrs });
            if raw {
                let lower = html[end..].to_ascii_lowercase();
                let rel = lower.find(&format!("</{name}")).ok_or_else(|| malformed("unterminated raw text element"))?;
                i = end + rel;
            } else {
                i = end;
            }
            continue;
        }
        i = p + 1;
    }
    Ok(tags)
}

/// Index of the start tags that carry `data-dlm-map="map_id"`. Exactly one is required by the callers.
pub(crate) fn mapping_hits(tags: &[Tag], map_id: &str) -> Vec<usize> {
    tags.iter()
        .enumerate()
        .filter(|(_, t)| !t.closing && t.attrs.iter().any(|a| a.name == MAP_ATTR && a.value == map_id))
        .map(|(i, _)| i)
        .collect()
}

/// Mapping IDs in document order. Duplicates are reported as they are.
pub(crate) fn mapping_ids(html: &str) -> DltResult<Vec<(String, String)>> {
    let tags = tokenize(html)?;
    Ok(tags
        .iter()
        .filter(|t| !t.closing)
        .filter_map(|t| t.attr(MAP_ATTR).map(|a| (a.value.clone(), t.name.clone())))
        .collect())
}

/// What a mapped edit changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Property {
    /// The whole text content of the element. Only plain-text elements qualify.
    Text,
    /// The whole value of one attribute.
    Attr(String),
}

/// Escapes text for element content: `&`, `<`, `>`.
pub(crate) fn escape_text(value: &str) -> String {
    value.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// Escapes an attribute value for a value delimited by `quote`.
pub(crate) fn escape_attr(value: &str, quote: u8) -> String {
    let mut out = value.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
    out = if quote == b'\'' { out.replace('\'', "&#39;") } else { out.replace('"', "&quot;") };
    out
}

/// Replaces the whole text or one attribute value of the single element that carries `data-dlm-map="map_id"`.
/// The element must be unique. Text targets must hold text only. The mapping key itself cannot change.
pub(crate) fn mapped_edit(html: &str, map_id: &str, property: &Property, value: &str) -> DltResult<String> {
    let tags = tokenize(html)?;
    let hits = mapping_hits(&tags, map_id);
    let si = match hits.len() {
        0 => return Err(crate::common::fail(designllm_contracts::ErrorCode::NotFound, "WEB_MAPPING_NOT_FOUND", format!("mapping {map_id} does not exist"))),
        1 => hits[0],
        n => {
            return Err(crate::common::fail(
                designllm_contracts::ErrorCode::InvalidArgument,
                "WEB_MAPPING_AMBIGUOUS",
                format!("mapping {map_id} occurs {n} times"),
            ))
        }
    };
    let start_tag = &tags[si];
    match property {
        Property::Attr(name) => {
            let key = name.to_ascii_lowercase();
            if key == MAP_ATTR {
                return Err(invalid("WEB_MAPPING_KEY", "the mapping attribute cannot be edited"));
            }
            let attr = start_tag
                .attr(&key)
                .ok_or_else(|| crate::common::fail(designllm_contracts::ErrorCode::NotFound, "WEB_ATTR_MISSING", format!("attribute {key} is not on mapping {map_id}")))?;
            match attr.quote {
                Some(quote) => {
                    let escaped = escape_attr(value, quote);
                    Ok(format!("{}{}{}", &html[..attr.vstart], escaped, &html[attr.vend..]))
                }
                None => {
                    // A boolean attribute has no `=`, so it has no value to replace. Writing after its name would rename it.
                    let had_equals = html[..attr.vstart].trim_end().ends_with('=');
                    if attr.vstart == attr.vend && !had_equals {
                        return Err(invalid("WEB_ATTR_NO_VALUE", "a boolean attribute has no value to replace"));
                    }
                    // An unquoted value (or an empty one after `=`) becomes a double-quoted value. It is escaped as every quoted
                    // value is, so the browser reads the stored text as written, and no character of the new value needs refusing.
                    let escaped = escape_attr(value, b'"');
                    Ok(format!("{}\"{}\"{}", &html[..attr.vstart], escaped, &html[attr.vend..]))
                }
            }
        }
        Property::Text => {
            let name = start_tag.name.as_str();
            if start_tag.self_closing || VOID.contains(&name) {
                return Err(unsupported("WEB_MAP_VOID", "a void or self-closing element has no text"));
            }
            if name == "script" || name == "style" {
                return Err(unsupported("WEB_MAP_RAW_TEXT", "script and style bodies are not edited through mappings"));
            }
            let close = tags
                .get(si + 1)
                .filter(|t| t.closing && t.name == name)
                .ok_or_else(|| unsupported("WEB_MAP_CHILDREN", "the element has child markup, so its text is not a single literal"))?;
            let content = &html[start_tag.end..close.start];
            if content.contains('<') {
                return Err(unsupported("WEB_MAP_CHILDREN", "the element body contains markup"));
            }
            let escaped = escape_text(value);
            Ok(format!("{}{}{}", &html[..start_tag.end], escaped, &html[close.start..]))
        }
    }
}

/// Resolves an `href` or `src` value to a project path, or None when it is external or outside the project.
pub(crate) fn local_path(entry: &str, reference: &str) -> Option<String> {
    let trimmed = reference.split(['?', '#']).next().unwrap_or("");
    if trimmed.is_empty()
        || trimmed.starts_with("//")
        || trimmed.starts_with('/')
        || trimmed.contains(':')
        || trimmed.contains('\\')
    {
        return None;
    }
    let dir = match entry.rfind('/') {
        Some(pos) => &entry[..pos],
        None => "",
    };
    let mut parts: Vec<&str> = if dir.is_empty() { Vec::new() } else { dir.split('/').collect() };
    for segment in trimmed.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            other => parts.push(other),
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("/"))
    }
}

pub(crate) fn is_external(reference: &str) -> bool {
    let lower = reference.trim_start().to_ascii_lowercase();
    lower.starts_with("http:") || lower.starts_with("https:") || lower.starts_with("//") || lower.starts_with("data:")
        || lower.starts_with("mailto:") || lower.starts_with("blob:")
}

/// Report of local resource references inside an HTML entry.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct ResourceReport {
    pub external: Vec<String>,
    pub missing: Vec<String>,
}

pub(crate) fn resource_report(entry: &str, html: &str, files: &BTreeMap<String, WebFile>) -> DltResult<ResourceReport> {
    let tags = tokenize(html)?;
    let mut report = ResourceReport::default();
    for tag in tags.iter().filter(|t| !t.closing) {
        let reference = match tag.name.as_str() {
            "link" if tag.attr("rel").is_some_and(|a| a.value.eq_ignore_ascii_case("stylesheet")) => tag.attr("href"),
            "script" => tag.attr("src"),
            "img" => tag.attr("src"),
            _ => None,
        };
        let Some(attr) = reference else { continue };
        if is_external(&attr.value) {
            report.external.push(attr.value.clone());
        } else if let Some(path) = local_path(entry, &attr.value) {
            if !files.contains_key(&path) {
                report.missing.push(path);
            }
        } else {
            report.missing.push(attr.value.clone());
        }
    }
    Ok(report)
}

/// Result of inlining local stylesheets and scripts into the entry for a single-file HTML export.
#[derive(Debug, Default)]
pub(crate) struct Inlined {
    pub html: String,
    pub inlined: Vec<String>,
}

/// Replaces `<link rel=stylesheet href=local>` with `<style>` and `<script src=local></script>` with an inline script.
/// Content containing a closing tag of the same kind is refused, because it would end the element early.
pub(crate) fn inline_local(entry: &str, html: &str, files: &BTreeMap<String, WebFile>) -> DltResult<Inlined> {
    let tags = tokenize(html)?;
    let mut replacements: Vec<(usize, usize, String, String)> = Vec::new();
    for (i, tag) in tags.iter().enumerate() {
        if tag.closing {
            continue;
        }
        if tag.name == "link" && tag.attr("rel").is_some_and(|a| a.value.eq_ignore_ascii_case("stylesheet")) {
            let Some(href) = tag.attr("href") else { continue };
            if is_external(&href.value) {
                continue;
            }
            let Some(path) = local_path(entry, &href.value) else { continue };
            let Some(file) = files.get(&path) else { continue };
            if file.content.to_ascii_lowercase().contains("</style") {
                return Err(crate::common::fail(
                    designllm_contracts::ErrorCode::UnsafeRoundtrip,
                    "WEB_INLINE_UNSAFE",
                    format!("{path} contains a closing style tag and cannot be inlined"),
                ));
            }
            replacements.push((tag.start, tag.end, format!("<style>\n{}</style>", file.content), path));
        } else if tag.name == "script" {
            let Some(src) = tag.attr("src") else { continue };
            if is_external(&src.value) {
                continue;
            }
            let Some(path) = local_path(entry, &src.value) else { continue };
            let Some(file) = files.get(&path) else { continue };
            let close = tags
                .get(i + 1)
                .filter(|t| t.closing && t.name == "script")
                .ok_or_else(|| unsupported("WEB_HTML_UNSUPPORTED", "script element without a closing tag"))?;
            if file.content.to_ascii_lowercase().contains("</script") {
                return Err(crate::common::fail(
                    designllm_contracts::ErrorCode::UnsafeRoundtrip,
                    "WEB_INLINE_UNSAFE",
                    format!("{path} contains a closing script tag and cannot be inlined"),
                ));
            }
            replacements.push((tag.start, close.end, format!("<script>\n{}</script>", file.content), path));
        }
    }
    let mut out = String::with_capacity(html.len());
    let mut cursor = 0usize;
    let mut inlined = Vec::new();
    for (start, end, text, path) in replacements {
        out.push_str(&html[cursor..start]);
        out.push_str(&text);
        cursor = end;
        inlined.push(path);
    }
    out.push_str(&html[cursor..]);
    Ok(Inlined { html: out, inlined })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mapped_text_replaces_the_whole_plain_text_and_escapes_markup() {
        let html = "<html><h1 data-dlm-map=\"title\">Old</h1></html>";
        let out = mapped_edit(html, "title", &Property::Text, "A < B & C").unwrap();
        assert_eq!(out, "<html><h1 data-dlm-map=\"title\">A &lt; B &amp; C</h1></html>");
    }

    #[test]
    fn mapped_attribute_replaces_the_value_with_escaping() {
        let html = "<a data-dlm-map=\"link\" href=\"old.html\">go</a>";
        let out = mapped_edit(html, "link", &Property::Attr("href".into()), "new\"x.html").unwrap();
        assert_eq!(out, "<a data-dlm-map=\"link\" href=\"new&quot;x.html\">go</a>");
    }

    #[test]
    fn duplicate_and_nested_mappings_are_refused() {
        let dup = "<p data-dlm-map=\"x\">a</p><p data-dlm-map=\"x\">b</p>";
        assert_eq!(mapped_edit(dup, "x", &Property::Text, "c").unwrap_err().code, designllm_contracts::ErrorCode::InvalidArgument);
        let nested = "<div data-dlm-map=\"x\"><b>bold</b></div>";
        assert_eq!(mapped_edit(nested, "x", &Property::Text, "c").unwrap_err().diagnostics[0].code, "WEB_MAP_CHILDREN");
    }

    #[test]
    fn mapping_inside_comment_or_script_is_ignored() {
        let html = "<!-- <p data-dlm-map=\"z\">a</p> --><script>var s = '<p data-dlm-map=\"z\">';</script>";
        assert_eq!(
            mapped_edit(html, "z", &Property::Text, "b").unwrap_err().code,
            designllm_contracts::ErrorCode::NotFound
        );
    }

    #[test]
    fn quoted_greater_than_in_attribute_does_not_end_the_tag() {
        let html = "<p title=\"a > b\" data-dlm-map=\"q\">x</p>";
        assert_eq!(mapped_edit(html, "q", &Property::Text, "y").unwrap(), "<p title=\"a > b\" data-dlm-map=\"q\">y</p>");
    }

    #[test]
    fn local_paths_resolve_relative_to_the_entry_and_stay_inside_the_project() {
        assert_eq!(local_path("index.html", "styles.css").as_deref(), Some("styles.css"));
        assert_eq!(local_path("site/index.html", "../shared/a.css").as_deref(), Some("shared/a.css"));
        assert_eq!(local_path("index.html", "../escape.css"), None);
        assert_eq!(local_path("index.html", "https://cdn.example/x.css"), None);
    }

    #[test]
    fn stylesheet_and_script_are_inlined_only_when_local_and_safe() {
        let mut files = BTreeMap::new();
        files.insert("app.js".to_string(), WebFile { content: "console.log(1);".into(), sha256: String::new() });
        files.insert("style.css".to_string(), WebFile { content: "p{color:red}".into(), sha256: String::new() });
        let html = "<head><link rel=\"stylesheet\" href=\"style.css\"></head><body><script src=\"app.js\"></script></body>";
        let out = inline_local("index.html", html, &files).unwrap();
        assert!(out.html.contains("<style>\np{color:red}</style>"));
        assert!(out.html.contains("<script>\nconsole.log(1);</script>"));
        assert_eq!(out.inlined, vec!["style.css".to_string(), "app.js".to_string()]);
    }
}
