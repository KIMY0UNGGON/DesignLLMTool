//! Line break opportunities between graphemes: a documented subset of UAX #14 with Korean
//! word-based (keep-all) breaking.
//!
//! - Never break before a space; break after spaces.
//! - No break before closing punctuation / CJK "no start" characters, none after opening
//!   punctuation.
//! - Break around ideographs, kana, fullwidth letters, emoji and inline atoms (U+FFFC).
//! - Hangul syllables/jamo and Latin letters/digits keep together (CSS `word-break: keep-all`);
//!   Korean lines break at spaces between word units (어절).
//! - Break after a hyphen that is followed by a letter.
//! Forced breaks (LF, CR, LS, PS, VT, FF, NEL) are handled by the caller.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BreakClass { Space, Open, Close, Ideographic, Hangul, Hyphen, Other }

const NO_START: &str = ")]}>,.;:!?%\u{00BB}\u{2019}\u{201D}\u{2026}\u{2025}\u{3001}\u{3002}\u{FF0C}\u{FF0E}\u{30FB}\u{FF1A}\u{FF1B}\u{FF1F}\u{FF01}\u{30FC}\u{3005}\u{301C}\u{FF5E}\u{FF09}\u{3015}\u{FF3D}\u{FF5D}\u{3009}\u{300B}\u{300D}\u{300F}\u{3011}\u{3019}\u{3017}\u{301F}\u{FF60}\u{3041}\u{3043}\u{3045}\u{3047}\u{3049}\u{3063}\u{3083}\u{3085}\u{3087}\u{308E}\u{30A1}\u{30A3}\u{30A5}\u{30A7}\u{30A9}\u{30C3}\u{30E3}\u{30E5}\u{30E7}\u{30EE}\u{30F5}\u{30F6}";
const NO_END: &str = "([{<\u{00AB}\u{2018}\u{201C}\u{FF08}\u{3014}\u{FF3B}\u{FF5B}\u{3008}\u{300A}\u{300C}\u{300E}\u{3010}\u{3018}\u{3016}\u{301D}\u{FF5F}";

pub fn classify(c: char) -> BreakClass {
    if NO_START.contains(c) { return BreakClass::Close; }
    if NO_END.contains(c) { return BreakClass::Open; }
    if c == '-' || c == '\u{2010}' { return BreakClass::Hyphen; }
    if is_space(c) { return BreakClass::Space; }
    let u = c as u32;
    if (0xAC00..=0xD7A3).contains(&u) || (0x1100..=0x11FF).contains(&u) || (0x3130..=0x318F).contains(&u) || (0xA960..=0xA97F).contains(&u) || (0xD7B0..=0xD7FF).contains(&u) {
        return BreakClass::Hangul;
    }
    let ideographic = (0x2E80..=0x2FFF).contains(&u) || (0x3000..=0x303F).contains(&u) || (0x3040..=0x30FF).contains(&u)
        || (0x31F0..=0x31FF).contains(&u) || (0x3400..=0x4DBF).contains(&u) || (0x4E00..=0x9FFF).contains(&u)
        || (0xF900..=0xFAFF).contains(&u) || (0xFF01..=0xFF60).contains(&u) || (0x20000..=0x3FFFF).contains(&u)
        || (0x1F300..=0x1FAFF).contains(&u) || (0x2600..=0x27BF).contains(&u) || u == 0xFFFC;
    if ideographic { BreakClass::Ideographic } else { BreakClass::Other }
}

/// Breaking white space (not NBSP, narrow NBSP or figure space).
pub fn is_space(c: char) -> bool {
    c == ' ' || c == '\u{3000}' || (c.is_whitespace() && !matches!(c, '\u{00A0}' | '\u{202F}' | '\u{2007}' | '\t' | '\n' | '\r' | '\u{000B}' | '\u{000C}' | '\u{0085}' | '\u{2028}' | '\u{2029}'))
}

/// Break opportunity between grapheme `prev` and grapheme `next`.
pub fn break_between(prev: &str, next: &str) -> bool {
    let (Some(a), Some(b)) = (prev.chars().next(), next.chars().next()) else { return false };
    let (ca, cb) = (classify(a), classify(b));
    if cb == BreakClass::Space || cb == BreakClass::Close || ca == BreakClass::Open { return false; }
    if ca == BreakClass::Space { return true; }
    if ca == BreakClass::Hyphen { return b.is_alphabetic(); }
    ca == BreakClass::Ideographic || cb == BreakClass::Ideographic
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn korean_keeps_words_cjk_breaks_and_punctuation_holds() {
        assert!(!break_between("한", "글"));
        assert!(break_between(" ", "한"));
        assert!(!break_between("a", " "));
        assert!(break_between("中", "文"));
        assert!(!break_between("文", "\u{3002}"));
        assert!(!break_between("(", "a"));
        assert!(break_between("-", "b"));
        assert!(!break_between("-", "5"));
        assert!(break_between("a", "\u{FFFC}"));
        assert!(!break_between("a", "b"));
    }
}
