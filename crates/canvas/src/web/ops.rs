//! WebCode operations. Each one needs the current file hash (`expected_hash`) and fails with SOURCE_CHANGED on mismatch.
//! The new file hash is the sha256 of the new content. No-op edits are refused, so every commit changes content.

use super::html::{mapped_edit, mapping_hits, tokenize, Property, MAP_ATTR};
use super::paths::validate_rel;
use super::WebDocument;
use crate::common::{invalid, unsupported};
use designllm_contracts::{sha256, DltError, DltResult, ErrorCode, Operation, WebFile};

/// Largest single file in a WebCode artifact.
pub(crate) const MAX_FILE_BYTES: usize = 4 * 1024 * 1024;

/// Attributes whose whole value is one URL. `to`, `from`, and `by` are SVG animation values, which become a URL when an
/// animation targets `href`.
const URL_ATTRS: &[&str] = &[
    "href", "src", "action", "formaction", "poster", "data", "cite", "longdesc", "background", "codebase", "archive",
    "classid", "manifest", "dynsrc", "lowsrc", "to", "from", "by",
];
/// Schemes a mapped URL may use. Relative references are always allowed.
const ALLOWED_SCHEMES: &[&str] = &["http", "https", "mailto", "tel"];
/// Image types allowed in a `data:` URL. SVG is left out because it can carry script.
const DATA_IMAGE_TYPES: &[&str] = &["image/png", "image/jpeg", "image/gif", "image/webp"];
/// Diagnostic codes of the mapping guard, named once so that each refusal and its test use the same text.
pub(crate) const WEB_ATTR_FORBIDDEN: &str = "WEB_ATTR_FORBIDDEN";
pub(crate) const WEB_URL_SCHEME: &str = "WEB_URL_SCHEME";
pub(crate) const WEB_STYLE_SYNTAX: &str = "WEB_STYLE_SYNTAX";

/// Applies operations in order. Returns the changed file paths. Any failure aborts the whole list.
pub(crate) fn apply(doc: &mut WebDocument, operations: &[Operation]) -> DltResult<Vec<String>> {
    let mut changed: Vec<String> = Vec::new();
    for op in operations {
        let path = match op {
            Operation::ReplaceWebSource { path, expected_hash, content } => {
                replace_source(doc, path, expected_hash, content)?;
                path.clone()
            }
            Operation::MappedWebEdit { path, expected_hash, mapping_id, property, value } => {
                mapped(doc, path, expected_hash, mapping_id, property, value)?;
                path.clone()
            }
            other => {
                return Err(unsupported(
                    "OPERATION_UNSUPPORTED",
                    format!("operation {} is not supported by the web engine", other.name()),
                ))
            }
        };
        if !changed.contains(&path) {
            changed.push(path);
        }
    }
    Ok(changed)
}

fn stale(path: &str, expected: &str, actual: &str) -> DltError {
    let mut error = DltError::new(
        ErrorCode::SourceChanged,
        format!("{path} changed since it was read: expected {expected}, current {actual}"),
    );
    error.affected_ids.push(path.to_string());
    error
}

fn existing<'a>(doc: &'a WebDocument, path: &str) -> DltResult<&'a WebFile> {
    validate_rel(path)?;
    doc.files.get(path).ok_or_else(|| {
        let mut error = DltError::new(ErrorCode::NotFound, format!("{path} is not in this project"));
        error.affected_ids.push(path.to_string());
        error
    })
}

fn check_hash(path: &str, file: &WebFile, expected: &str) -> DltResult<()> {
    if file.sha256.eq_ignore_ascii_case(expected) {
        Ok(())
    } else {
        Err(stale(path, expected, &file.sha256))
    }
}

fn check_content(content: &str) -> DltResult<()> {
    if content.starts_with('\u{FEFF}') {
        return Err(invalid("WEB_BOM", "written content must not start with a UTF-8 byte order mark"));
    }
    if content.len() > MAX_FILE_BYTES {
        return Err(invalid("WEB_FILE_TOO_LARGE", "file exceeds the per-file size limit"));
    }
    Ok(())
}

/// Event handlers, `srcdoc`, and the two attributes that change what another attribute means: `http-equiv` turns a `meta`
/// into a refresh, and `attributeName` retargets an SVG animation. A mapping never needs them.
fn attr_name_refused(name: &str) -> bool {
    name.starts_with("on") || matches!(name, "srcdoc" | "http-equiv" | "attributename")
}

/// Checks a value as the browser will read it. A quoted value is escaped by html.rs, and an unquoted value is written
/// double-quoted, so the stored text is the text the browser reads. `refresh` is true for a `content` value of a `meta`
/// with `http-equiv="refresh"`.
fn check_attr_value(name: &str, value: &str, refresh: bool) -> DltResult<()> {
    if name == "style" && !css_well_formed(value) {
        return Err(invalid(
            WEB_STYLE_SYNTAX,
            "a style value cannot hold a CSS comment, a backslash, a control character other than tab, an open string, or a quote in an unquoted url()",
        ));
    }
    let allowed = if URL_ATTRS.contains(&name) {
        url_allowed(value)
    } else if matches!(name, "srcset" | "imagesrcset") {
        // The browser splits a srcset at whitespace and at the commas that end a candidate. Splitting at every comma checks
        // a superset of its candidates. The split comes before the URL text is read, as the browser's list parsing does.
        value.split(|c: char| c.is_whitespace() || c == ',').all(url_allowed)
    } else if name == "ping" {
        // The browser splits ping URLs at ASCII whitespace, which includes tab, LF, and CR.
        value.split(char::is_whitespace).all(url_allowed)
    } else if name == "values" {
        // SVG animation values are separated by semicolons.
        value.split(';').all(url_allowed)
    } else if name == "style" {
        css_allowed(value)
    } else if name == "content" && refresh {
        refresh_allowed(value)
    } else {
        true
    };
    if allowed {
        Ok(())
    } else {
        Err(invalid(WEB_URL_SCHEME, format!("attribute {name} holds a URL whose scheme is not allowed")))
    }
}

/// The text of a URL as the browser reads it. The browser's URL parser removes ASCII tab, LF, and CR and strips leading and
/// trailing C0 controls and spaces. This removes every whitespace and control character, a superset of that, so every scheme
/// the browser reads is also read here (a check may refuse more than the browser would). Every URL check reads its text here.
fn url_text(value: &str) -> String {
    value.chars().filter(|c| !c.is_whitespace() && !c.is_control()).collect()
}

/// A relative reference, a URL whose scheme is in `ALLOWED_SCHEMES`, or a raster image `data:` URL. The scheme is the text
/// before the first colon, when that text is a valid scheme name.
fn url_allowed(value: &str) -> bool {
    let text = url_text(value);
    let Some(colon) = text.find(':') else { return true };
    let scheme = &text[..colon];
    if !is_scheme(scheme) {
        return true;
    }
    let scheme = scheme.to_ascii_lowercase();
    if ALLOWED_SCHEMES.contains(&scheme.as_str()) {
        return true;
    }
    if scheme == "data" {
        let mime = text[colon + 1..].split([';', ',']).next().unwrap_or("").to_ascii_lowercase();
        return DATA_IMAGE_TYPES.contains(&mime.as_str());
    }
    false
}

/// A `style` value. Every `url(...)` argument must pass `url_allowed`, and so must every quoted string. The image functions
/// (`image-set`, `-webkit-image-set`, `image`, `cross-fade`, `src`) take their URLs as quoted strings. A string that is not a
/// URL, such as a font name, is a relative reference and passes.
fn css_allowed(value: &str) -> bool {
    let urls = value.to_ascii_lowercase().split("url(").skip(1).all(|rest| {
        let argument = rest.split(')').next().unwrap_or("");
        url_allowed(&argument.replace(['"', '\''], ""))
    });
    urls && css_strings(value).is_some_and(|strings| strings.into_iter().all(url_allowed))
}

/// A control character the browser reads as a line break or as non-printable: C0 except TAB. CSS treats TAB as whitespace
/// that ends no string, so TAB stays. LF, CR, and FF end a quoted string (CR and FF become LF before the tokenizer runs).
fn css_control(c: char) -> bool {
    c < ' ' && c != '\t'
}

/// Whether this check reads a `style` value the way the browser does. A comment and a backslash escape are not modeled,
/// so both are refused. A control character other than TAB is refused, since a line break ends a quoted string and would
/// move the string boundaries this check reads. A quoted string left open is refused for the same reason. A quote inside an
/// unquoted `url(` is refused too: the browser reads that `url(` as a bad URL, which runs to its `)` and reads no string.
fn css_well_formed(value: &str) -> bool {
    !value.contains('\\')
        && !value.contains("/*")
        && !value.chars().any(css_control)
        && css_strings(value).is_some()
        && value.to_ascii_lowercase().split("url(").skip(1).all(|rest| {
            // A quoted argument is a string, which `css_allowed` reads. After `url(` the browser skips SPACE and TAB only,
            // since LF, CR, and FF are refused above.
            let quoted = rest.trim_start_matches([' ', '\t']).starts_with(['"', '\'']);
            quoted || !rest.split(')').next().unwrap_or("").contains(['"', '\''])
        })
}

/// The contents of the quoted strings in a CSS value, or `None` when a string is left open. Backslashes and control
/// characters are refused before this runs, so no escape or line break ends a string early.
fn css_strings(value: &str) -> Option<Vec<&str>> {
    let mut strings = Vec::new();
    let mut open: Option<(char, usize)> = None;
    for (index, c) in value.char_indices() {
        match open {
            None if c == '"' || c == '\'' => open = Some((c, index + c.len_utf8())),
            Some((quote, start)) if c == quote => {
                strings.push(&value[start..index]);
                open = None;
            }
            _ => {}
        }
    }
    open.is_none().then_some(strings)
}

/// The targets of a meta refresh. The browser reads a delay (digits and dots), then an optional `url=`, then an optional
/// quote, and the rest is the URL. The text is split at `;` and `,`, which end a delay or begin another target. Each part has
/// its delay and `url=` skipped, and the rest must pass `url_allowed`.
fn refresh_allowed(value: &str) -> bool {
    url_text(value).to_ascii_lowercase().split([';', ',']).all(|part| {
        let rest = part.trim_start_matches(|c: char| c.is_ascii_digit() || c == '.');
        let rest = rest.strip_prefix("url=").unwrap_or(rest);
        let rest = rest.strip_prefix(['"', '\'']).unwrap_or(rest);
        url_allowed(rest.trim_end_matches(['"', '\'']))
    })
}

fn is_scheme(text: &str) -> bool {
    let mut chars = text.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic()) && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
}

/// Whether the one element that carries `mapping_id` is a `meta` with `http-equiv="refresh"`. An unknown or ambiguous
/// mapping is not a refresh here, because `mapped_edit` refuses it.
fn is_refresh_meta(content: &str, mapping_id: &str) -> bool {
    let Ok(tags) = tokenize(content) else { return false };
    match mapping_hits(&tags, mapping_id).as_slice() {
        [index] => {
            let tag = &tags[*index];
            tag.name == "meta" && tag.attr("http-equiv").is_some_and(|attr| attr.value.eq_ignore_ascii_case("refresh"))
        }
        _ => false,
    }
}

fn replace_source(doc: &mut WebDocument, path: &str, expected_hash: &str, content: &str) -> DltResult<()> {
    let file = existing(doc, path)?;
    check_hash(path, file, expected_hash)?;
    check_content(content)?;
    if file.content == content {
        return Err(invalid("WEB_NO_CHANGE", "the replacement is identical to the current source"));
    }
    let next = WebFile { content: content.to_string(), sha256: sha256(content.as_bytes()) };
    doc.files.insert(path.to_string(), next);
    Ok(())
}

fn mapped(doc: &mut WebDocument, path: &str, expected_hash: &str, mapping_id: &str, property: &str, value: &str) -> DltResult<()> {
    let file = existing(doc, path)?;
    check_hash(path, file, expected_hash)?;
    check_content(value)?;
    let target = if property == "text" {
        Property::Text
    } else if let Some(name) = property.strip_prefix("attr:") {
        if name.is_empty() || !name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_') {
            return Err(invalid("WEB_PROPERTY", "attribute name must be lowercase letters, digits, - or _"));
        }
        if name == MAP_ATTR {
            return Err(invalid("WEB_MAPPING_KEY", "the mapping attribute cannot be edited"));
        }
        if attr_name_refused(name) {
            return Err(invalid(WEB_ATTR_FORBIDDEN, format!("attribute {name} cannot be edited through a mapping")));
        }
        // A `content` value is a refresh target only on a `meta` whose `http-equiv` is `refresh`. Mapped edits refuse
        // `http-equiv`, so the element keeps that meaning after the edit.
        let refresh = name == "content" && is_refresh_meta(&file.content, mapping_id);
        check_attr_value(name, value, refresh)?;
        Property::Attr(name.to_string())
    } else {
        return Err(invalid("WEB_PROPERTY", "property must be \"text\" or \"attr:<name>\""));
    };
    let updated = mapped_edit(&file.content, mapping_id, &target, value)?;
    if updated == file.content {
        return Err(invalid("WEB_NO_CHANGE", "the mapped value is already the current value"));
    }
    if updated.len() > MAX_FILE_BYTES {
        return Err(invalid("WEB_FILE_TOO_LARGE", "file exceeds the per-file size limit after the edit"));
    }
    let next = WebFile { sha256: sha256(updated.as_bytes()), content: updated };
    doc.files.insert(path.to_string(), next);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    const LINK: &str = "<a data-dlm-map=\"m\" href=\"old.html\">go</a>";

    /// Runs one mapped edit of `index.html`. Returns the outcome and the file text after it.
    fn run(content: &str, property: &str, value: &str) -> (DltResult<Vec<String>>, String) {
        let mut doc = doc_with(content);
        let op = Operation::MappedWebEdit {
            path: "index.html".into(),
            expected_hash: doc.files["index.html"].sha256.clone(),
            mapping_id: "m".into(),
            property: property.into(),
            value: value.into(),
        };
        let outcome = apply(&mut doc, &[op]);
        (outcome, doc.files["index.html"].content.clone())
    }

    /// The file text after an edit that must be written.
    fn accepted(content: &str, property: &str, value: &str) -> String {
        let (outcome, after) = run(content, property, value);
        if let Err(err) = outcome {
            panic!("{property} = {value:?} must be written: {err:?}");
        }
        after
    }

    /// The refusal of an edit that must not be written. The file must still hold `content`.
    fn refused(content: &str, property: &str, value: &str) -> DltError {
        let (outcome, after) = run(content, property, value);
        assert_eq!(after, content, "{property} = {value:?} must leave the file unchanged");
        outcome.unwrap_err()
    }

    fn doc_with(content: &str) -> WebDocument {
        let mut files = BTreeMap::new();
        files.insert("index.html".to_string(), WebFile { content: content.into(), sha256: sha256(content.as_bytes()) });
        WebDocument { entry: "index.html".into(), files }
    }

    #[test]
    fn replace_source_updates_content_and_hash_and_rejects_stale_hash() {
        let mut doc = doc_with("<p>a</p>");
        let old = sha256(b"<p>a</p>");
        let op = Operation::ReplaceWebSource { path: "index.html".into(), expected_hash: old.clone(), content: "<p>b</p>".into() };
        apply(&mut doc, std::slice::from_ref(&op)).unwrap();
        assert_eq!(doc.files["index.html"].sha256, sha256(b"<p>b</p>"));
        let err = apply(&mut doc, &[op]).unwrap_err();
        assert_eq!(err.code, ErrorCode::SourceChanged);
    }

    #[test]
    fn replace_source_refuses_bom_no_change_and_unknown_paths() {
        let mut doc = doc_with("<p>a</p>");
        let hash = sha256(b"<p>a</p>");
        let bom = Operation::ReplaceWebSource { path: "index.html".into(), expected_hash: hash.clone(), content: "\u{FEFF}<p>b</p>".into() };
        assert!(apply(&mut doc, &[bom]).is_err());
        let same = Operation::ReplaceWebSource { path: "index.html".into(), expected_hash: hash.clone(), content: "<p>a</p>".into() };
        assert_eq!(apply(&mut doc, &[same]).unwrap_err().diagnostics[0].code, "WEB_NO_CHANGE");
        let missing = Operation::ReplaceWebSource { path: "other.html".into(), expected_hash: hash, content: "x".into() };
        assert_eq!(apply(&mut doc, &[missing]).unwrap_err().code, ErrorCode::NotFound);
    }

    #[test]
    fn mapped_edit_changes_only_the_mapped_element_and_updates_the_hash() {
        let mut doc = doc_with("<h1 data-dlm-map=\"title\">Old</h1><p>keep</p>");
        let hash = doc.files["index.html"].sha256.clone();
        let op = Operation::MappedWebEdit {
            path: "index.html".into(),
            expected_hash: hash,
            mapping_id: "title".into(),
            property: "text".into(),
            value: "New & better".into(),
        };
        apply(&mut doc, &[op]).unwrap();
        assert_eq!(doc.files["index.html"].content, "<h1 data-dlm-map=\"title\">New &amp; better</h1><p>keep</p>");
    }

    #[test]
    fn mapped_edit_refuses_mapping_key_changes_and_bad_property_names() {
        let mut doc = doc_with("<h1 data-dlm-map=\"title\">Old</h1>");
        let hash = doc.files["index.html"].sha256.clone();
        let key = Operation::MappedWebEdit {
            path: "index.html".into(),
            expected_hash: hash.clone(),
            mapping_id: "title".into(),
            property: "attr:data-dlm-map".into(),
            value: "other".into(),
        };
        assert!(apply(&mut doc, &[key]).is_err());
        let bad = Operation::MappedWebEdit {
            path: "index.html".into(),
            expected_hash: hash,
            mapping_id: "title".into(),
            property: "style".into(),
            value: "x".into(),
        };
        assert_eq!(apply(&mut doc, &[bad]).unwrap_err().code, ErrorCode::InvalidArgument);
    }

    #[test]
    fn event_handler_srcdoc_and_switch_attributes_are_refused_by_name() {
        let html = "<div data-dlm-map=\"m\" onclick=\"a()\" srcdoc=\"x\" http-equiv=\"refresh\" attributename=\"fill\"></div>";
        for name in ["onclick", "onload", "on", "srcdoc", "http-equiv", "attributename"] {
            assert_eq!(refused(html, &format!("attr:{name}"), "x").diagnostics[0].code, WEB_ATTR_FORBIDDEN, "{name}");
        }
        for name in ["ONCLICK", "OnLoad", "xlink:href"] {
            assert_eq!(refused(html, &format!("attr:{name}"), "x").diagnostics[0].code, "WEB_PROPERTY", "{name}");
        }
    }

    #[test]
    fn url_attributes_refuse_script_and_non_allowed_schemes() {
        let names = ["href", "src", "action", "formaction", "poster", "data", "cite", "background", "srcset", "ping", "values", "to"];
        let attrs: Vec<String> = names.iter().map(|name| format!("{name}=\"a\"")).collect();
        let html = format!("<x data-dlm-map=\"m\" {}>", attrs.join(" "));
        let values = [
            "javascript:alert(1)", "vbscript:msgbox(1)", "livescript:x", "data:text/html,<script>alert(1)</script>",
            "data:text/html;base64,PHNjcmlwdD4=", "data:image/svg+xml,<svg onload=alert(1)>", "file:///C:/Windows/win.ini",
            "blob:https://example.com/x", "about:srcdoc",
        ];
        for name in names {
            for value in values {
                assert_eq!(refused(&html, &format!("attr:{name}"), value).diagnostics[0].code, WEB_URL_SCHEME, "{name} = {value}");
            }
        }
    }

    #[test]
    fn case_whitespace_and_controls_do_not_hide_a_scheme() {
        let tricks = [
            "JaVaScript:alert(1)", "JAVASCRIPT:alert(1)", " javascript:alert(1)", "\tjavascript:alert(1)",
            "\u{1}javascript:alert(1)", "javascript:alert(1)\n", "javascript:alert(1)\u{7f}", "java\tscript:alert(1)",
            "java\nscript:alert(1)", "java\r\nscript:alert(1)", "java\u{0}script:alert(1)", "java \tscript:alert(1)",
        ];
        // Written the same way in both forms: whitespace and controls are removed before the scheme is read.
        let unquoted = "<a data-dlm-map=\"m\" href=old.html>go</a>";
        for value in tricks {
            assert_eq!(refused(LINK, "attr:href", value).diagnostics[0].code, WEB_URL_SCHEME, "{value:?}");
            assert_eq!(refused(unquoted, "attr:href", value).diagnostics[0].code, WEB_URL_SCHEME, "unquoted {value:?}");
        }
    }

    #[test]
    fn allowed_schemes_relative_references_and_raster_data_urls_are_written() {
        for value in [
            "https://example.com/a?x=1&y=2", "http://example.com", "mailto:someone@example.com", "tel:+15555550100",
            "page2.html", "./img/a.png", "../x.css", "#top", "?q=1", "/abs/path.html", "//cdn.example.com/x.js", "&\u{e9};page.html",
            "data:image/png;base64,iVBORw0KGgo=", "data:image/jpeg;base64,/9j/4AAQ",
        ] {
            let written = accepted(LINK, "attr:href", value);
            assert!(written.contains(&format!("href=\"{}\"", value.replace('&', "&amp;"))), "{value}: {written}");
        }
    }

    #[test]
    fn every_item_of_a_list_is_checked_and_each_list_splits_as_the_browser_does() {
        let srcset = "<img data-dlm-map=\"m\" srcset=\"a.png 1x\" src=\"a.png\">";
        assert_eq!(refused(srcset, "attr:srcset", "a.png 1x, javascript:alert(1) 2x").diagnostics[0].code, WEB_URL_SCHEME);
        assert_eq!(refused(srcset, "attr:srcset", "a.png 1x,JaVaScript:x").diagnostics[0].code, WEB_URL_SCHEME);
        accepted(srcset, "attr:srcset", "a.png 1x, https://example.com/b.png 2x, data:image/png;base64,AAAA 3x");
        // A semicolon separates neither srcset candidates nor ping URLs, so these whole URLs are written.
        accepted(srcset, "attr:srcset", "https://example.com/a;k:v 1x");
        let ping = "<a data-dlm-map=\"m\" ping=\"a\">go</a>";
        accepted(ping, "attr:ping", "https://example.com/a;k:v");
        assert_eq!(refused(ping, "attr:ping", "https://example.com/x javascript:alert(1)").diagnostics[0].code, WEB_URL_SCHEME);
        let animate = "<animate data-dlm-map=\"m\" values=\"0;1\">";
        assert_eq!(refused(animate, "attr:values", "0;javascript:alert(1)").diagnostics[0].code, WEB_URL_SCHEME);
        accepted(animate, "attr:values", "0;10:30;1");
    }

    #[test]
    fn style_urls_and_image_function_strings_are_checked() {
        let html = "<p data-dlm-map=\"m\" style=\"color:red\">x</p>";
        for value in [
            "background:url(javascript:alert(1))", "background:URL('JaVaScript:alert(1)')",
            "background: url( \" javascript:x\" )", "background:url(vbscript:x) no-repeat",
            "background-image:image-set(\"data:image/svg+xml,<svg/>\" 1x)",
            "background-image:-webkit-image-set(\"javascript:alert(1)\" 1x)",
            "background-image:image(\"javascript:alert(1)\")",
            "background:cross-fade(url(a.png), \"javascript:alert(1)\" 50%)",
            "background:src(\"javascript:alert(1)\")",
            "background-image:image-set(\" javascript:x \" 1x)",
        ] {
            assert_eq!(refused(html, "attr:style", value).diagnostics[0].code, WEB_URL_SCHEME, "{value}");
        }
        for value in [
            "color:blue", "background:url(images/a.png) no-repeat", "background:url(https://example.com/a.png)",
            "background:url('data:image/png;base64,AAAA')", "background-image:image-set(\"a.png\" 1x, \"https://example.com/b.png\" 2x)",
            "background:cross-fade(url(a.png), \"b.png\" 50%)",
        ] {
            assert!(accepted(html, "attr:style", value).contains("style=\""), "{value}");
        }
    }

    #[test]
    fn ordinary_style_values_and_query_strings_are_written() {
        let style = "<p data-dlm-map=\"m\" style=\"color:red\">x</p>";
        let stored = accepted(style, "attr:style", "font-family: \"Segoe UI\", 'Noto Sans KR', sans-serif; color: #123");
        assert!(stored.contains("style=\"font-family: &quot;Segoe UI&quot;, 'Noto Sans KR', sans-serif; color: #123\""), "{stored}");
        let stored = accepted(style, "attr:style", "background-image: image-set(\"a.png\" 1x, \"b.png\" 2x); width: 120px");
        assert!(stored.contains("style=\"background-image: image-set(&quot;a.png&quot; 1x, &quot;b.png&quot; 2x); width: 120px\""), "{stored}");
        let link = "<a data-dlm-map=\"m\" href=old.html>go</a>";
        assert_eq!(accepted(link, "attr:href", "a.html?x=1&y=2"), "<a data-dlm-map=\"m\" href=\"a.html?x=1&amp;y=2\">go</a>");
        let description = "<meta data-dlm-map=\"m\" name=\"description\" content=\"old\">";
        assert!(accepted(description, "attr:content", "Plain text: see https://example.com/?q=a").contains("content=\"Plain text: see https://example.com/?q=a\""));
    }

    #[test]
    fn css_comments_and_backslashes_in_style_are_refused() {
        let html = "<p data-dlm-map=\"m\" style=\"color:red\">x</p>";
        for value in [
            "background:url(/*x*/\"javascript:alert(1)\")", "background:url(/*x*/\"data:image/svg+xml,<svg/>\")",
            "background:url(\\6a avascript:alert(1))", "background:u\\72l(javascript:x)",
        ] {
            assert_eq!(refused(html, "attr:style", value).diagnostics[0].code, WEB_STYLE_SYNTAX, "{value}");
        }
    }

    #[test]
    fn character_references_are_text_in_quoted_and_unquoted_values() {
        // Both forms store the value escaped inside double quotes, so the browser reads `&#106;` as the text `&#106;`, a
        // relative reference. Neither form is refused.
        let quoted = accepted(LINK, "attr:href", "&#106;avascript:alert(1)");
        assert!(quoted.contains("href=\"&amp;#106;avascript:alert(1)\""), "{quoted}");
        let unquoted = "<a data-dlm-map=\"m\" href=old.html>go</a>";
        for value in ["&#106;avascript:alert(1)", "&#x6A;avascript:alert(1)", "javascript&colon;alert(1)", "java&Tab;script:alert(1)", "&quot;x"] {
            let stored = accepted(unquoted, "attr:href", value);
            assert!(stored.contains(&format!("href=\"{}\"", value.replace('&', "&amp;"))), "{value}: {stored}");
        }
    }

    #[test]
    fn unquoted_values_are_written_quoted_and_checked_as_stored() {
        // An unquoted value is replaced by a double-quoted one, so `&`, `=`, and quotes need no refusal. The check reads the
        // stored text, so a real scheme is still refused in either form.
        let style = "<p data-dlm-map=\"m\" style=color:red>x</p>";
        let stored = accepted(style, "attr:style", "background:url(&quot;javascript:alert(1)&quot;)");
        assert!(stored.contains("style=\"background:url(&amp;quot;javascript:alert(1)&amp;quot;)\""), "{stored}");
        let meta = "<meta data-dlm-map=\"m\" http-equiv=refresh content=0;url=a>";
        let stored = accepted(meta, "attr:content", "0;url&equals;http://example.com");
        assert!(stored.contains("content=\"0;url&amp;equals;http://example.com\""), "{stored}");
        let srcset = "<img data-dlm-map=\"m\" srcset=a.png>";
        let stored = accepted(srcset, "attr:srcset", "a.png&#32&#44javascript:alert(1)");
        assert!(stored.contains("srcset=\"a.png&amp;#32&amp;#44javascript:alert(1)\""), "{stored}");
        let link = "<a data-dlm-map=\"m\" href=old.html>go</a>";
        assert_eq!(refused(link, "attr:href", "javascript:alert(1)").diagnostics[0].code, WEB_URL_SCHEME);
        assert_eq!(refused(link, "attr:href", "javascript\t:alert(1)").diagnostics[0].code, WEB_URL_SCHEME);
    }

    #[test]
    fn meta_refresh_targets_are_checked_with_or_without_the_url_marker() {
        let meta = "<meta data-dlm-map=\"m\" http-equiv=\"refresh\" content=\"0;url=index.html\">";
        for value in [
            "0;url=javascript:alert(1)", "0; URL = 'JavaScript:alert(1)'", "0;javascript:alert(1)", "0,javascript:alert(1)",
            "0 javascript:alert(1)", "0javascript:alert(1)", "0;url =javascript:alert(1)", "0;blob:https://example.com/x",
            "0;url=\"data:text/html,x\"",
        ] {
            assert_eq!(refused(meta, "attr:content", value).diagnostics[0].code, WEB_URL_SCHEME, "{value}");
        }
        assert!(accepted(meta, "attr:content", "5;url=https://example.com/next").contains("content=\"5;url=https://example.com/next\""));
        assert!(accepted(meta, "attr:content", "5 url=https://example.com/next").contains("content=\"5 url=https://example.com/next\""));
        assert!(accepted(meta, "attr:content", "Plain description").contains("content=\"Plain description\""));
    }

    #[test]
    fn content_is_a_refresh_target_only_on_a_refresh_meta() {
        let description = "<meta data-dlm-map=\"m\" name=\"description\" content=\"old\">";
        assert!(accepted(description, "attr:content", "see url=foo:bar").contains("content=\"see url=foo:bar\""));
    }

    #[test]
    fn whitespace_and_control_bypasses_are_refused_in_every_url_path() {
        // The browser drops ASCII tab, LF, and CR from a URL. A split at whitespace must not hide a scheme behind them.
        let link = "<a data-dlm-map=\"m\" href=\"old.html\">go</a>";
        for value in ["javascript\t:alert(1)", "java\nscript:alert(1)", "data\r:text/html,x"] {
            assert_eq!(refused(link, "attr:href", value).diagnostics[0].code, WEB_URL_SCHEME, "{value:?}");
        }
        let meta = "<meta data-dlm-map=\"m\" http-equiv=\"refresh\" content=\"0;url=index.html\">";
        for value in [
            "0;url=javascript\t:alert(1)", "0;url=data\n:text/html,<script>alert(1)</script>", "0\t;\turl\t=\tjavascript:alert(1)",
        ] {
            assert_eq!(refused(meta, "attr:content", value).diagnostics[0].code, WEB_URL_SCHEME, "{value:?}");
        }
        let animate = "<animate data-dlm-map=\"m\" values=\"0;1\">";
        assert_eq!(refused(animate, "attr:values", "0;javascript\t:alert(1)").diagnostics[0].code, WEB_URL_SCHEME);
        let ping = "<a data-dlm-map=\"m\" ping=\"a\">go</a>";
        assert_eq!(refused(ping, "attr:ping", "https://example.com/a\tjavascript:alert(1)").diagnostics[0].code, WEB_URL_SCHEME);
        let srcset = "<img data-dlm-map=\"m\" srcset=\"a.png 1x\" src=\"a.png\">";
        assert_eq!(refused(srcset, "attr:srcset", "a.png\tjavascript:alert(1) 2x").diagnostics[0].code, WEB_URL_SCHEME);
    }

    #[test]
    fn boolean_attributes_cannot_be_replaced_and_empty_unquoted_values_are_quoted() {
        let boolean = "<input data-dlm-map=\"m\" disabled class=\"x\">";
        assert_eq!(refused(boolean, "attr:disabled", "x").diagnostics[0].code, "WEB_ATTR_NO_VALUE");
        let empty = "<a data-dlm-map=\"m\" href=>go</a>";
        assert_eq!(accepted(empty, "attr:href", "a.html"), "<a data-dlm-map=\"m\" href=\"a.html\">go</a>");
    }

    #[test]
    fn text_values_are_escaped_so_no_markup_or_meta_refresh_can_appear() {
        let out = accepted(LINK, "text", "<meta http-equiv=refresh content='0;url=javascript:alert(1)'><script>alert(1)</script>");
        assert!(!out.contains("<meta") && !out.contains("<script"), "{out}");
        assert!(out.contains("&lt;meta http-equiv=refresh content='0;url=javascript:alert(1)'&gt;&lt;script&gt;"), "{out}");
    }

    #[test]
    fn quoted_values_cannot_add_an_attribute() {
        let out = accepted(LINK, "attr:href", "x\" onclick=\"alert(1)");
        let tags = crate::web::html::tokenize(&out).unwrap();
        assert!(tags[0].attr("onclick").is_none());
        assert_eq!(tags[0].attr("href").unwrap().value, "x&quot; onclick=&quot;alert(1)");
    }

    #[test]
    fn unknown_operations_are_unsupported_for_web() {
        let mut doc = doc_with("<p>a</p>");
        let op = Operation::SetText { node_id: "n".into(), text: "x".into() };
        assert_eq!(apply(&mut doc, &[op]).unwrap_err().code, ErrorCode::UnsupportedOperation);
    }
}
