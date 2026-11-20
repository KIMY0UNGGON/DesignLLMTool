//! WebCode design-token sync. A published design system becomes one generated stylesheet beside the entry HTML,
//! `design-tokens.css`, and the entry head links it exactly once. Token `<id>` becomes the custom property `--dl-<id>`:
//! light values sit on `:root`, dark values under `prefers-color-scheme: dark` and `[data-theme="dark"]`. Source files
//! refer to the properties with `var(--dl-<id>)`. This module never rewrites source.
//!
//! The file starts with a header that holds the SHA-256 of its body. A file is unchanged only when the header matches its body
//! and the body is exactly what the generator writes for the token values in it (`generated_body`). Any other file at that
//! path was edited, so a sync refuses it (SOURCE_CHANGED) instead of overwriting the edit. A file without the header belongs to
//! the user, so a sync refuses to replace it (WEB_NAME_CLASH). Detaching removes the link and keeps the file and its values.

use super::{capability_report, html, paths, validate_project};
use crate::canvas::model::into_result;
use crate::canvas::tokens::validate_definition;
use crate::common::{css_px, fail, invalid, EMU_PER_CSS_PX};
use designllm_contracts::{
    sha256, Artifact, ArtifactBody, DesignSystemRef, DltError, DltResult, ErrorCode, PreparedChange, TokenDef, TokenValues,
    WebDocument, WebFile,
};
use std::collections::BTreeMap;

/// Name of the generated stylesheet. It sits in the entry's folder, and the link names it relative to the entry.
pub(crate) const GENERATED_NAME: &str = "design-tokens.css";
const HEADER_START: &str = "/* designllm:design-tokens sha256=";
const HEADER_END: &str = " */";
/// Layout of the generated body. `render_body` writes it and `body_rows` reads it back, so the two cannot drift apart.
const ROOT_OPEN: &str = ":root {";
const MEDIA_OPEN: &str = "@media (prefers-color-scheme: dark) {";
const DARK_OPEN: &str = "  :root:not([data-theme=\"light\"]) {";
const DARK_CLOSE: &str = "  }";
const ATTR_OPEN: &str = ":root[data-theme=\"dark\"] {";
const BLOCK_CLOSE: &str = "}";
const TOP_INDENT: &str = "  ";
const MEDIA_INDENT: &str = "    ";
const PROPERTY_PREFIX: &str = "--dl-";
/// A quoted font family ends with this fallback. The generator writes the characters listed in `FAMILY_ESCAPES` as CSS escapes,
/// and the body reader accepts exactly these spellings.
const FAMILY_TAIL: &str = "\", sans-serif";
const FAMILY_ESCAPES: [(char, &str); 4] = [('\\', "\\\\"), ('"', "\\\""), ('<', "\\3C "), ('>', "\\3E ")];

/// Token ids become CSS property names: non-empty, lower-case ASCII letters, digits and `-` only.
fn valid_css_name(id: &str) -> bool {
    !id.is_empty() && id.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// Why a file at the generated path is not an unchanged file this module wrote.
enum Origin {
    /// The header is present, but the file is not the unchanged output: the hash does not match the body, or the body is not
    /// the generator's layout.
    Edited,
    /// No header of this module.
    Foreign,
}

/// The body of the file at the generated path, when that file is still this module's unchanged output. Every check of "unchanged"
/// goes through here: attach and update, the snapshot check and the content digest. The header must match the body, and the
/// body must be the generator's layout, so a recomputed header does not make an edited file unchanged.
fn generated_body(content: &str) -> Result<&str, Origin> {
    let (first, body) = content.split_once('\n').ok_or(Origin::Foreign)?;
    let hex = first
        .strip_prefix(HEADER_START)
        .and_then(|rest| rest.strip_suffix(HEADER_END))
        .ok_or(Origin::Foreign)?;
    if hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
        return Err(Origin::Foreign);
    }
    if sha256(body.as_bytes()) == hex && is_generator_layout(body) {
        Ok(body)
    } else {
        Err(Origin::Edited)
    }
}

/// Whether `body` is the generator's layout for the token values read back from it: every row parses, and writing those values
/// again gives the same bytes.
fn is_generator_layout(body: &str) -> bool {
    body_rows(body)
        .and_then(|rows| tokens_of(&rows))
        .and_then(|tokens| render_body(&tokens).ok())
        .is_some_and(|rendered| rendered == body)
}

pub(crate) fn generated_content(tokens: &BTreeMap<String, TokenDef>) -> DltResult<String> {
    let body = render_body(tokens)?;
    Ok(format!("{HEADER_START}{}{HEADER_END}\n{body}", sha256(body.as_bytes())))
}

/// The stylesheet body for a token set. Tokens are written in id order, so equal tokens give equal text.
fn render_body(tokens: &BTreeMap<String, TokenDef>) -> DltResult<String> {
    let mut rows: Vec<(&str, String, String)> = Vec::with_capacity(tokens.len());
    for (id, definition) in tokens {
        if !valid_css_name(id) {
            return Err(invalid("WEB_TOKEN_ID", format!("token id {id:?} must use only a-z, 0-9 and '-' in a CSS property name")));
        }
        validate_definition(id, definition)?;
        let (light, dark) = css_values(&definition.values)?;
        rows.push((id.as_str(), light, dark));
    }
    let declarations = |indent: &str, dark: bool| -> String {
        rows.iter()
            .map(|(id, light, dark_value)| format!("{indent}{PROPERTY_PREFIX}{id}: {};\n", if dark { dark_value } else { light }))
            .collect()
    };
    Ok(format!(
        "{ROOT_OPEN}\n{light}{BLOCK_CLOSE}\n{MEDIA_OPEN}\n{DARK_OPEN}\n{media}{DARK_CLOSE}\n{BLOCK_CLOSE}\n{ATTR_OPEN}\n{attr}{BLOCK_CLOSE}\n",
        light = declarations(TOP_INDENT, false),
        media = declarations(MEDIA_INDENT, true),
        attr = declarations(TOP_INDENT, true),
    ))
}

/// CSS text of the light and the dark value of one token. Lengths are stored in EMU and written as CSS pixels.
fn css_values(values: &TokenValues) -> DltResult<(String, String)> {
    Ok(match values {
        TokenValues::Color { light, dark } => (light.clone(), dark.clone()),
        TokenValues::FontFamily { light, dark } => (css_family(light)?, css_family(dark)?),
        TokenValues::FontSize { light, dark } | TokenValues::Spacing { light, dark } | TokenValues::Radius { light, dark } => {
            (length(*light), length(*dark))
        }
    })
}

fn length(emu: i64) -> String {
    format!("{}px", css_px(emu))
}

/// A quoted family name with a generic fallback. `<` and `>` become CSS escapes, so the text cannot end a `<style>`
/// element when the stylesheet is inlined into an HTML export.
fn css_family(name: &str) -> DltResult<String> {
    if name.chars().any(char::is_control) {
        return Err(invalid("WEB_TOKEN_FONT", "font family token values must not contain control characters"));
    }
    let mut out = String::with_capacity(name.len() + 16);
    out.push('"');
    for ch in name.chars() {
        match FAMILY_ESCAPES.iter().find(|(text, _)| *text == ch) {
            Some((_, escape)) => out.push_str(escape),
            None => out.push(ch),
        }
    }
    out.push_str(FAMILY_TAIL);
    Ok(out)
}

/// The generated file's project path: the entry's folder plus the fixed name.
fn generated_path(entry: &str) -> String {
    match entry.rfind('/') {
        Some(at) => format!("{}/{GENERATED_NAME}", &entry[..at]),
        None => GENERATED_NAME.to_owned(),
    }
}

fn entry_text(doc: &WebDocument) -> DltResult<&str> {
    doc.files
        .get(&doc.entry)
        .map(|file| file.content.as_str())
        .ok_or_else(|| invalid("WEB_ENTRY_MISSING", "the entry file is not in the project"))
}

/// Stylesheet links in the entry that load the generated file. Paths compare with their letter case folded, as the project's
/// path rule compares names, so `Design-Tokens.css` links the generated file too.
fn managed_links(entry: &str, text: &str, generated: &str) -> DltResult<Vec<html::Tag>> {
    let folded = paths::fold_case(generated);
    Ok(html::tokenize(text)?
        .into_iter()
        .filter(|tag| {
            !tag.closing
                && tag.name == "link"
                && tag.attr("rel").is_some_and(|rel| rel.value.eq_ignore_ascii_case("stylesheet"))
                && tag
                    .attr("href")
                    .and_then(|href| html::local_path(entry, &href.value))
                    .is_some_and(|path| paths::fold_case(&path) == folded)
        })
        .collect())
}

/// The entry's only head element, found by its closing tag. A missing or doubled head is refused, not guessed.
fn head_close(tags: &[html::Tag]) -> DltResult<&html::Tag> {
    let opens: Vec<&html::Tag> = tags.iter().filter(|tag| !tag.closing && tag.name == "head").collect();
    let closes: Vec<&html::Tag> = tags.iter().filter(|tag| tag.closing && tag.name == "head").collect();
    if opens.len() > 1 || closes.len() > 1 {
        return Err(invalid("WEB_HEAD_AMBIGUOUS", "the entry HTML must have exactly one head element"));
    }
    match (opens.first(), closes.first()) {
        (Some(open), Some(close)) if open.start < close.start => Ok(*close),
        _ => Err(invalid("WEB_HEAD_MISSING", "the entry HTML needs a head element with a closing tag to link the design tokens")),
    }
}

/// Where the link goes. When `</head>` starts its line, the link gets a line of its own with the same indent; otherwise it
/// goes right before `</head>`. `link_removal` undoes either form exactly.
fn link_insertion(text: &str, close: &html::Tag) -> (usize, String) {
    let newline = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let line_start = text[..close.start].rfind('\n').map_or(0, |at| at + 1);
    let indent = &text[line_start..close.start];
    let link = format!("<link rel=\"stylesheet\" href=\"{GENERATED_NAME}\">{newline}");
    if indent.bytes().all(|b| b == b' ' || b == b'\t') {
        (line_start, format!("{indent}{link}"))
    } else {
        (close.start, link)
    }
}

/// The bytes a managed link occupies: its whole line when the link is alone on it, otherwise the tag and its line break.
fn link_removal(text: &str, link: &html::Tag) -> (usize, usize) {
    let line_start = text[..link.start].rfind('\n').map_or(0, |at| at + 1);
    let start = if text[line_start..link.start].bytes().all(|b| b == b' ' || b == b'\t') { line_start } else { link.start };
    let end = if text[link.end..].starts_with("\r\n") {
        link.end + 2
    } else if text[link.end..].starts_with('\n') {
        link.end + 1
    } else {
        link.end
    };
    (start, end)
}

/// Attach to a project without a link. The generated path must be free, or hold this module's own unedited file from an
/// earlier link. Any other file with the same name in any letter case, and any folder of that name, is a clash.
fn check_slot(doc: &WebDocument, generated: &str) -> DltResult<()> {
    let folded = paths::fold_case(generated);
    let folder = format!("{folded}/");
    for (path, file) in &doc.files {
        if path == generated {
            match generated_body(&file.content) {
                Ok(_) => {}
                Err(Origin::Edited) => return Err(source_changed(path)),
                Err(Origin::Foreign) => return Err(name_clash(path)),
            }
        } else {
            let other = paths::fold_case(path);
            if other == folded || other.starts_with(folder.as_str()) {
                return Err(name_clash(path));
            }
        }
    }
    Ok(())
}

fn attach(doc: &mut WebDocument, tokens: &BTreeMap<String, TokenDef>) -> DltResult<Vec<String>> {
    let entry = doc.entry.clone();
    let generated = generated_path(&entry);
    paths::validate_rel(&generated)?;
    let text = entry_text(doc)?;
    if !managed_links(&entry, text, &generated)?.is_empty() {
        return Err(invalid("WEB_LINK_PRESENT", format!("the entry already links {GENERATED_NAME} but no design system is linked")));
    }
    check_slot(doc, &generated)?;
    let tags = html::tokenize(text)?;
    let (at, line) = link_insertion(text, head_close(&tags)?);
    let linked = format!("{}{}{}", &text[..at], line, &text[at..]);
    let content = generated_content(tokens)?;
    doc.files.insert(entry.clone(), web_file(linked));
    doc.files.insert(generated.clone(), web_file(content));
    Ok(vec![generated, entry])
}

fn update(doc: &mut WebDocument, tokens: &BTreeMap<String, TokenDef>) -> DltResult<Vec<String>> {
    let entry = doc.entry.clone();
    let generated = generated_path(&entry);
    let links = managed_links(&entry, entry_text(doc)?, &generated)?;
    if links.len() != 1 {
        return Err(link_count(links.len()));
    }
    let current = doc.files.get(&generated).ok_or_else(|| missing(&generated))?;
    generated_body(&current.content).map_err(|_| source_changed(&generated))?;
    let content = generated_content(tokens)?;
    if content == current.content {
        return Ok(Vec::new());
    }
    doc.files.insert(generated.clone(), web_file(content));
    Ok(vec![generated])
}

/// Removes the link only. The generated file stays with its values, so the project keeps what the tokens resolved to.
fn detach(doc: &mut WebDocument) -> DltResult<Vec<String>> {
    let entry = doc.entry.clone();
    let generated = generated_path(&entry);
    let text = entry_text(doc)?;
    let links = managed_links(&entry, text, &generated)?;
    let edited = match links.as_slice() {
        [] => return Ok(Vec::new()),
        [link] => {
            let (start, end) = link_removal(text, link);
            format!("{}{}", &text[..start], &text[end..])
        }
        _ => return Err(link_count(links.len())),
    };
    doc.files.insert(entry.clone(), web_file(edited));
    Ok(vec![entry])
}

/// Candidate after a sync. The source is changed only in the returned candidate; `artifact` is not touched.
pub(crate) fn sync(
    artifact: &Artifact,
    tokens: &BTreeMap<String, TokenDef>,
    design_system: Option<&DesignSystemRef>,
) -> DltResult<PreparedChange> {
    let mut next = artifact.clone();
    let ArtifactBody::WebCode(doc) = &mut next.body else {
        return Err(invalid("DESIGN_SYSTEM_BODY", "shared tokens require a WebCode artifact"));
    };
    let mut changed = match (artifact.design_system.is_some(), design_system.is_some()) {
        (false, false) => Vec::new(),
        (false, true) => attach(doc, tokens)?,
        (true, true) => update(doc, tokens)?,
        (true, false) => detach(doc)?,
    };
    let diagnostics = into_result(validate_project(doc))?;
    if artifact.design_system.as_ref() != design_system {
        changed.push("artifact:design_system".to_owned());
    }
    next.design_system = design_system.cloned();
    next.capabilities = capability_report();
    next.diagnostics = diagnostics.clone();
    Ok(PreparedChange { artifact: next, changed_ids: changed, diagnostics })
}

/// Read-only check of a linked project against its pinned tokens: one link, an unedited generated file, and the token text
/// the pinned tokens produce. A different text is a blocking drift diagnostic (WEB_TOKEN_DRIFT).
pub(crate) fn validate_snapshot(artifact: &Artifact, tokens: &BTreeMap<String, TokenDef>) -> DltResult<()> {
    let ArtifactBody::WebCode(doc) = &artifact.body else {
        return Err(invalid("DESIGN_SYSTEM_BODY", "shared tokens require a WebCode artifact"));
    };
    if artifact.design_system.is_none() {
        return Ok(());
    }
    let generated = generated_path(&doc.entry);
    let links = managed_links(&doc.entry, entry_text(doc)?, &generated)?;
    if links.len() != 1 {
        return Err(link_count(links.len()));
    }
    let current = doc.files.get(&generated).ok_or_else(|| missing(&generated))?;
    let body = generated_body(&current.content).map_err(|_| source_changed(&generated))?;
    if body != render_body(tokens)? {
        return Err(invalid("WEB_TOKEN_DRIFT", format!("{GENERATED_NAME} does not match the pinned design-system tokens")));
    }
    Ok(())
}

/// The generated file that the content digest leaves out: the entry folder's file, while `generated_body` accepts it as unchanged.
/// An edit, an extra rule or a second selector, and a foreign file all stay in the content digest.
pub(crate) fn digest_excluded_path(doc: &WebDocument) -> Option<String> {
    let generated = generated_path(&doc.entry);
    generated_body(&doc.files.get(&generated)?.content).ok()?;
    Some(generated)
}

/// Token definitions of body rows. The body carries no names, so each name is its id; only the values are written back.
fn tokens_of(rows: &[(&str, &str, &str)]) -> Option<BTreeMap<String, TokenDef>> {
    rows.iter()
        .map(|&(id, light, dark)| Some((id.to_owned(), TokenDef { name: id.to_owned(), values: token_values(light, dark)? })))
        .collect()
}

/// Token rows of a generated body, in body order: (id, light value text, dark value text). Only the generator's block layout
/// is read here. `is_generator_layout` writes the body again from these values and compares, which settles the values.
fn body_rows(body: &str) -> Option<Vec<(&str, &str, &str)>> {
    let mut lines = body.strip_suffix('\n')?.split('\n');
    expect_line(&mut lines, ROOT_OPEN)?;
    let light = block_declarations(&mut lines, TOP_INDENT, BLOCK_CLOSE)?;
    expect_line(&mut lines, MEDIA_OPEN)?;
    expect_line(&mut lines, DARK_OPEN)?;
    let media = block_declarations(&mut lines, MEDIA_INDENT, DARK_CLOSE)?;
    expect_line(&mut lines, BLOCK_CLOSE)?;
    expect_line(&mut lines, ATTR_OPEN)?;
    let attr = block_declarations(&mut lines, TOP_INDENT, BLOCK_CLOSE)?;
    if lines.next().is_some() || light.len() != media.len() || light.len() != attr.len() {
        return None;
    }
    let mut rows = Vec::with_capacity(light.len());
    for ((id, light_value), ((media_id, dark_value), (attr_id, attr_value))) in light.into_iter().zip(media.into_iter().zip(attr)) {
        if id != media_id || id != attr_id || dark_value != attr_value {
            return None;
        }
        rows.push((id, light_value, dark_value));
    }
    Some(rows)
}

fn expect_line<'a>(lines: &mut impl Iterator<Item = &'a str>, text: &str) -> Option<()> {
    (lines.next()? == text).then_some(())
}

/// The declarations of one block, read up to the block's closing line.
fn block_declarations<'a>(lines: &mut impl Iterator<Item = &'a str>, indent: &str, close: &str) -> Option<Vec<(&'a str, &'a str)>> {
    let mut found = Vec::new();
    loop {
        let line = lines.next()?;
        if line == close {
            return Some(found);
        }
        found.push(declaration(line, indent)?);
    }
}

/// One declaration line: the block's indent, the property prefix, the id, `: `, the value and a final `;`.
fn declaration<'a>(line: &'a str, indent: &str) -> Option<(&'a str, &'a str)> {
    let rest = line.strip_prefix(indent)?.strip_prefix(PROPERTY_PREFIX)?;
    let (id, value) = rest.split_once(": ")?;
    let value = value.strip_suffix(';')?;
    (valid_css_name(id) && !value.is_empty()).then_some((id, value))
}

enum ValueShape {
    Color,
    Family,
    Length,
}

/// A quoted value is a font family, a value ending in `px` is a length, anything else is a colour.
fn value_shape(value: &str) -> ValueShape {
    if value.starts_with('"') {
        ValueShape::Family
    } else if value.ends_with("px") {
        ValueShape::Length
    } else {
        ValueShape::Color
    }
}

/// The values of one token, read from their text. Light and dark must have the same shape. A length is read as a spacing
/// value: the text does not say which length token it came from, and every length token writes the same text.
fn token_values(light: &str, dark: &str) -> Option<TokenValues> {
    Some(match (value_shape(light), value_shape(dark)) {
        (ValueShape::Color, ValueShape::Color) => TokenValues::Color { light: light.to_owned(), dark: dark.to_owned() },
        (ValueShape::Family, ValueShape::Family) => {
            TokenValues::FontFamily { light: family_name(light)?, dark: family_name(dark)? }
        }
        (ValueShape::Length, ValueShape::Length) => {
            TokenValues::Spacing { light: length_emu(light)?, dark: length_emu(dark)? }
        }
        _ => return None,
    })
}

/// The name inside a quoted font family value, with the generator's escapes read back.
fn family_name(value: &str) -> Option<String> {
    unescape_family(value.strip_prefix('"')?.strip_suffix(FAMILY_TAIL)?)
}

fn unescape_family(inner: &str) -> Option<String> {
    let mut out = String::with_capacity(inner.len());
    let mut rest = inner;
    while let Some(ch) = rest.chars().next() {
        let escape = FAMILY_ESCAPES.iter().find(|(_, escape)| rest.starts_with(*escape)).copied();
        if let Some((text, escape)) = escape {
            out.push(text);
            rest = &rest[escape.len()..];
        } else if ch.is_control() || FAMILY_ESCAPES.iter().any(|(text, _)| *text == ch) {
            return None;
        } else {
            out.push(ch);
            rest = &rest[ch.len_utf8()..];
        }
    }
    Some(out)
}

/// The EMU value whose `css_px` text is `text`, taken as the nearest whole EMU. `is_generator_layout` writes the text back from
/// that value, so a text that no EMU value writes is refused there.
fn length_emu(text: &str) -> Option<i64> {
    let number = text.strip_suffix("px")?;
    let (whole, fraction) = number.split_once('.').unwrap_or((number, ""));
    if whole.is_empty() || fraction.len() > 6 || !whole.bytes().chain(fraction.bytes()).all(|b| b.is_ascii_digit()) {
        return None;
    }
    let micro = whole
        .parse::<i128>()
        .ok()?
        .checked_mul(1_000_000)?
        .checked_add(format!("{fraction:0<6}").parse::<i128>().ok()?)?;
    let emu = micro.checked_mul(i128::from(EMU_PER_CSS_PX))?.checked_add(500_000)? / 1_000_000;
    i64::try_from(emu).ok()
}

fn web_file(content: String) -> WebFile {
    WebFile { sha256: sha256(content.as_bytes()), content }
}

fn link_count(found: usize) -> DltError {
    invalid("WEB_LINK_COUNT", format!("the entry must link {GENERATED_NAME} exactly once; found {found} links"))
}

fn missing(path: &str) -> DltError {
    let mut error = fail(ErrorCode::NotFound, "WEB_GENERATED_MISSING", format!("{path} is missing from the linked project"));
    error.affected_ids.push(path.to_owned());
    error
}

fn source_changed(path: &str) -> DltError {
    let mut error = fail(
        ErrorCode::SourceChanged,
        "SOURCE_CHANGED",
        format!("{path} was edited after it was written for the design system; the edit is kept"),
    );
    error.affected_ids.push(path.to_owned());
    error
}

fn name_clash(path: &str) -> DltError {
    let mut error = invalid(
        "WEB_NAME_CLASH",
        format!("{path} is a project file that the design system cannot replace; rename or remove it first"),
    );
    error.affected_ids.push(path.to_owned());
    error
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(values: TokenValues) -> BTreeMap<String, TokenDef> {
        BTreeMap::from([("primary".to_owned(), TokenDef { name: "Primary".into(), values })])
    }

    #[test]
    fn rendered_layout_puts_light_on_root_and_dark_in_both_dark_rules() {
        let css = render_body(&one(TokenValues::Color { light: "#1F6FEB".into(), dark: "#8AB4FF".into() })).unwrap();
        assert_eq!(
            css,
            ":root {\n  --dl-primary: #1F6FEB;\n}\n@media (prefers-color-scheme: dark) {\n  :root:not([data-theme=\"light\"]) {\n    --dl-primary: #8AB4FF;\n  }\n}\n:root[data-theme=\"dark\"] {\n  --dl-primary: #8AB4FF;\n}\n"
        );
    }

    #[test]
    fn lengths_are_css_pixels_converted_from_emu() {
        let css = render_body(&one(TokenValues::FontSize { light: 152_400, dark: 190_500 })).unwrap();
        assert!(css.contains("\n  --dl-primary: 16px;\n"));
        assert!(css.contains("\n    --dl-primary: 20px;\n"));
    }

    #[test]
    fn font_families_are_quoted_and_markup_is_escaped() {
        assert_eq!(css_family("Noto Sans KR").unwrap(), "\"Noto Sans KR\", sans-serif");
        let hostile = css_family("A\"B\\C</style>").unwrap();
        assert_eq!(hostile, "\"A\\\"B\\\\C\\3C /style\\3E \", sans-serif");
        assert!(!hostile.to_ascii_lowercase().contains("</style"));
        assert!(css_family("tab\there").is_err());
    }

    #[test]
    fn token_ids_must_be_css_names() {
        for id in ["Primary", "a.b", "a_b", "x</style>", ""] {
            let bad = BTreeMap::from([(id.to_owned(), TokenDef { name: "Bad".into(), values: TokenValues::Color { light: "#000000".into(), dark: "#FFFFFF".into() } })]);
            assert!(render_body(&bad).is_err(), "token id {id:?} must be refused");
        }
    }

    #[test]
    fn link_insertion_and_removal_restore_the_entry_exactly() {
        let entries = [
            "<!doctype html>\n<html>\n<head>\n<title>x</title>\n</head>\n<body></body>\n</html>\n",
            "<!doctype html>\r\n<html>\r\n<head>\r\n  <title>x</title>\r\n  </head>\r\n<body></body>\r\n</html>\r\n",
            "<html><head><title>x</title></head><body></body></html>",
        ];
        for entry in entries {
            let tags = html::tokenize(entry).unwrap();
            let (at, line) = link_insertion(entry, head_close(&tags).unwrap());
            let linked = format!("{}{}{}", &entry[..at], line, &entry[at..]);
            let link = managed_links("index.html", &linked, GENERATED_NAME).unwrap().remove(0);
            let (start, end) = link_removal(&linked, &link);
            assert_eq!(format!("{}{}", &linked[..start], &linked[end..]), entry);
        }
    }

    #[test]
    fn generated_body_tells_unchanged_edited_and_foreign_files_apart() {
        let content = generated_content(&one(TokenValues::Color { light: "#1F6FEB".into(), dark: "#8AB4FF".into() })).unwrap();
        assert!(generated_body(&content).is_ok());
        assert!(matches!(generated_body(&content.replace("#1F6FEB", "#000000")), Err(Origin::Edited)));
        assert!(matches!(generated_body("body { color: red; }"), Err(Origin::Foreign)));

        // A header recomputed for a body with one more rule is still an edit: the body must be the generator's layout.
        let body = content.split_once('\n').expect("header line").1;
        let extra = format!("{body}body {{ display: none; }}\n");
        let recomputed = format!("{HEADER_START}{}{HEADER_END}\n{extra}", sha256(extra.as_bytes()));
        assert!(matches!(generated_body(&recomputed), Err(Origin::Edited)));
    }
}
