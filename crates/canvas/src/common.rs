//! Shared helpers: diagnostics, identifiers, colors, CSS number text, HTML escaping, base64 (from contracts), and grapheme classes.

use designllm_contracts::{Diagnostic, DltError, DltResult, ErrorCode};

// EMU per CSS pixel at 96 dpi (1 inch = 914400 EMU), defined in contracts.
pub(crate) use designllm_contracts::EMU_PER_CSS_PX;
/// Largest absolute coordinate or dimension in EMU (10000 mm).
pub(crate) const MAX_EXTENT_EMU: i64 = 10_000 * designllm_contracts::EMU_PER_MM;
/// Maximum number of nodes in one page tree.
pub(crate) const MAX_NODES: usize = 5_000;
/// Maximum number of pages in one canvas.
pub(crate) const MAX_PAGES: usize = 500;
/// Maximum text length of one node, in bytes.
pub(crate) const MAX_TEXT_BYTES: usize = 200_000;

/// A blocking error with a stable diagnostic code.
pub(crate) fn fail(code: ErrorCode, diagnostic: &str, message: impl Into<String>) -> DltError {
    let message = message.into();
    let mut error = DltError::new(code, message.clone());
    error.diagnostics.push(Diagnostic { code: diagnostic.into(), message, node_id: None, blocking: true, params: Default::default() });
    error
}

pub(crate) fn invalid(diagnostic: &str, message: impl Into<String>) -> DltError {
    fail(ErrorCode::InvalidArgument, diagnostic, message)
}

pub(crate) fn unsupported(diagnostic: &str, message: impl Into<String>) -> DltError {
    fail(ErrorCode::UnsupportedOperation, diagnostic, message)
}

/// Titles are shown in lists and file names. Control characters are removed and the length is capped.
pub(crate) fn normalize_title_text(title: &str, default: &str) -> String {
    let cleaned: String = title.chars().filter(|c| !c.is_control()).collect();
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        default.to_string()
    } else {
        trimmed.chars().take(256).collect()
    }
}

pub(crate) fn info(code: &str, message: impl Into<String>) -> Diagnostic {
    Diagnostic { code: code.into(), message: message.into(), node_id: None, blocking: false, params: Default::default() }
}

pub(crate) fn blocking(code: &str, message: impl Into<String>, node_id: Option<String>) -> Diagnostic {
    Diagnostic { code: code.into(), message: message.into(), node_id, blocking: true, params: Default::default() }
}

/// Identifiers: 1 to 128 characters from ASCII letters, digits, `_`, `-`, `.`, `:`. UUIDs and test IDs fit.
pub(crate) fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b':'))
}

/// Variant and named-style keys: 1 to 64 characters from ASCII letters, digits, `_`, `-`, `.`.
pub(crate) fn valid_key(key: &str) -> bool {
    !key.is_empty() && key.len() <= 64 && key.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
}

/// `transparent`, `#RRGGBB`, or `#RRGGBBAA` (hex digits in any case).
pub(crate) fn valid_color(value: &str) -> bool {
    if value == "transparent" {
        return true;
    }
    match value.strip_prefix('#') {
        Some(hex) => (hex.len() == 6 || hex.len() == 8) && hex.bytes().all(|b| b.is_ascii_hexdigit()),
        None => false,
    }
}

/// Converts an EMU value to CSS pixels as an exact decimal with at most six fractional digits, without floats.
pub(crate) fn css_px(emu: i64) -> String {
    let scaled = i128::from(emu) * 1_000_000;
    let denom = i128::from(EMU_PER_CSS_PX);
    let rounded = if scaled >= 0 { (scaled + denom / 2) / denom } else { -((-scaled + denom / 2) / denom) };
    let sign = if rounded < 0 { "-" } else { "" };
    let abs = rounded.abs();
    let whole = abs / 1_000_000;
    let frac = abs % 1_000_000;
    if frac == 0 {
        format!("{sign}{whole}")
    } else {
        let text = format!("{frac:06}");
        format!("{sign}{whole}.{}", text.trim_end_matches('0'))
    }
}

/// Percentage text (for absolute children of a box with a known size) with at most six decimals.
pub(crate) fn percent(part: i64, whole: i64) -> String {
    if whole <= 0 {
        return "0".into();
    }
    let scaled = i128::from(part) * 100_000_000;
    let denom = i128::from(whole);
    let rounded = if scaled >= 0 { (scaled + denom / 2) / denom } else { -((-scaled + denom / 2) / denom) };
    let sign = if rounded < 0 { "-" } else { "" };
    let abs = rounded.abs();
    let whole_part = abs / 1_000_000;
    let frac = abs % 1_000_000;
    if frac == 0 {
        format!("{sign}{whole_part}")
    } else {
        let text = format!("{frac:06}");
        format!("{sign}{whole_part}.{}", text.trim_end_matches('0'))
    }
}

/// Pixels at 96 dpi for a page or viewport width, rounded half up.
pub(crate) fn viewport_px(emu: i64) -> u32 {
    let px = (i128::from(emu) * 96 + 457_200) / 914_400;
    px.clamp(0, i128::from(u32::MAX)) as u32
}

pub(crate) fn escape_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            other => out.push(other),
        }
    }
    out
}

/// Escapes text that is placed between tags. Newlines become `<br>` so that layout matches the scene.
pub(crate) fn escape_text_with_breaks(text: &str) -> String {
    escape_html(text).replace('\n', "<br>")
}

/// Escapes a JSON document for embedding in a `<script type="application/json">` block.
pub(crate) fn script_json(value: &serde_json::Value) -> DltResult<String> {
    let text = serde_json::to_string(value).map_err(|e| invalid("CANVAS_EXPORT_JSON", e.to_string()))?;
    Ok(text.replace('<', "\\u003c").replace('>', "\\u003e").replace('&', "\\u0026"))
}

// Standard padded base64: the shared implementation in contracts.
pub(crate) use designllm_contracts::base64::encode as base64_encode;

/// The canvas refusal of text that is not strict padded base64: InvalidArgument under `CANVAS_BASE64`.
pub(crate) fn base64_refusal(error: designllm_contracts::base64::DecodeError) -> DltError {
    use designllm_contracts::base64::DecodeError;
    let message = match error {
        DecodeError::Length => "base64 length is not a multiple of 4",
        DecodeError::MisplacedPadding => "misplaced base64 padding",
        DecodeError::DataAfterPadding => "base64 data after padding",
        DecodeError::InvalidCharacter => "base64 contains an invalid character",
    };
    invalid("CANVAS_BASE64", message)
}

/// Grapheme classes used for line-break opportunities. Hangul and Latin break only at spaces.
pub(crate) fn is_ideograph_or_kana(grapheme: &str) -> bool {
    grapheme.chars().next().is_some_and(|c| {
        matches!(c as u32,
            0x3040..=0x30FF   // Hiragana, Katakana
            | 0x3400..=0x4DBF // CJK Extension A
            | 0x4E00..=0x9FFF // CJK Unified Ideographs
            | 0xF900..=0xFAFF // CJK Compatibility Ideographs
            | 0x20000..=0x2FFFF // CJK Extensions B and later
        )
    })
}

pub(crate) fn is_space(grapheme: &str) -> bool {
    grapheme.chars().all(|c| c == ' ' || c == '\t' || c == '\u{3000}')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn css_px_is_exact_decimal() {
        assert_eq!(css_px(9_525), "1");
        assert_eq!(css_px(4_762), "0.499948");
        assert_eq!(css_px(1), "0.000105");
        assert_eq!(css_px(12_192_000), "1280");
        assert_eq!(css_px(-9_525), "-1");
    }

    #[test]
    fn viewport_is_half_up_pixels() {
        assert_eq!(viewport_px(12_192_000), 1280);
        assert_eq!(viewport_px(914_400), 96);
    }

    #[test]
    fn percent_rounds_to_six_decimals() {
        assert_eq!(percent(1, 3), "33.333333");
        assert_eq!(percent(50, 100), "50");
        assert_eq!(percent(0, 0), "0");
    }

    #[test]
    fn base64_round_trips_all_padding_cases() {
        use designllm_contracts::base64::decode;
        for input in [&b""[..], b"f", b"fo", b"foo", b"foob", b"fooba", b"foobar"] {
            let text = base64_encode(input);
            assert_eq!(decode(&text).unwrap(), input);
        }
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64_encode(b"f"), "Zg==");
        let refusal = |text: &str| base64_refusal(decode(text).expect_err(text));
        let err = refusal("Zg=x");
        assert_eq!((err.code, err.diagnostics[0].code.as_str(), err.message.as_str()), (ErrorCode::InvalidArgument, "CANVAS_BASE64", "base64 data after padding"));
        assert_eq!(refusal("Zm9").message, "base64 length is not a multiple of 4");
        assert_eq!(refusal("=m9v").message, "misplaced base64 padding");
        assert_eq!(refusal("Zm9*").message, "base64 contains an invalid character");
    }

    #[test]
    fn colors_and_ids_are_checked() {
        assert!(valid_color("#20252D"));
        assert!(valid_color("#20252D80"));
        assert!(valid_color("transparent"));
        assert!(!valid_color("red"));
        assert!(!valid_color("#GGGGGG"));
        assert!(valid_id("node-1"));
        assert!(!valid_id("bad id"));
        assert!(!valid_id(""));
    }

    #[test]
    fn html_escaping_covers_markup_characters() {
        assert_eq!(escape_html("<a href=\"x\">&'"), "&lt;a href=&quot;x&quot;&gt;&amp;&#39;");
        assert_eq!(escape_text_with_breaks("a\nb"), "a<br>b");
    }
}
