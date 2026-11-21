//! The built-in encoding of an embedded Type 1 font program (ISO 32000-1 9.6.6.2). It is the `/Encoding` of the program's cleartext
//! part, before the `eexec` operator. The encrypted part after `eexec` is never decrypted or read.
//!
//! Only these forms are read: `/Encoding StandardEncoding def`, or `/Encoding 256 array`, then the optional loop that sets every
//! code to .notdef, then `dup <code> /<name> put` entries, then `readonly def` or `def`. The loop is accepted only as the token
//! sequence of `0 1 255 {1 index exch /.notdef put} for` (white space and comments aside); any other procedure or loop is refused.
//! Any other token in the array, a code outside 0..255, a missing end, or a second /Encoding breaks the table. The program then has
//! no built-in encoding, and the codes that need it have no text, as they do without an encoding. A name that the Adobe Glyph List
//! does not map leaves only its own code without text. The names are not checked against the glyphs of the program, because its
//! charstrings are encrypted and not read.

use super::{is_delimiter, tokenize, Budget, Token};
use crate::content::Meters;
use crate::error::PResult;
use crate::glyph_names;

/// Most cleartext bytes, before `eexec`, that a Type 1 program may have for its encoding to be read.
const MAX_CLEARTEXT_BYTES: usize = 1 << 20;
/// The operator that ends the cleartext part.
const EEXEC: &[u8] = b"eexec";
/// The loop that sets every code of an encoding array to .notdef, as a token sequence. A font program writes it right after
/// `256 array`. Comments and white space are not tokens, so they do not change the match.
const NOTDEF_LOOP: [&str; 11] = ["0", "1", "255", "{", "1", "index", "exch", "/.notdef", "put", "}", "for"];

/// The glyph names of the built-in encoding of a Type 1 program, by code. An empty name is a code that the encoding leaves to
/// .notdef. None when the program has no encoding that can be read. A budget refusal of the page or the request is an error.
pub(crate) fn read_builtin_encoding(program: &[u8], meters: Meters<'_>) -> PResult<Option<Vec<String>>> {
    let Some(cleartext) = cleartext(program) else {
        return Ok(None);
    };
    let mut budget = Budget::new(meters);
    match parse_encoding(cleartext, &mut budget) {
        Some(names) => Ok(Some(names)),
        None => match budget.refused.take() {
            Some(refusal) => Err(refusal),
            None => Ok(None),
        },
    }
}

/// The bytes before the first `eexec` operator, or None when there is none, or when they are over the limit.
fn cleartext(program: &[u8]) -> Option<&[u8]> {
    // The operator is a token of the cleartext. Its start is at most the limit, because the search stops past it.
    let end = eexec_operator(program)?;
    Some(&program[..end])
}

/// The offset of the first `eexec` operator that is a token, or None when there is none, or when a string before it is not
/// closed. Comments, literal strings, hex strings and names are skipped as tokens, so the word inside them is not the operator.
/// Only tokens that start within the limit are read, so an operator past it is not found. A `)` or `>` of the encrypted part can
/// close a string that the cleartext leaves open, and the scan then reads on in the encrypted bytes, where it finds the operator
/// only if they spell it. The font that needs an encoding that is not read reports it, whatever the reason (`font.rs`,
/// PDF_TYPE1_ENCODING_UNREADABLE).
fn eexec_operator(program: &[u8]) -> Option<usize> {
    let mut i = 0;
    while i < program.len() && i <= MAX_CLEARTEXT_BYTES {
        i = match program[i] {
            b' ' | b'\t' | b'\r' | b'\n' | 0 | 12 => i + 1,
            b'%' => program[i..].iter().position(|&b| b == b'\n' || b == b'\r').map_or(program.len(), |p| i + p),
            b'(' => super::string_end(program, i)?,
            b'<' if program.get(i + 1) == Some(&b'<') => i + 2,
            b'<' => i + program[i..].iter().position(|&b| b == b'>')? + 1,
            b'/' => regular_end(program, i + 1),
            b')' | b'>' | b'[' | b']' | b'{' | b'}' => i + 1,
            _ => {
                let end = regular_end(program, i);
                if &program[i..end] == EEXEC {
                    return Some(i);
                }
                end
            }
        };
    }
    None
}

/// The index just past the regular token that starts at `i`: the bytes up to the next delimiter, or the end of the data.
fn regular_end(data: &[u8], i: usize) -> usize {
    data[i..].iter().position(|&b| is_delimiter(b)).map_or(data.len(), |p| i + p)
}

/// The glyph names of the encoding in the cleartext. None when there is none, or when it is not one of the accepted forms.
fn parse_encoding(cleartext: &[u8], budget: &mut Budget<'_>) -> Option<Vec<String>> {
    let tokens = tokenize(cleartext, budget).ok()?;
    let word = |i: usize, w: &str| matches!(tokens.get(i), Some(Token::Keyword(k)) if k == w);
    // The program names its encoding once. A second /Encoding is not chosen between, so it refuses the program.
    let mut keys = (0..tokens.len()).filter(|&i| word(i, "/Encoding"));
    let key = keys.next()?;
    if keys.next().is_some() {
        return None;
    }
    if word(key + 1, "StandardEncoding") {
        return word(key + 2, "def").then(|| glyph_names::STANDARD.iter().map(|name| name.to_string()).collect());
    }
    if !(word(key + 1, "256") && word(key + 2, "array")) {
        return None;
    }
    let mut names = vec![String::new(); 256];
    let mut i = key + 3;
    if (0..NOTDEF_LOOP.len()).all(|k| word(i + k, NOTDEF_LOOP[k])) {
        i += NOTDEF_LOOP.len();
    }
    loop {
        if word(i, "dup") {
            let code = code_at(&tokens, i + 1)?;
            let Some(Token::Keyword(name)) = tokens.get(i + 2) else {
                return None;
            };
            let name = name.strip_prefix('/')?;
            if !word(i + 3, "put") {
                return None;
            }
            names[code] = name.to_string();
            i += 4;
        } else if word(i, "def") || (word(i, "readonly") && word(i + 1, "def")) {
            return Some(names);
        } else {
            return None;
        }
    }
}

/// The code of a `dup` entry: a whole number of digits from 0 to 255.
fn code_at(tokens: &[Token], i: usize) -> Option<usize> {
    match tokens.get(i) {
        Some(Token::Keyword(k)) if !k.is_empty() && k.bytes().all(|b| b.is_ascii_digit()) => {
            k.parse::<usize>().ok().filter(|code| *code < 256)
        }
        _ => None,
    }
}

#[cfg(test)]
#[path = "type1_encoding_tests.rs"]
mod tests;
