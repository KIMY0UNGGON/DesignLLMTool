//! Math source of a figure Diagram math node (`NodeKind::Text` with `NodeExt.math = tex_subset`; the source is
//! `Node.text`): the parser, its limits and the diagnostic codes. The figure engine typesets the parsed items and the
//! desktop checks a source with the same parser before it sends a change, so both read one grammar.
//!
//! The grammar is a TeX subset:
//! - plain characters: ASCII letters and digits, Hangul syllables, and the punctuation in [`MATH_PUNCTUATION`];
//!   spaces separate tokens and are not drawn (as in TeX math), `\,` is a thin space
//! - `^` and `_` with a braced argument or a single character or symbol (`x^2`, `x_{i+1}`, `x^\alpha`); a base may
//!   carry one of each (`x_i^2`)
//! - `{…}` groups, `\frac{…}{…}`, `\sqrt{…}`
//! - the commands in [`MATH_SYMBOLS`] (Greek letters and symbols) and `\{` `\}`
//!
//! Anything else is refused as a whole with [`FIGURE_MATH_UNSUPPORTED`]: other commands (among them `\mp`, `\equiv`,
//! `\propto` and `\nabla`, whose glyphs the bundled fonts lack), an unmatched brace, an empty script, fraction part or
//! root, two superscripts or two subscripts on one base, control characters, and an empty source. A source over
//! [`MATH_MAX_BYTES`], [`MATH_MAX_TOKENS`] or [`MATH_MAX_DEPTH`] is refused with [`FIGURE_MATH_LIMIT`]. Nothing is
//! guessed or partly parsed. The work is bounded by the limits: the byte limit is checked first, the lexer stops at
//! the token after the last allowed one, and the nesting depth is checked before each descent.
//!
//! A refusal is a `DltError` with one blocking diagnostic whose `code` is one of the two codes and whose
//! `params["byte"]` is the byte offset of the offending token (the limit refusals also carry `params["limit"]` and
//! `params["measure"]`: `bytes`, `tokens` or `depth`). FIGURE_MATH_UNSUPPORTED is UNSUPPORTED_OPERATION and
//! FIGURE_MATH_LIMIT is INVALID_ARGUMENT.
use crate::{Diagnostic, DltError, DltResult, ErrorCode};
use std::collections::BTreeMap;

/// Most bytes a math source may hold.
pub const MATH_MAX_BYTES: usize = 1024;
/// Most tokens a math source may hold. A token is a character, a command, a brace, `^` or `_`; spaces are not tokens.
pub const MATH_MAX_TOKENS: usize = 512;
/// Deepest nesting of groups, script arguments, fraction parts and roots. The top level is depth 0.
pub const MATH_MAX_DEPTH: usize = 8;

/// A math source outside the grammar: an unknown command, an unmatched brace, an empty script and the like.
pub const FIGURE_MATH_UNSUPPORTED: &str = "FIGURE_MATH_UNSUPPORTED";
/// A math source over one of the limits.
pub const FIGURE_MATH_LIMIT: &str = "FIGURE_MATH_LIMIT";
/// Non-blocking export diagnostic: the figure has math and the format keeps its characters and lines but not its
/// structure (PPTX).
pub const FIGURE_MATH_FLATTENED: &str = "FIGURE_MATH_FLATTENED";

/// Plain punctuation drawn as is, besides letters and digits.
pub const MATH_PUNCTUATION: &str = "+-=<>/*,.;:!'|()[]";

/// Every command that names one character, with the character drawn for it. `\cdot` is U+00B7 (the bundled fonts have
/// no U+22C5).
pub const MATH_SYMBOLS: [(&str, char); 51] = [
    ("alpha", '\u{03B1}'), ("beta", '\u{03B2}'), ("gamma", '\u{03B3}'), ("delta", '\u{03B4}'), ("epsilon", '\u{03B5}'),
    ("zeta", '\u{03B6}'), ("eta", '\u{03B7}'), ("theta", '\u{03B8}'), ("iota", '\u{03B9}'), ("kappa", '\u{03BA}'),
    ("lambda", '\u{03BB}'), ("mu", '\u{03BC}'), ("nu", '\u{03BD}'), ("xi", '\u{03BE}'), ("pi", '\u{03C0}'),
    ("rho", '\u{03C1}'), ("sigma", '\u{03C3}'), ("tau", '\u{03C4}'), ("upsilon", '\u{03C5}'), ("phi", '\u{03C6}'),
    ("chi", '\u{03C7}'), ("psi", '\u{03C8}'), ("omega", '\u{03C9}'),
    ("Gamma", '\u{0393}'), ("Delta", '\u{0394}'), ("Theta", '\u{0398}'), ("Lambda", '\u{039B}'), ("Xi", '\u{039E}'),
    ("Pi", '\u{03A0}'), ("Sigma", '\u{03A3}'), ("Upsilon", '\u{03A5}'), ("Phi", '\u{03A6}'), ("Psi", '\u{03A8}'),
    ("Omega", '\u{03A9}'),
    ("pm", '\u{00B1}'), ("times", '\u{00D7}'), ("cdot", '\u{00B7}'), ("div", '\u{00F7}'), ("le", '\u{2264}'),
    ("ge", '\u{2265}'), ("ne", '\u{2260}'), ("approx", '\u{2248}'), ("infty", '\u{221E}'), ("partial", '\u{2202}'),
    ("sum", '\u{2211}'), ("prod", '\u{220F}'), ("int", '\u{222B}'), ("to", '\u{2192}'), ("leftarrow", '\u{2190}'),
    ("rightarrow", '\u{2192}'), ("degree", '\u{00B0}'),
];

/// One parsed item of a math source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MathItem {
    /// One character drawn as is: a letter, a digit, punctuation, or the character a command names (`\alpha` is
    /// U+03B1, `\{` is `{`).
    Glyph(char),
    /// `\,`.
    ThinSpace,
    /// `{…}`, drawn as one unit. It may be empty (`{}^{14}C`).
    Group(Vec<MathItem>),
    /// A base with a subscript, a superscript, or both. The base is the item before the first `^` or `_`, or an
    /// empty group when there is none.
    Scripts { base: Box<MathItem>, sub: Option<Vec<MathItem>>, sup: Option<Vec<MathItem>> },
    /// `\frac{numerator}{denominator}`.
    Fraction { numerator: Vec<MathItem>, denominator: Vec<MathItem> },
    /// `\sqrt{radicand}`.
    Root(Vec<MathItem>),
}

/// The character a symbol command names (`"alpha"` gives U+03B1), or `None` for any other name.
pub fn math_symbol(name: &str) -> Option<char> {
    MATH_SYMBOLS.iter().find(|(command, _)| *command == name).map(|(_, glyph)| *glyph)
}

/// Parses a math source into its items. See the module documentation for the grammar, the limits and the refusals.
pub fn parse(source: &str) -> DltResult<Vec<MathItem>> {
    if source.len() > MATH_MAX_BYTES {
        return Err(limit("math source is longer than the byte limit", "bytes", MATH_MAX_BYTES, MATH_MAX_BYTES));
    }
    let tokens = lex(source)?;
    if tokens.is_empty() {
        return Err(unsupported("math source is empty", 0));
    }
    let mut parser = Parser { tokens: &tokens, next: 0, end: source.len() };
    let items = parser.list(0, None)?;
    Ok(items)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tok { Glyph(char), ThinSpace, Open, Close, Sup, Sub, Frac, Sqrt }

#[derive(Debug, Clone, Copy)]
struct Token { tok: Tok, byte: usize }

fn plain(c: char) -> bool {
    c.is_ascii_alphanumeric() || MATH_PUNCTUATION.contains(c) || ('\u{AC00}'..='\u{D7A3}').contains(&c)
}

fn lex(source: &str) -> DltResult<Vec<Token>> {
    let mut tokens = Vec::new();
    let mut chars = source.char_indices().peekable();
    while let Some((byte, c)) = chars.next() {
        let tok = match c {
            ' ' => continue,
            '{' => Tok::Open,
            '}' => Tok::Close,
            '^' => Tok::Sup,
            '_' => Tok::Sub,
            '\\' => match chars.peek().copied() {
                Some((_, ',')) => { chars.next(); Tok::ThinSpace }
                Some((_, '{')) => { chars.next(); Tok::Glyph('{') }
                Some((_, '}')) => { chars.next(); Tok::Glyph('}') }
                Some((start, letter)) if letter.is_ascii_alphabetic() => {
                    let mut stop = start;
                    while let Some(&(at, next)) = chars.peek() {
                        if !next.is_ascii_alphabetic() { break; }
                        stop = at + next.len_utf8();
                        chars.next();
                    }
                    match &source[start..stop] {
                        "frac" => Tok::Frac,
                        "sqrt" => Tok::Sqrt,
                        name => match math_symbol(name) {
                            Some(glyph) => Tok::Glyph(glyph),
                            None => return Err(unsupported(&format!("math command \\{name} is not supported"), byte)),
                        },
                    }
                }
                Some((_, other)) => return Err(unsupported(&format!("math command \\{} is not supported", other.escape_debug()), byte)),
                None => return Err(unsupported("math source ends with a backslash", byte)),
            },
            c if plain(c) => Tok::Glyph(c),
            c => return Err(unsupported(&format!("math character {:?} is not supported", c), byte)),
        };
        if tokens.len() == MATH_MAX_TOKENS {
            return Err(limit("math source has more tokens than the limit", "tokens", MATH_MAX_TOKENS, byte));
        }
        tokens.push(Token { tok, byte });
    }
    Ok(tokens)
}

struct Parser<'a> {
    tokens: &'a [Token],
    next: usize,
    /// Byte offset reported for a problem at the end of the source.
    end: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Option<Token> {
        self.tokens.get(self.next).copied()
    }

    fn here(&self) -> usize {
        self.peek().map_or(self.end, |token| token.byte)
    }

    /// The items up to the matching `}` (`open` is the byte of its `{`) or, at the top level, the end.
    fn list(&mut self, depth: usize, open: Option<usize>) -> DltResult<Vec<MathItem>> {
        let mut items: Vec<MathItem> = Vec::new();
        loop {
            let Some(token) = self.peek() else {
                return match open {
                    Some(byte) => Err(unsupported("math source has an unmatched {", byte)),
                    None => Ok(items),
                };
            };
            match token.tok {
                Tok::Close => {
                    if open.is_none() {
                        return Err(unsupported("math source has an unmatched }", token.byte));
                    }
                    self.next += 1;
                    return Ok(items);
                }
                Tok::Sup | Tok::Sub => {
                    self.next += 1;
                    let script = self.script(depth, token)?;
                    let superscript = token.tok == Tok::Sup;
                    // A script right after a script goes on the same base (`x_i^2`).
                    if let Some(MathItem::Scripts { sub, sup, .. }) = items.last_mut() {
                        let slot = if superscript { sup } else { sub };
                        if slot.is_some() {
                            return Err(unsupported("math base has two superscripts or two subscripts", token.byte));
                        }
                        *slot = Some(script);
                        continue;
                    }
                    let base = items.pop().unwrap_or(MathItem::Group(Vec::new()));
                    let (sub, sup) = if superscript { (None, Some(script)) } else { (Some(script), None) };
                    items.push(MathItem::Scripts { base: Box::new(base), sub, sup });
                }
                _ => {
                    let item = self.atom(depth)?;
                    items.push(item);
                }
            }
        }
    }

    /// One item that is not a script.
    fn atom(&mut self, depth: usize) -> DltResult<MathItem> {
        let token = self.peek().expect("atom is called on a token");
        self.next += 1;
        match token.tok {
            Tok::Glyph(c) => Ok(MathItem::Glyph(c)),
            Tok::ThinSpace => Ok(MathItem::ThinSpace),
            Tok::Open => Ok(MathItem::Group(self.nested(depth, token.byte)?)),
            Tok::Frac => {
                let numerator = self.argument(depth, "fraction numerator")?;
                let denominator = self.argument(depth, "fraction denominator")?;
                Ok(MathItem::Fraction { numerator, denominator })
            }
            Tok::Sqrt => Ok(MathItem::Root(self.argument(depth, "root")?)),
            Tok::Close | Tok::Sup | Tok::Sub => unreachable!("list handles braces and scripts"),
        }
    }

    /// The items of a group whose `{` (at `open`) was just read, one level deeper.
    fn nested(&mut self, depth: usize, open: usize) -> DltResult<Vec<MathItem>> {
        if depth + 1 > MATH_MAX_DEPTH {
            return Err(limit("math source nests deeper than the limit", "depth", MATH_MAX_DEPTH, open));
        }
        self.list(depth + 1, Some(open))
    }

    /// A braced, non-empty argument of `\frac` or `\sqrt`.
    fn argument(&mut self, depth: usize, what: &str) -> DltResult<Vec<MathItem>> {
        let at = self.here();
        match self.peek() {
            Some(Token { tok: Tok::Open, byte }) => {
                self.next += 1;
                let items = self.nested(depth, byte)?;
                if items.is_empty() {
                    return Err(unsupported(&format!("math {what} is empty"), byte));
                }
                Ok(items)
            }
            _ => Err(unsupported(&format!("math {what} needs a braced argument"), at)),
        }
    }

    /// The argument of the `^` or `_` just read: a non-empty braced group, or one character or symbol.
    fn script(&mut self, depth: usize, operator: Token) -> DltResult<Vec<MathItem>> {
        match self.peek() {
            Some(Token { tok: Tok::Open, byte }) => {
                self.next += 1;
                let items = self.nested(depth, byte)?;
                if items.is_empty() {
                    return Err(unsupported("math script is empty", operator.byte));
                }
                Ok(items)
            }
            // The depth refusal points at the argument, as it does at the `{` of a braced one.
            Some(Token { tok: Tok::Glyph(c), byte }) => {
                if depth + 1 > MATH_MAX_DEPTH {
                    return Err(limit("math source nests deeper than the limit", "depth", MATH_MAX_DEPTH, byte));
                }
                self.next += 1;
                Ok(vec![MathItem::Glyph(c)])
            }
            Some(Token { tok: Tok::Frac | Tok::Sqrt, byte }) => {
                Err(unsupported("a fraction or root in a script needs braces around it", byte))
            }
            Some(Token { tok: Tok::ThinSpace | Tok::Sup | Tok::Sub, byte }) => {
                Err(unsupported("a math script needs a braced group or one character or symbol", byte))
            }
            Some(Token { tok: Tok::Close, .. }) | None => Err(unsupported("math script is empty", operator.byte)),
        }
    }
}

fn refusal(code: ErrorCode, diagnostic: &str, message: &str, byte: usize) -> DltError {
    let mut params = BTreeMap::new();
    params.insert("byte".to_owned(), serde_json::json!(byte));
    let mut error = DltError::new(code, message);
    error.diagnostics.push(Diagnostic { code: diagnostic.to_owned(), message: message.to_owned(), node_id: None, blocking: true, params });
    error
}

fn unsupported(message: &str, byte: usize) -> DltError {
    refusal(ErrorCode::UnsupportedOperation, FIGURE_MATH_UNSUPPORTED, message, byte)
}

fn limit(message: &str, measure: &str, limit: usize, byte: usize) -> DltError {
    let mut error = refusal(ErrorCode::InvalidArgument, FIGURE_MATH_LIMIT, message, byte);
    let params = &mut error.diagnostics[0].params;
    params.insert("limit".to_owned(), serde_json::json!(limit));
    params.insert("measure".to_owned(), serde_json::json!(measure));
    error
}
