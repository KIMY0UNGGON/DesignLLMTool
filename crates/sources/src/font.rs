//! Font programs for text extraction: simple fonts and Type0 fonts with Identity-H.
//! Embedded font programs are not parsed, except the cleartext /Encoding of an embedded Type 1 program (type1_encoding), which
//! names the glyphs of the codes that the font's own encoding leaves to it. Text comes from /ToUnicode, or for a simple font from
//! the glyph names of its encoding (ISO 32000-1 9.10.2, through the Adobe Glyph List). Anything else returns an explicit
//! unsupported error instead of guessing text.

use crate::boxes::resolve;
use crate::builtin::{self, Standard};
use crate::content::{Meters, Unit};
use crate::decode::decode_complete;
use crate::error::{PResult, PdfErr};
use crate::glyph_names;
use lopdf::{Dictionary, Document, Object, Stream};
use std::collections::{BTreeMap, HashMap};

#[path = "type1_encoding.rs"]
mod type1_encoding;

/// Ascent and descent fallback in 1/1000 em when neither the font descriptor nor a built-in table provides them.
const DEFAULT_ASCENT: f64 = 800.0;
const DEFAULT_DESCENT: f64 = -200.0;

/// Text of each character code of a ToUnicode CMap that was accepted whole. None marks a code that a sequential range refuses
/// (see `bfrange_block`): it has no text, and the encoding must not supply one.
type CodeMap = HashMap<u32, Option<String>>;
/// A ToUnicode CMap that was accepted whole: its mappings, and how many sequential ranges pass 0xFF.
#[derive(Debug)]
pub(crate) struct ToUnicodeCMap {
    codes: CodeMap,
    /// Sequential ranges whose destination would pass 0xFF in its last byte. Each counts once. Only the codes past 0xFF are refused;
    /// the other codes of the range keep their text.
    ignored: u32,
}
/// Parsed ToUnicode CMap. `Err` means the whole CMap is rejected, so no code of the font is read from it.
type ToUnicodeMap = Result<ToUnicodeCMap, &'static str>;
/// Marks the one byte left at the end of a two-byte operand. Two-byte codes are at most 0xFFFF, so a marked value
/// equals no code of a Type0 font.
const TRUNCATED_CODE: u32 = 0x0001_0000;
/// The largest code of a two-byte font. A /W entry past it names no code that a string can show.
const MAX_CID: u64 = 0xFFFF;

/// Advance widths of a CIDFont by CID, as intervals. A range of /W is one interval, so a range of any length costs one entry.
/// Later entries take precedence over earlier ones on the codes that they share, as they did when each code was stored.
#[derive(Debug, Default, Clone, PartialEq)]
pub(crate) struct CidWidths {
    /// Start of each interval: (last code, width). The intervals do not overlap.
    spans: BTreeMap<u32, (u32, f64)>,
}

impl CidWidths {
    /// Gives the codes `first` to `last`, both included, the width `width`. Requires `first <= last`.
    fn assign(&mut self, first: u32, last: u32, width: f64) {
        let mut overlapping: Vec<(u32, u32, f64)> = Vec::new();
        if let Some((&start, &(end, w))) = self.spans.range(..first).next_back() {
            if end >= first {
                overlapping.push((start, end, w));
            }
        }
        for (&start, &(end, w)) in self.spans.range(first..=last) {
            overlapping.push((start, end, w));
        }
        for (start, end, w) in overlapping {
            self.spans.remove(&start);
            if start < first {
                self.spans.insert(start, (first - 1, w));
            }
            if end > last {
                self.spans.insert(last + 1, (end, w));
            }
        }
        self.spans.insert(first, (last, width));
    }

    /// The width of `code`, when a /W entry gives one.
    fn get(&self, code: u32) -> Option<f64> {
        self.spans
            .range(..=code)
            .next_back()
            .filter(|(_, (last, _))| code <= *last)
            .map(|(_, (_, width))| *width)
    }
}

#[derive(Debug)]
pub(crate) struct Font {
    pub base_font: String,
    pub two_byte: bool,
    /// Unicode text of each one-byte code, by the glyph name that the font's encoding gives it. None for a code that the encoding
    /// does not name, or names with no Unicode. None for the whole field when the encoding cannot be read.
    encoding: Option<Vec<Option<String>>>,
    /// Parsed ToUnicode, if the font has one. Read only when a code of this font is shown.
    to_unicode: Option<ToUnicodeMap>,
    first_char: i64,
    widths: Vec<f64>,
    missing_width: Option<f64>,
    standard: Option<Standard>,
    cid_widths: CidWidths,
    cid_default: f64,
    pub ascent: f64,
    pub descent: f64,
    /// False when a glyph advance cannot be determined. Width 0 is then used and the page reports it.
    pub width_known: bool,
    /// By code: the encoding gives the code its text from StandardEncoding, in place of the built-in encoding of the embedded Type 1
    /// program, which the encoding needed and which was not read. A code that /Differences names is not one of them. Empty when no
    /// code is.
    standard_for_builtin: Vec<bool>,
}

/// Removes an embedded subset prefix such as `ABCDEF+` from a BaseFont name.
pub(crate) fn normalize_base_font(name: &str) -> String {
    match name.split_once('+') {
        Some((prefix, rest)) if prefix.len() == 6 && prefix.bytes().all(|b| b.is_ascii_uppercase()) => rest.to_string(),
        _ => name.to_string(),
    }
}

fn name_of(dict: &Dictionary, key: &[u8]) -> Option<String> {
    dict.get(key).ok().and_then(|o| o.as_name().ok()).map(|n| String::from_utf8_lossy(n).into_owned())
}

fn num(doc: &Document, object: &Object) -> PResult<Option<f64>> {
    Ok(crate::boxes::number(resolve(doc, object)?))
}

impl Font {
    /// Loads a font dictionary. Type3 and unsupported encodings fail here, before any text is shown. The CMap and the widths are
    /// charged to the meters as they are read.
    pub(crate) fn load(doc: &Document, dict: &Dictionary, meters: Meters<'_>) -> PResult<Font> {
        let subtype = name_of(dict, b"Subtype").unwrap_or_default();
        let base_font = normalize_base_font(&name_of(dict, b"BaseFont").unwrap_or_default());
        match subtype.as_str() {
            "Type0" => Self::load_type0(doc, dict, base_font, meters),
            "Type3" => Err(PdfErr::unsupported(
                "PDF_UNSUPPORTED_FONT",
                format!("Type3 font {base_font} is not supported by the text extractor"),
            )),
            _ => Self::load_simple(doc, dict, base_font, meters),
        }
    }

    fn load_type0(doc: &Document, dict: &Dictionary, base_font: String, meters: Meters<'_>) -> PResult<Font> {
        let encoding = dict.get(b"Encoding").map_err(|_| PdfErr::Parse("Type0 font without /Encoding".into()))?;
        let encoding = resolve(doc, encoding)?;
        match encoding.as_name() {
            Ok(b"Identity-H") => {}
            Ok(other) => {
                return Err(PdfErr::unsupported(
                    "PDF_UNSUPPORTED_CMAP",
                    format!("Type0 CMap {} is not supported (only Identity-H)", String::from_utf8_lossy(other)),
                ))
            }
            Err(_) => {
                return Err(PdfErr::unsupported(
                    "PDF_UNSUPPORTED_CMAP",
                    "embedded CMap streams are not supported (only Identity-H)",
                ))
            }
        }
        let descendants = resolve(doc, dict.get(b"DescendantFonts")?)?.as_array()?;
        let descendant = resolve(doc, descendants.first().ok_or_else(|| PdfErr::Parse("empty DescendantFonts".into()))?)?
            .as_dict()?;

        let mut ascent = None;
        let mut descent = None;
        if let Ok(desc_obj) = descendant.get(b"FontDescriptor") {
            let fd = resolve(doc, desc_obj)?.as_dict()?;
            ascent = fd.get(b"Ascent").ok().and_then(|o| num(doc, o).ok().flatten());
            descent = fd.get(b"Descent").ok().and_then(|o| num(doc, o).ok().flatten());
        }
        let cid_default = match descendant.get(b"DW") {
            Ok(o) => num(doc, o)?.unwrap_or(1000.0),
            Err(_) => 1000.0,
        };
        let cid_widths = match descendant.get(b"W") {
            Ok(w_obj) => read_cid_widths(doc, resolve(doc, w_obj)?.as_array()?, cid_default, meters)?,
            Err(_) => CidWidths::default(),
        };
        let to_unicode = read_to_unicode(doc, dict, 2, Some(IDENTITY_H_CODESPACE), meters)?;
        Ok(Font {
            base_font,
            two_byte: true,
            encoding: None,
            to_unicode,
            first_char: 0,
            widths: Vec::new(),
            missing_width: None,
            standard: None,
            cid_widths,
            cid_default,
            ascent: ascent.unwrap_or(DEFAULT_ASCENT),
            descent: descent.unwrap_or(DEFAULT_DESCENT),
            width_known: true,
            standard_for_builtin: Vec::new(),
        })
    }

    fn load_simple(doc: &Document, dict: &Dictionary, base_font: String, meters: Meters<'_>) -> PResult<Font> {
        let first_char = match dict.get(b"FirstChar") {
            Ok(o) => num(doc, o)?.unwrap_or(0.0) as i64,
            Err(_) => 0,
        };
        let widths: Option<Vec<f64>> = match dict.get(b"Widths") {
            Ok(o) => {
                let array = resolve(doc, o)?.as_array()?;
                // The widths are kept for the request, so the whole array is charged before it is copied.
                meters.charge(Unit::Entries, array.len() as u64)?;
                let mut out = Vec::with_capacity(array.len());
                for item in array {
                    out.push(num(doc, item)?.ok_or_else(|| width_invalid("a /Widths entry is not a number"))?);
                }
                Some(out)
            }
            Err(_) => None,
        };
        let mut ascent = None;
        let mut descent = None;
        let mut missing_width = None;
        let mut symbolic = false;
        if let Ok(fd_obj) = dict.get(b"FontDescriptor") {
            let fd = resolve(doc, fd_obj)?.as_dict()?;
            ascent = fd.get(b"Ascent").ok().and_then(|o| num(doc, o).ok().flatten());
            descent = fd.get(b"Descent").ok().and_then(|o| num(doc, o).ok().flatten());
            missing_width = fd.get(b"MissingWidth").ok().and_then(|o| num(doc, o).ok().flatten());
            symbolic = fd.get(b"Flags").ok().and_then(|o| num(doc, o).ok().flatten()).is_some_and(|f| (f as i64) & SYMBOLIC_FLAG != 0);
        }
        let standard = builtin::lookup(&base_font);
        let (std_ascent, std_descent) = match standard {
            Some(s) => {
                let (a, d) = builtin::ascent_descent(s);
                (Some(a), Some(d))
            }
            None => (None, None),
        };
        let (encoding, standard_for_builtin) = simple_encoding(doc, dict, &base_font, symbolic, meters)?;
        let width_known = widths.is_some() || standard.is_some();
        let to_unicode = read_to_unicode(doc, dict, 1, Some(ONE_BYTE_CODESPACE), meters)?;
        Ok(Font {
            base_font,
            two_byte: false,
            encoding,
            to_unicode,
            first_char,
            widths: widths.unwrap_or_default(),
            missing_width,
            standard,
            cid_widths: CidWidths::default(),
            cid_default: 0.0,
            ascent: ascent.or(std_ascent).unwrap_or(DEFAULT_ASCENT),
            descent: descent.or(std_descent).unwrap_or(DEFAULT_DESCENT),
            width_known,
            standard_for_builtin,
        })
    }

    /// Splits a show-text operand into character codes with their byte lengths. A lone last byte of a two-byte operand
    /// is marked with TRUNCATED_CODE, so `text` cannot read it as the two-byte code of the same value.
    /// Lazily, so a long string costs nothing beyond the codes that the page reads before its limit stops it.
    pub(crate) fn codes<'a>(&'a self, bytes: &'a [u8]) -> impl Iterator<Item = (u32, usize)> + 'a {
        let width = if self.two_byte { 2 } else { 1 };
        bytes.chunks(width).map(move |chunk| {
            if chunk.len() == width {
                (chunk.iter().fold(0u32, |acc, b| (acc << 8) | u32::from(*b)), width)
            } else {
                (TRUNCATED_CODE | u32::from(chunk[0]), 1)
            }
        })
    }

    /// Advance width in 1/1000 em.
    pub(crate) fn width(&self, code: u32) -> f64 {
        if self.two_byte {
            return self.cid_widths.get(code).unwrap_or(self.cid_default);
        }
        let index = i64::from(code) - self.first_char;
        if index >= 0 && (index as usize) < self.widths.len() {
            return self.widths[index as usize];
        }
        if let Some(standard) = self.standard {
            if let Some(w) = builtin::width(standard, code) {
                return w;
            }
        }
        self.missing_width.unwrap_or(0.0)
    }

    /// Unicode text of one character code. A simple font reads a code from its ToUnicode, and a code that the ToUnicode does not map
    /// from its encoding. Errors when the ToUnicode CMap is rejected (it is never replaced by the encoding), when a sequential range
    /// refuses the code (its last byte would pass 0xFF; the encoding is not used for it), when a two-byte code has no mapping, or
    /// when the code is an incomplete two-byte code.
    pub(crate) fn text(&self, code: u32) -> PResult<String> {
        if (code & TRUNCATED_CODE) != 0 {
            return Err(PdfErr::invalid(
                "PDF_TRUNCATED_CODE",
                format!("font {} has an incomplete two-byte code {:#04x} at the end of a string", self.base_font, code & 0xFF),
            ));
        }
        if let Some(parsed) = &self.to_unicode {
            let cmap = parsed.as_ref().map_err(|reason| {
                PdfErr::invalid(
                    "PDF_INVALID_TOUNICODE",
                    format!("ToUnicode of font {} is structurally damaged: {reason}", self.base_font),
                )
            })?;
            match cmap.codes.get(&code) {
                Some(Some(text)) => return Ok(text.clone()),
                Some(None) => {
                    return Err(PdfErr::unsupported(
                        "PDF_TOUNICODE_ENTRY_IGNORED",
                        format!(
                            "font {} has a ToUnicode range whose destination would overflow its last byte at code {code:#x}; \
                             the character is not read",
                            self.base_font
                        ),
                    ))
                }
                None => {}
            }
            if self.two_byte {
                return Err(PdfErr::unsupported(
                    "PDF_UNMAPPED_CID",
                    format!("character code {code:#06x} of font {} has no ToUnicode entry", self.base_font),
                ));
            }
        }
        if self.two_byte {
            return Err(PdfErr::unsupported(
                "PDF_NO_TOUNICODE",
                format!("CID font {} has no /ToUnicode, so its text cannot be read", self.base_font),
            ));
        }
        if let Some(text) = self.encoded_text(code) {
            return Ok(text.to_string());
        }
        Err(PdfErr::unsupported(
            "PDF_UNSUPPORTED_ENCODING",
            format!(
                "font {} has no ToUnicode entry for code {code:#04x}, and its encoding does not give that code a glyph with Unicode text",
                self.base_font
            ),
        ))
    }

    /// The text of a one-byte code by the glyph name that the encoding gives it.
    fn encoded_text(&self, code: u32) -> Option<&str> {
        self.encoding.as_ref()?.get(code as usize)?.as_deref()
    }

    /// Whether `text` of this code comes from the encoding: the ToUnicode has no entry for the code, and the encoding names it.
    /// The page reports this once per font, so evidence consumers can see which path read the text.
    pub(crate) fn text_from_encoding(&self, code: u32) -> bool {
        if self.two_byte {
            return false;
        }
        let in_to_unicode = match &self.to_unicode {
            None => false,
            Some(Ok(cmap)) => cmap.codes.contains_key(&code),
            Some(Err(_)) => return false,
        };
        !in_to_unicode && self.encoded_text(code).is_some()
    }

    /// Whether `text` of this code comes from StandardEncoding in place of the program's built-in encoding that was not read
    /// (`standard_for_builtin`). The page reports this once per font (PDF_TYPE1_ENCODING_UNREADABLE).
    pub(crate) fn text_from_standard_for_builtin(&self, code: u32) -> bool {
        self.standard_for_builtin.get(code as usize).copied().unwrap_or(false) && self.text_from_encoding(code)
    }

    /// Sequential ranges of the ToUnicode that refuse codes because they pass 0xFF. Zero when the font has no accepted CMap.
    pub(crate) fn ignored_tounicode_entries(&self) -> u32 {
        match &self.to_unicode {
            Some(Ok(cmap)) => cmap.ignored,
            _ => 0,
        }
    }
}

/// FontDescriptor /Flags bit 3 (value 4): the font uses its built-in encoding.
const SYMBOLIC_FLAG: i64 = 4;

/// The standard-14 fonts that are not symbolic. Their built-in encoding is StandardEncoding.
fn is_standard_nonsymbolic(base_font: &str) -> bool {
    matches!(
        base_font,
        "Times-Roman"
            | "Times-Bold"
            | "Times-Italic"
            | "Times-BoldItalic"
            | "Helvetica"
            | "Helvetica-Bold"
            | "Helvetica-Oblique"
            | "Helvetica-BoldOblique"
            | "Courier"
            | "Courier-Bold"
            | "Courier-Oblique"
            | "Courier-BoldOblique"
    )
}

/// The encoding table that a name selects, when this reader has its glyph names. MacExpertEncoding and other names have none.
fn named_base(name: &[u8]) -> Option<&'static [&'static str; 256]> {
    match name {
        b"WinAnsiEncoding" => Some(&glyph_names::WIN_ANSI),
        b"MacRomanEncoding" => Some(&glyph_names::MAC_ROMAN),
        b"StandardEncoding" => Some(&glyph_names::STANDARD),
        _ => None,
    }
}

/// The base of an encoding: the glyph names that /Differences is applied to.
enum Base {
    /// A predefined table of glyph names (ISO 32000-1 Annex D), or the standard-14 base.
    Table(&'static [&'static str; 256]),
    /// The built-in encoding of an embedded Type 1 program, by the glyph names that are read from it.
    Program(Vec<String>),
}

/// The Type 1 font program (FontFile) that a FontDescriptor embeds. A FontFile entry that is not a stream counts as absent.
fn type1_program<'a>(doc: &'a Document, dict: &'a Dictionary) -> Option<&'a Stream> {
    let descriptor = resolve(doc, dict.get(b"FontDescriptor").ok()?).ok()?.as_dict().ok()?;
    resolve(doc, descriptor.get(b"FontFile").ok()?).ok()?.as_stream().ok()
}

/// The base of an encoding that names none (ISO 32000-1 Table 114, BaseEncoding). An embedded Type 1 program's implicit base is its
/// built-in encoding, when that can be read. In every other case the base is the one this reader has always used: StandardEncoding
/// for a non-symbolic standard-14 font, and none for any other font. So the built-in encoding of a TrueType or CFF program is not read,
/// and a Type 1 program whose encoding cannot be read keeps that base too.
/// The flag is true when StandardEncoding is that base in place of the built-in encoding of a Type 1 program that was not read, for
/// any reason: the program cannot be decoded, has no `eexec` (or a string before it that is not closed), or has an encoding of a form
/// that is not read. A font with no base has no text for those codes, so its page fails on them and the flag stays false.
fn implicit_base(
    doc: &Document,
    dict: &Dictionary,
    base_font: &str,
    symbolic: bool,
    meters: Meters<'_>,
) -> PResult<(Option<Base>, bool)> {
    let program = type1_program(doc, dict);
    if let Some(stream) = program {
        if let Some(names) = read_type1_program(doc, stream, meters)? {
            return Ok((Some(Base::Program(names)), false));
        }
    }
    let base = (!symbolic && is_standard_nonsymbolic(base_font)).then_some(Base::Table(&glyph_names::STANDARD));
    let replaced = program.is_some() && base.is_some();
    Ok((base, replaced))
}

/// The built-in encoding of an embedded Type 1 program, when it can be read. A program that cannot be decoded, or whose encoding is
/// not one that can be read, gives None. A budget refusal of the page or the request stays an error.
fn read_type1_program(doc: &Document, stream: &Stream, meters: Meters<'_>) -> PResult<Option<Vec<String>>> {
    let data = match decode_complete(stream, doc, meters) {
        Ok(data) => data,
        Err(error) if error.code().ends_with("_BUDGET") => return Err(error),
        Err(_) => return Ok(None),
    };
    type1_encoding::read_builtin_encoding(&data, meters)
}

/// Unicode text of each code, by the glyph name that the encoding gives it. An empty name, or a name with no Unicode, gives None.
fn texts_of(names: &[impl AsRef<str>]) -> Vec<Option<String>> {
    names.iter().map(|name| glyph_names::unicode_text(name.as_ref())).collect()
}

/// The text of each one-byte code of a simple font, by its encoding (ISO 32000-1 9.6.6.1). The base is the table that /Encoding
/// names, or /BaseEncoding of its dictionary, or the implicit base. A /Differences entry names the glyph of each code it lists, and
/// that name is used whatever the base is. A code that neither names has no text, so a built-in encoding that is not read is never
/// guessed. None when there is neither a base nor a /Differences entry, or when /Differences is malformed: the codes then fail as
/// they do without an encoding.
/// With it, by code, whether the text comes from StandardEncoding in place of a built-in encoding that was not read (`Font`
/// `standard_for_builtin`): every code of that base except the ones /Differences names. Empty when no code does.
fn simple_encoding(
    doc: &Document,
    dict: &Dictionary,
    base_font: &str,
    symbolic: bool,
    meters: Meters<'_>,
) -> PResult<(Option<Vec<Option<String>>>, Vec<bool>)> {
    let not_used = (None, Vec::new());
    let (base, replaced, differences) = match dict.get(b"Encoding") {
        Err(_) => {
            let (base, replaced) = implicit_base(doc, dict, base_font, symbolic, meters)?;
            (base, replaced, None)
        }
        Ok(obj) => match resolve(doc, obj)? {
            Object::Name(name) => (named_base(name).map(Base::Table), false, None),
            Object::Dictionary(enc) => {
                let (base, replaced) = match enc.get(b"BaseEncoding") {
                    Err(_) => implicit_base(doc, dict, base_font, symbolic, meters)?,
                    Ok(obj) => (resolve(doc, obj)?.as_name().ok().and_then(named_base).map(Base::Table), false),
                };
                let differences = match enc.get(b"Differences") {
                    Err(_) => None,
                    // A /Differences that is not an array is malformed, so the encoding is not used.
                    Ok(obj) => match resolve(doc, obj)?.as_array() {
                        Ok(array) => Some(array),
                        Err(_) => return Ok(not_used),
                    },
                };
                (base, replaced, differences)
            }
            _ => return Ok(not_used),
        },
    };
    if base.is_none() && differences.is_none() {
        return Ok(not_used);
    }
    let mut table: Vec<Option<String>> = match base {
        Some(Base::Table(names)) => texts_of(names),
        Some(Base::Program(names)) => texts_of(&names),
        None => vec![None; 256],
    };
    let mut standard_for_builtin = if replaced { vec![true; 256] } else { Vec::new() };
    if let Some(array) = differences {
        // The entries are charged as they are read, so an array past the page or request limit is refused.
        meters.charge(Unit::Entries, array.len() as u64)?;
        let mut code: Option<usize> = None;
        for item in array {
            match resolve(doc, item)? {
                Object::Integer(n) => code = usize::try_from(*n).ok().filter(|c| *c < 256),
                Object::Name(name) => {
                    let Some(c) = code else { continue };
                    table[c] = glyph_names::unicode_text(&String::from_utf8_lossy(name));
                    if let Some(flag) = standard_for_builtin.get_mut(c) {
                        *flag = false;
                    }
                    code = (c + 1 < 256).then_some(c + 1);
                }
                _ => return Ok(not_used),
            }
        }
    }
    Ok((Some(table), standard_for_builtin))
}

/// One codespace range. Byte `i` of a code of `width` bytes must lie between `lo[i]` and `hi[i]`. Containment and overlap are
/// judged per byte: a code is a sequence of bytes, so a value between the two bounds as integers is not necessarily inside.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CodeBox {
    width: usize,
    lo: [u8; 4],
    hi: [u8; 4],
}

/// Codespace that the predefined Identity-H CMap declares: every two-byte code from <0000> to <FFFF>. Type0 fonts are the
/// only two-byte fonts this extractor loads, and load_type0 accepts Identity-H alone (Identity-V is rejected as unsupported).
const IDENTITY_H_CODESPACE: &[CodeBox] = &[CodeBox { width: 2, lo: [0x00, 0x00, 0x00, 0x00], hi: [0xFF, 0xFF, 0x00, 0x00] }];

/// Codespace of a simple font: one-byte codes from <00> to <FF>. Simple fonts have one-byte codes by definition, so this is the
/// codespace used when a simple font's ToUnicode declares none.
const ONE_BYTE_CODESPACE: &[CodeBox] = &[CodeBox { width: 1, lo: [0x00, 0x00, 0x00, 0x00], hi: [0xFF, 0x00, 0x00, 0x00] }];

/// Read limits of one ToUnicode CMap. A CMap over a limit is rejected as a whole, so the text of its font is not read.
const MAX_TOKENS: usize = 1 << 20;
const MAX_ENTRIES: usize = 1 << 20;
const MAX_WORK: usize = 1 << 20;
const MAX_CODESPACE_RANGES: usize = 64;

/// Reason of a ToUnicode that inherits another CMap. The parent's codespace and mappings are not loaded, so the font fails
/// instead of reading its own entries as if they were the whole CMap.
const INHERITED_CMAP_REASON: &str = "ToUnicode inherits another CMap through /UseCMap or usecmap, which is not supported";

/// Parses the ToUnicode of a font whose character codes are `code_len` bytes: 1 for a simple font, 2 for Identity-H.
/// `implied` is the codespace used when the ToUnicode declares none.
fn read_to_unicode(
    doc: &Document,
    dict: &Dictionary,
    code_len: usize,
    implied: Option<&[CodeBox]>,
    meters: Meters<'_>,
) -> PResult<Option<ToUnicodeMap>> {
    let Ok(obj) = dict.get(b"ToUnicode") else {
        return Ok(None);
    };
    let stream = resolve(doc, obj)?.as_stream()?;
    if stream.dict.get(b"UseCMap").is_ok() {
        return Err(PdfErr::unsupported("PDF_UNSUPPORTED_CMAP", INHERITED_CMAP_REASON));
    }
    let data = decode_complete(stream, doc, meters)?;
    let parsed = parse_to_unicode_within(&data, code_len, implied, meters)?;
    // A usecmap operator in the body is the same inheritance, so it fails the font at load as the dictionary entry does.
    if parsed.as_ref().err() == Some(&INHERITED_CMAP_REASON) {
        return Err(PdfErr::unsupported("PDF_UNSUPPORTED_CMAP", INHERITED_CMAP_REASON));
    }
    // The mappings of an accepted CMap are kept for the request, so they are charged as entries.
    if let Ok(cmap) = &parsed {
        meters.charge(Unit::Entries, cmap.codes.len() as u64)?;
    }
    Ok(Some(parsed))
}

/// A number of a /W array as a code: whole and not negative. A number past the range of a code is kept, and cut where it is used.
fn code_value(value: f64) -> PResult<u64> {
    if value.is_finite() && value >= 0.0 {
        // Rust's float-to-integer cast saturates, so a huge value stays huge and is cut by the caller.
        Ok(value as u64)
    } else {
        Err(PdfErr::invalid("PDF_FONT_WIDTHS_INVALID", "a /W code is negative or is not a number"))
    }
}

/// Reads the /W array of a CIDFont. Entries are read in order, and a later entry takes the codes it shares with an earlier one.
/// `c [w ...]` gives one width per code from c, and `c_first c_last w` gives one width to every code in the range. A range is
/// stored as one interval, so its length costs nothing. Entries past 0xFFFF are cut to it or dropped, because no two-byte code
/// reaches them. A range whose last code is below its first is malformed, and fails the font.
fn read_cid_widths(doc: &Document, w: &[Object], cid_default: f64, meters: Meters<'_>) -> PResult<CidWidths> {
    let mut widths = CidWidths::default();
    let mut i = 0usize;
    while i < w.len() {
        meters.charge(Unit::Entries, 1)?;
        let first = num(doc, &w[i])?.ok_or_else(|| width_invalid("a /W entry that starts a range is not a number"))?;
        let first = code_value(first)?;
        match w.get(i + 1).map(|o| resolve(doc, o)).transpose()? {
            Some(Object::Array(inner)) => {
                for (k, value) in inner.iter().enumerate() {
                    meters.charge(Unit::Entries, 1)?;
                    let width = num(doc, value)?.ok_or_else(|| width_invalid("a /W width is not a number"))?;
                    // A code past the two-byte range names nothing, so its width is validated and then not kept.
                    if let Some(cid) = first.checked_add(k as u64).filter(|&cid| cid <= MAX_CID) {
                        widths.assign(cid as u32, cid as u32, width);
                    }
                }
                i += 2;
            }
            Some(other) => {
                let last = match crate::boxes::number(other) {
                    Some(last) => code_value(last)?,
                    None => return Err(PdfErr::invalid("PDF_FONT_WIDTHS_INVALID", "a /W range end is not a number")),
                };
                if last < first {
                    return Err(PdfErr::invalid("PDF_FONT_WIDTHS_INVALID", "a /W range ends before it starts"));
                }
                let width = match w.get(i + 2) {
                    Some(o) => num(doc, o)?.ok_or_else(|| width_invalid("a /W range width is not a number"))?,
                    None => cid_default,
                };
                if first <= MAX_CID {
                    widths.assign(first as u32, last.min(MAX_CID) as u32, width);
                }
                i += 3;
            }
            None => break,
        }
    }
    Ok(widths)
}

/// The font's widths cannot be used, so the font is refused. The reason is fixed text.
fn width_invalid(reason: &str) -> PdfErr {
    PdfErr::invalid("PDF_FONT_WIDTHS_INVALID", format!("the font's widths are not usable: {reason}"))
}

#[derive(Debug, PartialEq)]
enum Token {
    Hex(Vec<u8>),
    /// A hex string that is not closed, or holds a byte other than hex digits and white space.
    Bad,
    /// A literal string. Its content is data: an operator written inside it is not read as one.
    Str,
    Open,
    Close,
    Keyword(String),
}

/// Bytes that end a regular token.
fn is_delimiter(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\r' | b'\n' | 0 | 12 | b'(' | b')' | b'<' | b'>' | b'[' | b']' | b'{' | b'}' | b'/' | b'%')
}

/// Index just past the literal string that opens at `start` (a `(`). Parentheses nest, and a backslash escapes the byte after
/// it. None when the string is not closed.
fn string_end(data: &[u8], start: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut i = start;
    while i < data.len() {
        match data[i] {
            b'\\' => i += 2,
            b'(' => {
                depth += 1;
                i += 1;
            }
            b')' => {
                depth -= 1;
                i += 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => i += 1,
        }
    }
    None
}

/// Splits a CMap, or the cleartext of a Type 1 program (`type1_encoding`), into tokens. White space and comments separate
/// tokens and are skipped. A literal string is one token, so operators inside it are never read. A name keeps its slash.
/// The error reasons do not name the data: the ToUnicode error puts its own subject in front of them.
fn tokenize(data: &[u8], budget: &mut Budget<'_>) -> Result<Vec<Token>, &'static str> {
    let mut out = Vec::new();
    let mut counted = 0usize;
    let mut i = 0usize;
    while i < data.len() {
        if out.len() >= MAX_TOKENS {
            return Err("it has more tokens than the reader accepts");
        }
        let c = data[i];
        match c {
            b' ' | b'\t' | b'\r' | b'\n' | 0 | 12 => i += 1,
            b'%' => {
                while i < data.len() && data[i] != b'\n' && data[i] != b'\r' {
                    i += 1;
                }
            }
            b'(' => {
                i = string_end(data, i).ok_or("it has a literal string that is not closed")?;
                out.push(Token::Str);
            }
            b'<' if data.get(i + 1) == Some(&b'<') => {
                out.push(Token::Keyword("<<".into()));
                i += 2;
            }
            b'<' => {
                i += 1;
                let mut digits = Vec::new();
                let mut valid = true;
                while i < data.len() && data[i] != b'>' {
                    match data[i] {
                        b if b.is_ascii_hexdigit() => digits.push(b),
                        b' ' | b'\t' | b'\r' | b'\n' | 0 | 12 => {}
                        _ => valid = false,
                    }
                    i += 1;
                }
                // The loop stopped at the end of the data, so the closing `>` is missing.
                let closed = i < data.len();
                i += 1;
                if !closed || !valid {
                    out.push(Token::Bad);
                } else {
                    if digits.len() % 2 == 1 {
                        digits.push(b'0');
                    }
                    let bytes: Vec<u8> = digits.chunks(2).map(|p| (hex_val(p[0]) << 4) | hex_val(p[1])).collect();
                    out.push(Token::Hex(bytes));
                }
            }
            b'>' if data.get(i + 1) == Some(&b'>') => {
                out.push(Token::Keyword(">>".into()));
                i += 2;
            }
            b'[' => {
                out.push(Token::Open);
                i += 1;
            }
            b']' => {
                out.push(Token::Close);
                i += 1;
            }
            b')' | b'>' | b'{' | b'}' => {
                out.push(Token::Keyword((c as char).to_string()));
                i += 1;
            }
            _ => {
                // A name starts with `/`, a keyword or number with any other regular byte.
                let start = i;
                i += 1;
                while i < data.len() && !is_delimiter(data[i]) {
                    i += 1;
                }
                out.push(Token::Keyword(String::from_utf8_lossy(&data[start..i]).into_owned()));
            }
        }
        // Each token is charged as it is made, so a CMap over the page or request limit stops here and is not parsed further.
        if out.len() > counted {
            budget.meter(Unit::Tokens, (out.len() - counted) as u64)?;
            counted = out.len();
        }
    }
    Ok(out)
}

fn hex_val(b: u8) -> u8 {
    match b {
        b'0'..=b'9' => b - b'0',
        b'a'..=b'f' => b - b'a' + 10,
        b'A'..=b'F' => b - b'A' + 10,
        _ => 0,
    }
}

fn be_value(bytes: &[u8]) -> u32 {
    bytes.iter().fold(0u32, |acc, b| (acc << 8) | u32::from(*b))
}

/// Splits UTF-16BE bytes into code units. An odd byte count is not a UTF-16BE string. A byte order mark (U+FEFF) at the start is
/// not text, so it is dropped. A mark anywhere else is text, and stays.
fn utf16be_units(bytes: &[u8]) -> Result<Vec<u16>, &'static str> {
    if bytes.len() % 2 != 0 {
        return Err("destination has an odd number of bytes");
    }
    let units: Vec<u16> = bytes.chunks_exact(2).map(|pair| u16::from_be_bytes([pair[0], pair[1]])).collect();
    let start = usize::from(units.first() == Some(&0xFEFF));
    Ok(units[start..].to_vec())
}

/// Decodes UTF-16 code units. An unpaired surrogate is an error, never U+FFFD.
fn decode_utf16_units(units: impl Iterator<Item = u16>) -> Result<String, &'static str> {
    char::decode_utf16(units).map(|unit| unit.map_err(|_| "destination contains an unpaired surrogate")).collect()
}

/// Text of a bfchar or array destination: one non-empty UTF-16BE string, without its byte order mark.
fn utf16be_text(bytes: &[u8]) -> Result<String, &'static str> {
    let units = utf16be_units(bytes)?;
    if units.is_empty() {
        return Err("destination is empty");
    }
    decode_utf16_units(units.into_iter())
}

/// Whether a sequential range that spans `span` further codes runs the last byte of its destination past 0xFF. The code at that
/// point has no defined mapping (PDF 32000-1 9.10.3 grows only the last byte), so it is refused.
fn crosses_low_byte(units: &[u16], span: u32) -> bool {
    units.last().is_some_and(|last| u64::from(*last & 0x00FF) + u64::from(span) > 0xFF)
}

/// Text of the `offset`-th code of a sequential bfrange. As PDF 32000-1 9.10.3 specifies, only the last byte of the
/// destination string grows by `offset`. Nothing carries into the byte before it, and a surrogate pair stays intact for every
/// offset that fits. A value past 0xFF is an error here. The caller checks `crosses_low_byte` first and refuses such a code.
fn range_text(units: &[u16], offset: u32) -> Result<String, &'static str> {
    let Some((last, prefix)) = units.split_last() else {
        return Err("destination is empty");
    };
    // The last byte of a UTF-16BE string is the low byte of its last code unit.
    let low = u8::try_from(u32::from(*last & 0x00FF) + offset)
        .map_err(|_| "range increment overflows the last destination byte")?;
    let last = (*last & 0xFF00) | u16::from(low);
    decode_utf16_units(prefix.iter().copied().chain(std::iter::once(last)))
}

/// Work counters of one ToUnicode CMap: its mapping entries and the work their code positions cost. A code read through a
/// sequential destination costs one step per UTF-16 unit of that destination. The tokens and the steps are also charged to the page
/// and the request. A charge that they refuse stops the parse, and the refusal is kept in `refused`. So the page fails for its budget,
/// and the CMap is not taken for a damaged one.
struct Budget<'m> {
    entries: usize,
    work: usize,
    meters: Meters<'m>,
    refused: Option<PdfErr>,
}

/// The reason that stops a parse at a refused charge. `parse_to_unicode_within` returns the refusal in its place.
const REFUSED: &str = "the page or request budget stopped the parse";

impl<'m> Budget<'m> {
    fn new(meters: Meters<'m>) -> Self {
        Budget { entries: 0, work: 0, meters, refused: None }
    }

    fn entry(&mut self) -> Result<(), &'static str> {
        self.entries += 1;
        if self.entries > MAX_ENTRIES {
            return Err("it has more mapping entries than the reader accepts");
        }
        Ok(())
    }

    fn codes(&mut self, n: usize) -> Result<(), &'static str> {
        self.work = self.work.saturating_add(n);
        if self.work > MAX_WORK {
            return Err("it expands more code positions than the reader accepts");
        }
        self.meter(Unit::Work, n as u64)
    }

    /// Charges `n` units of `unit` to the page and to the request.
    fn meter(&mut self, unit: Unit, n: u64) -> Result<(), &'static str> {
        match self.meters.charge(unit, n) {
            Ok(()) => Ok(()),
            Err(refusal) => {
                self.refused = Some(refusal);
                Err(REFUSED)
            }
        }
    }
}

/// Byte `i` of a code of `width` bytes held as a big-endian value.
fn byte_at(value: u32, width: usize, i: usize) -> u8 {
    (value >> (8 * (width - 1 - i))) as u8
}

/// Whether the code `value` of `width` bytes lies inside one codespace range: each byte is between that range's bounds.
fn in_codespace(space: &[CodeBox], width: usize, value: u32) -> bool {
    space.iter().any(|b| b.width == width && (0..width).all(|i| {
        let byte = byte_at(value, width, i);
        b.lo[i] <= byte && byte <= b.hi[i]
    }))
}

/// A source code is exactly `code_len` bytes, the width of the font's character codes. Lookup is by value, so a source of
/// another width would match the wrong glyph: `<0041>` has the value of simple-font code 0x41.
fn source_code(bytes: &[u8], code_len: usize) -> Option<u32> {
    (bytes.len() == code_len && (1..=4).contains(&code_len)).then(|| be_value(bytes))
}

/// The number that a block keyword declares: the integer token just before `beginXXX`. A block without one is malformed.
fn declared_count(tokens: &[Token], keyword: usize) -> Result<usize, &'static str> {
    match keyword.checked_sub(1).and_then(|p| tokens.get(p)) {
        Some(Token::Keyword(k)) => k.parse::<usize>().map_err(|_| "a mapping block has no integer count before its keyword"),
        _ => Err("a mapping block has no integer count before its keyword"),
    }
}

/// Codespace ranges of a ToUnicode CMap, or None when it declares no begincodespacerange.
fn read_codespace(tokens: &[Token]) -> Result<Option<Vec<CodeBox>>, &'static str> {
    let mut boxes = Vec::new();
    let mut declared = false;
    let mut i = 0usize;
    while i < tokens.len() {
        if matches!(&tokens[i], Token::Keyword(k) if k == "begincodespacerange") {
            declared = true;
            let count = declared_count(tokens, i)?;
            i = codespace_block(tokens, i + 1, count, &mut boxes)?;
        } else {
            i += 1;
        }
    }
    if !declared {
        return Ok(None);
    }
    check_codespace(&boxes)?;
    Ok(Some(boxes))
}

/// Reads `<lo> <hi>` pairs up to endcodespacerange. Bounds are 1 to 4 bytes, equally long, and every byte of lo is at most the
/// byte of hi. The number of pairs must equal the declared count. Returns the index after endcodespacerange.
fn codespace_block(tokens: &[Token], mut i: usize, count: usize, boxes: &mut Vec<CodeBox>) -> Result<usize, &'static str> {
    let mut pairs = 0usize;
    loop {
        match (tokens.get(i), tokens.get(i + 1)) {
            (Some(Token::Keyword(k)), _) if k == "endcodespacerange" => break,
            (Some(Token::Hex(lo)), Some(Token::Hex(hi))) => {
                let width = lo.len();
                if !(1..=4).contains(&width) || hi.len() != width {
                    return Err("begincodespacerange bounds differ in byte length or are not 1 to 4 bytes");
                }
                let mut range = CodeBox { width, lo: [0; 4], hi: [0; 4] };
                for k in 0..width {
                    if lo[k] > hi[k] {
                        return Err("begincodespacerange range is reversed");
                    }
                    range.lo[k] = lo[k];
                    range.hi[k] = hi[k];
                }
                boxes.push(range);
                if boxes.len() > MAX_CODESPACE_RANGES {
                    return Err("begincodespacerange declares more ranges than the reader accepts");
                }
                pairs += 1;
                i += 2;
            }
            _ => return Err("begincodespacerange block is malformed or not closed by endcodespacerange"),
        }
    }
    if pairs != count {
        return Err("begincodespacerange declares a different number of ranges than it holds");
    }
    Ok(i + 1)
}

/// Rejects codespace ranges that make a code ambiguous. Two ranges share a code when every byte position they both have
/// overlaps. Ranges of one length must not share a code. A shorter range must not share its bytes with the first bytes of a
/// longer one, because then the same bytes could end a short code or start a longer one. Both checks are per byte.
fn check_codespace(boxes: &[CodeBox]) -> Result<(), &'static str> {
    for (a, x) in boxes.iter().enumerate() {
        for y in &boxes[a + 1..] {
            let shared = (0..x.width.min(y.width)).all(|i| x.lo[i].max(y.lo[i]) <= x.hi[i].min(y.hi[i]));
            if !shared {
                continue;
            }
            if x.width == y.width {
                return Err("begincodespacerange ranges of one byte length overlap");
            }
            return Err("begincodespacerange ranges of different lengths share their first bytes, so a code length is ambiguous");
        }
    }
    Ok(())
}

/// Reads `<src> <dst>` pairs up to endbfchar. Each source is `code_len` bytes inside the codespace, and each destination is a
/// valid text. The number of pairs must equal the declared count. Returns the index after endbfchar.
fn bfchar_block(
    tokens: &[Token],
    mut i: usize,
    count: usize,
    code_len: usize,
    space: &[CodeBox],
    map: &mut CodeMap,
    budget: &mut Budget,
) -> Result<usize, &'static str> {
    let mut pairs = 0usize;
    loop {
        match (tokens.get(i), tokens.get(i + 1)) {
            (Some(Token::Keyword(k)), _) if k == "endbfchar" => break,
            (Some(Token::Hex(src)), Some(Token::Hex(dst))) => {
                let code = source_code(src, code_len)
                    .ok_or("beginbfchar source code length differs from the font's character code width")?;
                if !in_codespace(space, code_len, code) {
                    return Err("beginbfchar source code is outside the codespacerange");
                }
                let text = utf16be_text(dst)?;
                budget.entry()?;
                budget.codes(1)?;
                map.insert(code, Some(text));
                pairs += 1;
                i += 2;
            }
            _ => return Err("beginbfchar block is malformed or not closed by endbfchar"),
        }
    }
    if pairs != count {
        return Err("beginbfchar declares a different number of mappings than it holds");
    }
    Ok(i + 1)
}

/// Reads `<lo> <hi> <dst>` and `<lo> <hi> [<dst> ...]` entries up to endbfrange. Every code from lo to hi must lie inside the
/// codespace, and a sequential destination must be valid text. A sequential entry whose last destination byte would pass 0xFF is
/// counted in `ignored`. Its codes that fit keep their text, and each code past 0xFF is refused (None in `map`), so the encoding
/// does not supply it. An array holds exactly one destination per code. The number of entries must equal the declared count.
/// Returns the index after endbfrange.
fn bfrange_block(
    tokens: &[Token],
    mut i: usize,
    count: usize,
    code_len: usize,
    space: &[CodeBox],
    map: &mut CodeMap,
    ignored: &mut u32,
    budget: &mut Budget,
) -> Result<usize, &'static str> {
    let mut entries = 0usize;
    loop {
        match tokens.get(i) {
            Some(Token::Keyword(k)) if k == "endbfrange" => break,
            Some(Token::Hex(lo_bytes)) => {
                let Some(Token::Hex(hi_bytes)) = tokens.get(i + 1) else {
                    return Err("beginbfrange entry has no high source code");
                };
                let width_error = "beginbfrange source code length differs from the font's character code width";
                let lo = source_code(lo_bytes, code_len).ok_or(width_error)?;
                let hi = source_code(hi_bytes, code_len).ok_or(width_error)?;
                if lo > hi {
                    return Err("beginbfrange source range is reversed");
                }
                budget.entry()?;
                let codes = (hi - lo) as usize + 1;
                // A sequential destination is read again for every code, so each code costs one step per unit it holds.
                let units = match tokens.get(i + 2) {
                    Some(Token::Hex(dst)) => Some(utf16be_units(dst)?),
                    _ => None,
                };
                let cost = units.as_ref().map_or(codes, |u| codes.saturating_mul(u.len().max(1)));
                // Charged before the per-code check below, so that check never runs beyond the limit.
                budget.codes(cost)?;
                for code in lo..=hi {
                    if !in_codespace(space, code_len, code) {
                        return Err("beginbfrange source range is outside the codespacerange");
                    }
                }
                match (units, tokens.get(i + 2)) {
                    (Some(units), _) => {
                        // The first code checks the destination's text. Its validity does not depend on the offset, so a broken
                        // destination still rejects the whole CMap before the range is judged malformed.
                        let first = range_text(&units, 0)?;
                        if crosses_low_byte(&units, hi - lo) {
                            *ignored += 1;
                        }
                        map.insert(lo, Some(first));
                        for code in lo + 1..=hi {
                            let offset = code - lo;
                            let text = if crosses_low_byte(&units, offset) { None } else { Some(range_text(&units, offset)?) };
                            map.insert(code, text);
                        }
                        i += 3;
                    }
                    (None, Some(Token::Open)) => i = array_entries(tokens, i + 3, lo, hi, map)?,
                    _ => return Err("beginbfrange entry has no destination"),
                }
                entries += 1;
            }
            _ => return Err("beginbfrange block is malformed or not closed by endbfrange"),
        }
    }
    if entries != count {
        return Err("beginbfrange declares a different number of entries than it holds");
    }
    Ok(i + 1)
}

/// Reads the destinations of an array bfrange after its `[`: entry k is the destination of code lo + k. Every entry must be a
/// valid text, and the array must hold exactly one entry per code. Returns the index after `]`.
fn array_entries(tokens: &[Token], mut j: usize, lo: u32, hi: u32, map: &mut CodeMap) -> Result<usize, &'static str> {
    let expected = (hi - lo) as usize + 1;
    let mut texts: Vec<String> = Vec::new();
    loop {
        match tokens.get(j) {
            Some(Token::Close) => break,
            Some(Token::Hex(dst)) => {
                if texts.len() == expected {
                    return Err("beginbfrange array has more entries than codes in its range");
                }
                texts.push(utf16be_text(dst)?);
            }
            Some(_) => return Err("beginbfrange array holds a value that is not a hex string"),
            None => return Err("beginbfrange array is not closed by ]"),
        }
        j += 1;
    }
    if texts.len() != expected {
        return Err("beginbfrange array has fewer entries than codes in its range");
    }
    for (k, text) in texts.into_iter().enumerate() {
        map.insert(lo + k as u32, Some(text));
    }
    Ok(j + 1)
}

/// Parses a ToUnicode CMap whose character codes are `code_len` bytes long. The codespace is the one the CMap declares with
/// begincodespacerange; a CMap that declares none uses `implied`, and is rejected when there is none. The CMap is accepted only
/// as a whole: a token limit, a block whose count or entries do not match, a source outside the codespace or of another width,
/// an invalid destination, an array of the wrong length, a literal string that is not closed, an inherited CMap (`usecmap`), or
/// a limit on the work the CMap expands to, rejects all of it. A sequential range whose last destination byte passes 0xFF is the
/// one exception: its codes past 0xFF are refused and the range is counted, while its other codes keep their text. Literal strings
/// and comments are skipped, so an operator written inside them is not read.
///
/// The tokens and the work are charged to `meters` as they are made. A charge that the page or the request refuses fails the call
/// with that refusal. A CMap that breaks its own rules is `Ok(Err(reason))`, and the font fails its text with that reason.
pub(crate) fn parse_to_unicode_within(
    data: &[u8],
    code_len: usize,
    implied: Option<&[CodeBox]>,
    meters: Meters<'_>,
) -> PResult<ToUnicodeMap> {
    let mut budget = Budget::new(meters);
    match parse_cmap(data, code_len, implied, &mut budget) {
        Ok(map) => Ok(Ok(map)),
        Err(reason) => match budget.refused.take() {
            Some(refusal) => Err(refusal),
            None => Ok(Err(reason)),
        },
    }
}

/// The rules of a ToUnicode CMap. Its charges go through `budget`, and the first refused charge stops the parse.
fn parse_cmap(data: &[u8], code_len: usize, implied: Option<&[CodeBox]>, budget: &mut Budget<'_>) -> Result<ToUnicodeCMap, &'static str> {
    let tokens = tokenize(data, budget)?;
    if tokens.iter().any(|t| matches!(t, Token::Bad)) {
        return Err("ToUnicode has a hex string that is not closed or holds a byte other than hex digits");
    }
    if tokens.iter().any(|t| matches!(t, Token::Keyword(k) if k == "usecmap" || k == "/UseCMap")) {
        return Err(INHERITED_CMAP_REASON);
    }
    let declared = read_codespace(&tokens)?;
    let space: &[CodeBox] = match &declared {
        Some(space) => space.as_slice(),
        None => implied.ok_or("ToUnicode has no begincodespacerange, and the font's encoding implies no codespace")?,
    };
    let mut map: CodeMap = HashMap::new();
    let mut ignored = 0u32;
    let mut i = 0usize;
    while i < tokens.len() {
        match &tokens[i] {
            Token::Keyword(k) if k == "begincodespacerange" => {
                // The ranges were read above. This pass steps over the block and checks it the same way again.
                let count = declared_count(&tokens, i)?;
                i = codespace_block(&tokens, i + 1, count, &mut Vec::new())?;
            }
            Token::Keyword(k) if k == "beginbfchar" => {
                let count = declared_count(&tokens, i)?;
                i = bfchar_block(&tokens, i + 1, count, code_len, space, &mut map, budget)?;
            }
            Token::Keyword(k) if k == "beginbfrange" => {
                let count = declared_count(&tokens, i)?;
                i = bfrange_block(&tokens, i + 1, count, code_len, space, &mut map, &mut ignored, budget)?;
            }
            // A hex string, or an end keyword, outside its block means that a block keyword is misspelled or missing. Such
            // a CMap is not read in part, so the whole CMap is rejected.
            Token::Hex(_) | Token::Bad => {
                return Err("ToUnicode has a hex string outside its mapping or codespace block");
            }
            Token::Keyword(k) if matches!(k.as_str(), "endcodespacerange" | "endbfchar" | "endbfrange") => {
                return Err("ToUnicode has an end keyword outside its block");
            }
            _ => i += 1,
        }
    }
    Ok(ToUnicodeCMap { codes: map, ignored })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_of(map: &CodeMap, code: u32) -> Option<&str> {
        map.get(&code).and_then(|text| text.as_deref())
    }

    /// Whether a sequential range refuses `code`.
    fn refused(map: &CodeMap, code: u32) -> bool {
        matches!(map.get(&code), Some(None))
    }

    /// Limits that no test CMap reaches, so only the CMap's own rules apply to it.
    const UNLIMITED: crate::content::Budget = crate::content::Budget {
        decoded: u64::MAX,
        tokens: u64::MAX,
        work: u64::MAX,
        glyphs: u64::MAX,
        form_runs: u64::MAX,
        entries: u64::MAX,
        evidence: u64::MAX,
    };

    /// Parses a CMap under no page or request limit. The result is the CMap's own verdict.
    fn parse_to_unicode(data: &[u8], code_len: usize, implied: Option<&[CodeBox]>) -> ToUnicodeMap {
        let page = crate::content::Meter::page(UNLIMITED);
        let request = crate::content::Meter::request(UNLIMITED);
        parse_to_unicode_within(data, code_len, implied, Meters { page: &page, request: &request })
            .expect("no page or request limit applies to this CMap")
    }

    #[test]
    fn tokens_and_work_of_a_tounicode_parse_are_charged_to_the_page_and_the_request() {
        // The CMap has ten tokens and one mapping, which is one unit of work.
        let body: &[u8] = b"1 begincodespacerange <00> <FF> endcodespacerange 1 beginbfchar <41> <0058> endbfchar";
        let parse = |page: crate::content::Budget, request: crate::content::Budget| {
            let page = crate::content::Meter::page(page);
            let request = crate::content::Meter::request(request);
            parse_to_unicode_within(body, 1, None, Meters { page: &page, request: &request })
        };
        let tokens = |tokens| crate::content::Budget { tokens, ..UNLIMITED };
        assert!(parse(tokens(10), UNLIMITED).is_ok());
        assert_eq!(parse(tokens(9), UNLIMITED).unwrap_err().code(), "PDF_CONTENT_BUDGET");
        assert_eq!(parse(UNLIMITED, tokens(9)).unwrap_err().code(), "PDF_REQUEST_BUDGET");
        assert_eq!(parse(crate::content::Budget { work: 0, ..UNLIMITED }, UNLIMITED).unwrap_err().code(), "PDF_CONTENT_BUDGET");
    }

    /// Codespace declarations that cover every one-byte code and every two-byte code.
    const SPACE_1: &[u8] = b"1 begincodespacerange <00> <FF> endcodespacerange ";
    const SPACE_2: &[u8] = b"1 begincodespacerange <0000> <FFFF> endcodespacerange ";

    /// Parses a CMap body that follows the full codespacerange of `code_len`-byte codes.
    fn parse_body(code_len: usize, body: &[u8]) -> ToUnicodeMap {
        let space = if code_len == 1 { SPACE_1 } else { SPACE_2 };
        parse_to_unicode(&[space, body].concat(), code_len, None)
    }

    /// Parses a two-byte (Identity-H) CMap body that must be well formed.
    fn parsed(body: &[u8]) -> CodeMap {
        parse_body(2, body).expect("well-formed ToUnicode CMap").codes
    }

    /// A two-byte font whose ToUnicode is the given parse result.
    fn font_with(to_unicode: ToUnicodeMap) -> Font {
        Font {
            base_font: "Test".into(),
            two_byte: true,
            encoding: None,
            to_unicode: Some(to_unicode),
            first_char: 0,
            widths: Vec::new(),
            missing_width: None,
            standard: None,
            cid_widths: CidWidths::default(),
            cid_default: 1000.0,
            ascent: DEFAULT_ASCENT,
            descent: DEFAULT_DESCENT,
            width_known: true,
            standard_for_builtin: Vec::new(),
        }
    }

    /// A simple font (one-byte codes) whose ToUnicode is the given parse result.
    fn simple_font_with(to_unicode: ToUnicodeMap) -> Font {
        Font { two_byte: false, ..font_with(to_unicode) }
    }

    #[test]
    fn subset_prefix_is_removed() {
        assert_eq!(normalize_base_font("ABCDEF+Helvetica"), "Helvetica");
        assert_eq!(normalize_base_font("Helvetica"), "Helvetica");
        assert_eq!(normalize_base_font("abcdef+Helvetica"), "abcdef+Helvetica");
    }

    #[test]
    fn to_unicode_bfchar_and_bfrange_are_parsed() {
        let cmap = b"1 beginbfrange <0010> <0012> <AC00> endbfrange 1 beginbfrange <0020> <0021> [<0041> <0042>] endbfrange 1 beginbfchar <0030> <00660069> endbfchar";
        let map = parsed(cmap);
        assert_eq!(text_of(&map, 0x10), Some("\u{AC00}"));
        assert_eq!(text_of(&map, 0x12), Some("\u{AC02}"));
        assert_eq!(text_of(&map, 0x20), Some("A"));
        assert_eq!(text_of(&map, 0x21), Some("B"));
        // A ligature destination can hold several UTF-16 units.
        assert_eq!(text_of(&map, 0x30), Some("fi"));
    }

    #[test]
    fn sequential_bfrange_advances_a_surrogate_pair() {
        // D83D DE00 is U+1F600; the offset changes only the low byte of the last code unit.
        let map = parsed(b"1 beginbfrange <0001> <0003> <D83DDE00> endbfrange");
        assert_eq!(text_of(&map, 1), Some("\u{1F600}"));
        assert_eq!(text_of(&map, 2), Some("\u{1F601}"));
        assert_eq!(text_of(&map, 3), Some("\u{1F602}"));
    }

    #[test]
    fn surrogate_pair_range_keeps_all_256_low_byte_values() {
        // D83D DEFF is U+1F6FF: the last of the 256 codes of a valid range.
        let map = parsed(b"1 beginbfrange <0000> <00FF> <D83DDE00> endbfrange");
        assert_eq!(text_of(&map, 0), Some("\u{1F600}"));
        assert_eq!(text_of(&map, 0xFF), Some("\u{1F6FF}"));
    }

    #[test]
    fn sequential_bfrange_last_byte_overflow_refuses_only_the_codes_past_0xff() {
        // 00FF + 1 would carry into the high byte. Code 1 fits its low byte and keeps U+00FF. Code 2 would need 0x100, so it is refused.
        let cmap = parse_body(2, b"1 beginbfrange <0001> <0002> <00FF> endbfrange").expect("well-formed ToUnicode CMap");
        assert_eq!(cmap.ignored, 1);
        assert_eq!(text_of(&cmap.codes, 1), Some("\u{FF}"));
        assert!(refused(&cmap.codes, 2));
        // The range whose last code still fits its low byte is a valid entry.
        let cmap = parse_body(2, b"1 beginbfrange <0001> <0001> <00FF> endbfrange").expect("well-formed ToUnicode CMap");
        assert_eq!(cmap.ignored, 0);
        assert_eq!(text_of(&cmap.codes, 1), Some("\u{FF}"));
    }

    #[test]
    fn sequential_bfrange_surrogate_low_byte_overflow_refuses_only_the_codes_past_0xff() {
        // D83D DEFF + 1 must not become U+1F700 (D83D DF00). Code 1 fits and keeps U+1F6FF. Code 2 is refused, and the next entry is kept.
        let cmap = parse_body(2, b"1 beginbfrange <0001> <0002> <D83DDEFF> endbfrange 1 beginbfchar <0005> <0042> endbfchar")
            .expect("well-formed ToUnicode CMap");
        assert_eq!(cmap.ignored, 1);
        assert_eq!(text_of(&cmap.codes, 1), Some("\u{1F6FF}"));
        assert!(refused(&cmap.codes, 2));
        assert_eq!(text_of(&cmap.codes, 5), Some("B"));
    }

    #[test]
    fn sequential_bfrange_low_byte_overflow_past_a_surrogate_refuses_only_the_codes_past_0xff() {
        // D83D DFFF is U+1F7FF. Code 1 keeps it. Code 2 would need the low byte 0x100, so it is refused.
        let cmap = parse_body(2, b"1 beginbfrange <0001> <0002> <D83DDFFF> endbfrange").expect("well-formed ToUnicode CMap");
        assert_eq!(text_of(&cmap.codes, 1), Some("\u{1F7FF}"));
        assert!(refused(&cmap.codes, 2));
        assert_eq!(cmap.ignored, 1);
    }

    #[test]
    fn sequential_bfrange_from_ffff_refuses_the_code_past_it() {
        let cmap = parse_body(2, b"1 beginbfrange <0001> <0002> <FFFF> endbfrange").expect("well-formed ToUnicode CMap");
        assert_eq!(text_of(&cmap.codes, 1), Some("\u{FFFF}"));
        assert!(refused(&cmap.codes, 2));
        assert_eq!(cmap.ignored, 1);
    }

    #[test]
    fn overflowing_entry_refuses_its_codes_past_0xff_and_the_entries_around_it_are_read() {
        let cmap = parse_body(
            2,
            b"1 beginbfrange <0001> <0002> <00FF> endbfrange 1 beginbfchar <0005> <0042> endbfchar 1 beginbfrange <0010> <0011> <0061> endbfrange",
        )
        .expect("well-formed ToUnicode CMap");
        assert_eq!(cmap.ignored, 1);
        assert_eq!(text_of(&cmap.codes, 1), Some("\u{FF}"));
        assert!(refused(&cmap.codes, 2));
        assert_eq!(text_of(&cmap.codes, 5), Some("B"));
        assert_eq!(text_of(&cmap.codes, 0x10), Some("a"));
        assert_eq!(text_of(&cmap.codes, 0x11), Some("b"));
        assert_eq!(cmap.codes.len(), 5);
    }

    #[test]
    fn a_broken_destination_rejects_the_whole_cmap_even_when_it_would_also_overflow() {
        // D8FF is a high surrogate with no low surrogate after it, so the destination is damaged whatever the offset.
        assert!(parse_body(2, b"1 beginbfrange <0001> <0002> <D8FF00FF> endbfrange").is_err());
    }

    #[test]
    fn sequential_bfrange_increments_only_the_last_byte_of_a_multi_unit_string() {
        // "fi" becomes "fj"; a surrogate pair followed by a BMP unit keeps both units.
        let map = parsed(b"1 beginbfrange <0001> <0002> <00660069> endbfrange 1 beginbfrange <0003> <0004> <D83DDE000041> endbfrange");
        assert_eq!(text_of(&map, 1), Some("fi"));
        assert_eq!(text_of(&map, 2), Some("fj"));
        assert_eq!(text_of(&map, 3), Some("\u{1F600}A"));
        assert_eq!(text_of(&map, 4), Some("\u{1F600}B"));
    }

    #[test]
    fn lone_surrogate_destinations_reject_the_whole_cmap() {
        assert!(parse_body(2, b"1 beginbfchar <0001> <D800> endbfchar").is_err());
        assert!(parse_body(2, b"1 beginbfrange <0002> <0002> <DC00> endbfrange").is_err());
        assert!(parse_body(2, b"1 beginbfrange <0003> <0003> [<D800>] endbfrange").is_err());
    }

    #[test]
    fn one_bad_destination_rejects_the_whole_cmap_not_one_code() {
        // <0041> is valid, but the second pair's destination is an unpaired surrogate: nothing from this CMap is read.
        assert!(parse_body(2, b"2 beginbfchar <0041> <0058> <0042> <D800> endbfchar").is_err());
    }

    #[test]
    fn odd_length_and_empty_destinations_reject_the_whole_cmap() {
        assert!(parse_body(2, b"1 beginbfrange <0001> <0001> <41> endbfrange").is_err());
        assert!(parse_body(2, b"1 beginbfchar <0002> <414243> endbfchar").is_err());
        assert!(parse_body(2, b"1 beginbfrange <0004> <0004> <> endbfrange").is_err());
    }

    #[test]
    fn array_bfrange_keeps_surrogate_pairs_and_one_entry_per_code() {
        let map = parsed(b"1 beginbfrange <0001> <0003> [<D83DDE00> <0041> <0042>] endbfrange");
        assert_eq!(text_of(&map, 1), Some("\u{1F600}"));
        assert_eq!(text_of(&map, 2), Some("A"));
        assert_eq!(text_of(&map, 3), Some("B"));
    }

    #[test]
    fn array_entry_that_is_not_hex_rejects_the_whole_cmap() {
        assert!(parse_body(2, b"1 beginbfrange <0001> <0003> [<D83DDE00> /X <0041>] endbfrange").is_err());
    }

    #[test]
    fn array_with_fewer_or_more_entries_than_codes_rejects_the_whole_cmap() {
        assert!(parse_body(2, b"1 beginbfrange <0001> <0003> [<0041>] endbfrange").is_err());
        assert!(parse_body(2, b"1 beginbfrange <0001> <0001> [<0041> <0042>] endbfrange").is_err());
    }

    #[test]
    fn unclosed_array_rejects_the_whole_cmap() {
        // The valid bfchar entry after the broken array is not trusted either.
        let data = b"1 beginbfrange <0001> <0002> [<0041> endbfrange 1 beginbfchar <0005> <0042> endbfchar";
        assert!(parse_body(2, data).is_err());
    }

    #[test]
    fn range_without_destination_rejects_the_whole_cmap() {
        let data = b"1 beginbfrange <0001> <0002> endbfrange 1 beginbfchar <0003> <0041> endbfchar";
        assert!(parse_body(2, data).is_err());
    }

    #[test]
    fn unclosed_bfchar_rejects_the_whole_cmap() {
        // The bfrange starts inside the unclosed bfchar. Its operands must not be read as bfchar pairs.
        let data = b"1 beginbfchar <0001> <0041> 1 beginbfrange <0010> <0010> <0061> endbfrange";
        assert!(parse_body(2, data).is_err());
    }

    #[test]
    fn bfchar_with_an_odd_operand_rejects_the_whole_cmap() {
        let data = b"1 beginbfchar <0001> <0041> <0002> endbfchar 1 beginbfrange <0010> <0010> <0061> endbfrange";
        assert!(parse_body(2, data).is_err());
    }

    #[test]
    fn unclosed_bfrange_at_end_of_data_rejects_the_whole_cmap() {
        assert!(parse_body(2, b"1 beginbfrange <0001> <0002> <0041>").is_err());
    }

    #[test]
    fn unterminated_or_non_hex_string_rejects_the_whole_cmap() {
        assert!(parse_body(2, b"1 beginbfchar <0001> <0041").is_err());
        assert!(parse_body(2, b"1 beginbfchar <00G1> <0041> endbfchar").is_err());
    }

    #[test]
    fn empty_or_long_source_code_and_reversed_range_reject_the_whole_cmap() {
        assert!(parse_body(2, b"1 beginbfchar <> <0041> endbfchar").is_err());
        assert!(parse_body(2, b"1 beginbfchar <0000000041> <0041> endbfchar").is_err());
        assert!(parse_body(2, b"1 beginbfrange <0005> <0001> <0041> endbfrange").is_err());
    }

    #[test]
    fn block_counts_must_match_the_entries_they_hold() {
        assert!(parse_body(2, b"2 beginbfchar <0001> <0041> <0002> <0042> endbfchar").is_ok());
        assert!(parse_body(2, b"0 beginbfchar <0001> <0041> endbfchar").is_err());
        assert!(parse_body(2, b"2 beginbfchar <0001> <0041> endbfchar").is_err());
        assert!(parse_body(2, b"beginbfchar <0001> <0041> endbfchar").is_err());
        assert!(parse_body(2, b"2 beginbfrange <0001> <0001> <0041> endbfrange").is_err());
        // An array must hold exactly one destination per code of its range.
        assert!(parse_body(2, b"1 beginbfrange <0001> <0001> [<0041> <0042>] endbfrange").is_err());
    }

    #[test]
    fn literal_strings_are_skipped_so_operators_inside_them_are_not_run() {
        // The string holds a complete bfchar block. It is data, so the CMap maps nothing.
        assert!(parsed(b"/Notice ( beginbfchar <41> <0058> endbfchar ) def").is_empty());
        // Nested parentheses and escapes do not end the string early.
        assert!(parsed(b"/Notice ((a) \\) beginbfchar <41> <0058> endbfchar) def").is_empty());
        // A string that is never closed is damage.
        assert!(parse_body(2, b"/Notice (beginbfchar <41> <0058> endbfchar").is_err());
    }

    #[test]
    fn usecmap_inheritance_is_rejected_rather_than_guessed() {
        assert!(parse_body(2, b"/Parent usecmap 1 beginbfchar <0001> <0041> endbfchar").is_err());
        assert!(parse_body(2, b"/UseCMap /Parent 1 beginbfchar <0001> <0041> endbfchar").is_err());
    }

    #[test]
    fn content_outside_the_mapping_blocks_is_skipped() {
        let map = parse_to_unicode(b"%!PS-Adobe-3.0 Resource-CMap\n/CIDInit /ProcSet findresource begin\n1 begincodespacerange <0000> <FFFF> endcodespacerange\n1 beginbfchar <0001> <0041> endbfchar\nendcmap", 2, None).expect("well-formed ToUnicode CMap").codes;
        assert_eq!(text_of(&map, 1), Some("A"));
    }

    #[test]
    fn two_byte_range_must_fit_its_destination_and_the_codespace() {
        // Sequential destinations change only the last byte. In this 65536-code range, 0x00 to 0xBE stay within the low byte and keep
        // their text, and each code past 0xBE is refused. None of them is left to the encoding.
        let overflowing = parse_body(2, b"1 beginbfrange <0000> <FFFF> <0041> endbfrange").expect("well-formed ToUnicode CMap");
        assert_eq!(overflowing.ignored, 1);
        assert_eq!(text_of(&overflowing.codes, 0xBE), Some("\u{FF}"));
        assert!(refused(&overflowing.codes, 0xBF));
        assert!(refused(&overflowing.codes, 0xFFFF));
        // <0000>-<00BE> maps 191 codes; the last one, 0xBE, takes the low byte 0xFF.
        let map = parsed(b"1 beginbfrange <0000> <00BE> <0041> endbfrange");
        assert_eq!(map.len(), 0xBF);
        assert_eq!(text_of(&map, 0), Some("A"));
    }

    #[test]
    fn invalid_entry_rejects_the_whole_cmap_for_every_code() {
        let font = font_with(parse_body(2, b"1 beginbfchar <0001> <D800> endbfchar 1 beginbfchar <0002> <0041> endbfchar"));
        assert_eq!(font.text(2).unwrap_err().code(), "PDF_INVALID_TOUNICODE");
        assert_eq!(font.text(1).unwrap_err().code(), "PDF_INVALID_TOUNICODE");
    }

    #[test]
    fn damaged_tounicode_fails_every_code_of_the_font() {
        let font = font_with(parse_body(2, b"1 beginbfchar <0001> <0041> <0002> endbfchar"));
        assert_eq!(font.text(1).unwrap_err().code(), "PDF_INVALID_TOUNICODE");
        assert_eq!(font.text(3).unwrap_err().code(), "PDF_INVALID_TOUNICODE");
    }

    #[test]
    fn source_width_must_match_the_font_code_width() {
        // A source of the wrong width is rejected in bfchar and bfrange, for both kinds of font.
        assert!(parse_body(2, b"1 beginbfchar <41> <0058> endbfchar").is_err());
        assert!(parse_body(1, b"1 beginbfchar <0041> <0058> endbfchar").is_err());
        assert!(parse_body(2, b"1 beginbfrange <41> <42> <0058> endbfrange").is_err());
        assert!(parse_body(1, b"1 beginbfrange <0041> <0042> <0058> endbfrange").is_err());
        assert!(parse_body(2, b"1 beginbfrange <41> <42> [<0058>] endbfrange").is_err());
        // Both bounds of a range are checked, not only the first.
        assert!(parse_body(1, b"1 beginbfrange <41> <0042> <0058> endbfrange").is_err());
        assert!(parse_body(1, b"1 beginbfrange <0041> <42> <0058> endbfrange").is_err());
        // A four-byte range is not a two-byte range.
        assert!(parse_body(2, b"1 beginbfrange <00000000> <FFFFFFFF> <0041> endbfrange").is_err());
    }

    #[test]
    fn two_byte_source_in_a_simple_font_rejects_the_whole_cmap() {
        // <42> is a valid one-byte entry. <0041> is not a one-byte code, so the whole CMap is rejected and <42> is not trusted.
        let font = simple_font_with(parse_body(1, b"2 beginbfchar <42> <0059> <0041> <0058> endbfchar"));
        assert_eq!(font.text(0x41).unwrap_err().code(), "PDF_INVALID_TOUNICODE");
        assert_eq!(font.text(0x42).unwrap_err().code(), "PDF_INVALID_TOUNICODE");
    }

    #[test]
    fn one_byte_codes_match_one_byte_sources() {
        let font = simple_font_with(parse_body(1, b"1 beginbfchar <41> <0058> endbfchar 1 beginbfrange <80> <FF> <0061> endbfrange"));
        assert_eq!(font.text(0x41).unwrap(), "X");
        assert_eq!(font.text(0x81).unwrap(), "b");
        assert_eq!(font.text(0xFF).unwrap(), "\u{00E0}");
    }

    #[test]
    fn one_byte_range_stops_at_the_low_byte_limit() {
        // 0x41 + 0xBE = 0xFF is the last code whose low byte fits. The code past it is refused, and it is not read from the encoding.
        let overflowing = parse_body(1, b"1 beginbfrange <00> <BF> <0041> endbfrange").expect("well-formed ToUnicode CMap");
        assert_eq!(overflowing.ignored, 1);
        assert_eq!(text_of(&overflowing.codes, 0xBE), Some("\u{00FF}"));
        assert!(refused(&overflowing.codes, 0xBF));
        let font = simple_font_with(parse_body(1, b"1 beginbfrange <00> <BE> <0041> endbfrange"));
        assert_eq!(font.text(0).unwrap(), "A");
        assert_eq!(font.text(0xBE).unwrap(), "\u{00FF}");
    }

    #[test]
    fn a_refused_code_is_not_read_from_an_encoding_that_names_it() {
        // The encoding names every code "Z", so reading 0xBF from it would give "Z". The refusal must fail the code instead.
        let overflowing = parse_body(1, b"1 beginbfrange <00> <BF> <0041> endbfrange").expect("well-formed ToUnicode CMap");
        let font = Font {
            encoding: Some(vec![Some("Z".to_string()); 256]),
            ..simple_font_with(Ok(ToUnicodeCMap { codes: overflowing.codes, ignored: 1 }))
        };
        assert_eq!(font.text(0xBF).unwrap_err().code(), "PDF_TOUNICODE_ENTRY_IGNORED");
        assert!(!font.text_from_encoding(0xBF));
        assert_eq!(font.text(0xBE).unwrap(), "\u{00FF}");
    }

    #[test]
    fn lone_trailing_byte_does_not_read_a_two_byte_entry() {
        let font = font_with(parse_body(2, b"1 beginbfchar <0041> <0058> endbfchar"));
        // 00 41 is the complete code 0x0041 and reads X. The 41 after it is one byte, not that code.
        let codes: Vec<(u32, usize)> = font.codes(&[0x00, 0x41, 0x41]).collect();
        assert_eq!(codes[0], (0x41, 2));
        assert_eq!(font.text(codes[0].0).unwrap(), "X");
        assert_eq!(codes[1].1, 1);
        assert_eq!(font.text(codes[1].0).unwrap_err().code(), "PDF_TRUNCATED_CODE");
    }

    #[test]
    fn simple_codespace_reads_sources_inside_it() {
        // <00> and <3F> are the ends of the codespace <00>-<3F>, so both sources are read.
        let map = parse_to_unicode(
            b"1 begincodespacerange <00> <3F> endcodespacerange 2 beginbfchar <00> <0041> <3F> <005A> endbfchar",
            1,
            None,
        )
        .expect("well-formed ToUnicode CMap").codes;
        assert_eq!(text_of(&map, 0x00), Some("A"));
        assert_eq!(text_of(&map, 0x3F), Some("Z"));
    }

    #[test]
    fn simple_source_outside_the_codespace_rejects_the_whole_cmap() {
        // The codespace is <00>-<3F>, so <41> is not a code of this font.
        let font = simple_font_with(parse_to_unicode(
            b"1 begincodespacerange <00> <3F> endcodespacerange 1 beginbfchar <41> <0058> endbfchar",
            1,
            None,
        ));
        assert_eq!(font.text(0x41).unwrap_err().code(), "PDF_INVALID_TOUNICODE");
        // <3E> is inside the codespace, but the whole CMap is rejected, so it is not trusted either.
        assert!(parse_to_unicode(
            b"1 begincodespacerange <00> <3F> endcodespacerange 2 beginbfchar <3E> <0059> <41> <0058> endbfchar",
            1,
            None,
        )
        .is_err());
    }

    #[test]
    fn bfrange_must_lie_inside_the_codespace() {
        // <3E>-<41> runs past the end of <00>-<3F>, so the whole CMap is rejected.
        assert!(parse_to_unicode(
            b"1 begincodespacerange <00> <3F> endcodespacerange 1 beginbfrange <3E> <41> <0058> endbfrange",
            1,
            None,
        )
        .is_err());
        // The same range inside the codespace is read.
        let map = parse_to_unicode(
            b"1 begincodespacerange <00> <3F> endcodespacerange 1 beginbfrange <3E> <3F> <0058> endbfrange",
            1,
            None,
        )
        .expect("well-formed ToUnicode CMap").codes;
        assert_eq!(text_of(&map, 0x3E), Some("X"));
        assert_eq!(text_of(&map, 0x3F), Some("Y"));
    }

    #[test]
    fn touching_codespace_ranges_cover_a_source_that_spans_them() {
        // <00>-<3F> and <40>-<FF> touch, so <3E>-<41> is inside the codespace although no single range holds it.
        let map = parse_to_unicode(
            b"1 begincodespacerange <00> <3F> endcodespacerange 1 begincodespacerange <40> <FF> endcodespacerange 1 beginbfrange <3E> <41> <0058> endbfrange",
            1,
            None,
        )
        .expect("well-formed ToUnicode CMap").codes;
        assert_eq!(text_of(&map, 0x3E), Some("X"));
        assert_eq!(text_of(&map, 0x41), Some("["));
    }

    #[test]
    fn gap_in_the_codespace_rejects_a_source_that_spans_it() {
        // Code 40 is in no codespace range, so <3E>-<41> is not inside the codespace.
        assert!(parse_to_unicode(
            b"2 begincodespacerange <00> <3F> <41> <FF> endcodespacerange 1 beginbfrange <3E> <41> <0058> endbfrange",
            1,
            None,
        )
        .is_err());
    }

    #[test]
    fn overlapping_codespace_ranges_of_one_length_reject_the_whole_cmap() {
        // <00>-<7F> and <40>-<FF> share the codes 40 to 7F.
        assert!(parse_to_unicode(b"2 begincodespacerange <00> <7F> <40> <FF> endcodespacerange", 1, None).is_err());
        // A repeated range overlaps itself.
        assert!(parse_to_unicode(
            b"1 begincodespacerange <00> <FF> endcodespacerange 1 begincodespacerange <00> <FF> endcodespacerange",
            1,
            None,
        )
        .is_err());
    }

    #[test]
    fn different_length_codespace_ranges_that_share_a_first_byte_are_rejected() {
        // The byte 00 is a complete one-byte code and also the start of every two-byte code <0000>-<FFFF>, so no length is defined.
        assert!(parse_to_unicode(
            b"2 begincodespacerange <00> <FF> <0000> <FFFF> endcodespacerange 1 beginbfchar <0041> <0058> endbfchar",
            2,
            None,
        )
        .is_err());
        // One-byte codes <00>-<7F> and two-byte codes <8000>-<FFFF> start with disjoint bytes, so every code has one length.
        let map = parse_to_unicode(
            b"2 begincodespacerange <00> <7F> <8000> <FFFF> endcodespacerange 1 beginbfchar <8041> <0058> endbfchar",
            2,
            None,
        )
        .expect("well-formed ToUnicode CMap").codes;
        assert_eq!(text_of(&map, 0x8041), Some("X"));
    }

    #[test]
    fn codespace_is_checked_per_byte_not_between_integer_bounds() {
        // <8200> lies between the integer bounds <8140> and <9FFC>, but its second byte 00 is outside 40..FC.
        let space: &[u8] = b"1 begincodespacerange <8140> <9FFC> endcodespacerange ";
        assert!(parse_to_unicode(&[space, b"1 beginbfchar <8200> <0041> endbfchar"].concat(), 2, None).is_err());
        // A range with a code between the bounds that is not inside per byte is rejected as a whole.
        assert!(parse_to_unicode(&[space, b"1 beginbfrange <8100> <8150> <0041> endbfrange"].concat(), 2, None).is_err());
        // A code inside both bytes' ranges is read.
        let map = parse_to_unicode(&[space, b"1 beginbfchar <8140> <0041> endbfchar"].concat(), 2, None).expect("well-formed ToUnicode CMap").codes;
        assert_eq!(text_of(&map, 0x8140), Some("A"));
    }

    #[test]
    fn malformed_codespace_rejects_the_whole_cmap() {
        // Bounds of different byte lengths.
        assert!(parse_to_unicode(b"1 begincodespacerange <00> <FFFF> endcodespacerange", 1, None).is_err());
        // A reversed range.
        assert!(parse_to_unicode(b"1 begincodespacerange <3F> <00> endcodespacerange", 1, None).is_err());
        // A bound without its partner.
        assert!(parse_to_unicode(b"1 begincodespacerange <00> <3F> <80> endcodespacerange", 1, None).is_err());
        // An empty bound, a bound that is not hex, and a block that is not closed.
        assert!(parse_to_unicode(b"1 begincodespacerange <> <3F> endcodespacerange", 1, None).is_err());
        assert!(parse_to_unicode(b"1 begincodespacerange <00> /X endcodespacerange", 1, None).is_err());
        assert!(parse_to_unicode(b"1 begincodespacerange <00> <3F>", 1, None).is_err());
        // Five bytes is wider than any PDF character code.
        assert!(parse_to_unicode(b"1 begincodespacerange <0000000000> <0000000001> endcodespacerange", 1, None).is_err());
        // A malformed codespacerange is an error even when the font's encoding implies a codespace.
        assert!(parse_to_unicode(b"1 begincodespacerange <00> <3F> <80> endcodespacerange", 2, Some(IDENTITY_H_CODESPACE)).is_err());
    }

    #[test]
    fn cmap_without_a_codespacerange_is_rejected_without_an_implied_codespace() {
        // Nothing declares the code width, so the CMap is rejected.
        assert!(parse_to_unicode(b"1 beginbfchar <0041> <0058> endbfchar", 1, None).is_err());
        assert!(parse_to_unicode(b"1 beginbfchar <0001> <0041> endbfchar", 2, None).is_err());
        // A declared codespace takes precedence over the implied one. This one declares one-byte codes only.
        assert!(parse_to_unicode(
            b"1 begincodespacerange <00> <FF> endcodespacerange 1 beginbfchar <0001> <0041> endbfchar",
            2,
            Some(IDENTITY_H_CODESPACE),
        )
        .is_err());
    }

    #[test]
    fn simple_cmap_without_a_codespacerange_uses_the_one_byte_codespace() {
        // Simple font codes are one byte, so <00>-<FF> is implied and every one-byte source is inside it.
        let map = parse_to_unicode(b"2 beginbfchar <41> <0058> <FF> <0059> endbfchar", 1, Some(ONE_BYTE_CODESPACE))
            .expect("well-formed ToUnicode CMap").codes;
        assert_eq!(text_of(&map, 0x41), Some("X"));
        assert_eq!(text_of(&map, 0xFF), Some("Y"));
        // A two-byte source is not a one-byte code, even though its value is inside the implied codespace.
        assert!(parse_to_unicode(b"1 beginbfchar <0041> <0058> endbfchar", 1, Some(ONE_BYTE_CODESPACE)).is_err());
    }

    #[test]
    fn one_byte_font_rejects_a_codespace_of_two_byte_codes() {
        // The declared codespace has two-byte codes only, so the one-byte source <41> is not inside it. The implied
        // one-byte codespace is not used, because the CMap declares a codespace.
        assert!(parse_to_unicode(
            b"1 begincodespacerange <0000> <FFFF> endcodespacerange 1 beginbfchar <41> <0058> endbfchar",
            1,
            Some(ONE_BYTE_CODESPACE),
        )
        .is_err());
    }

    #[test]
    fn identity_h_cmap_without_a_codespacerange_uses_the_implied_two_byte_codespace() {
        let map = parse_to_unicode(b"1 beginbfchar <0001> <0041> endbfchar", 2, Some(IDENTITY_H_CODESPACE))
            .expect("well-formed ToUnicode CMap").codes;
        assert_eq!(text_of(&map, 1), Some("A"));
        // The implied codespace is two bytes wide, so a one-byte source is still rejected.
        assert!(parse_to_unicode(b"1 beginbfchar <41> <0058> endbfchar", 2, Some(IDENTITY_H_CODESPACE)).is_err());
    }

    #[test]
    fn nul_and_form_feed_end_keywords_as_white_space() {
        // NUL and FF are white space in PDF, as the tokenizer treats them between tokens. They also end a keyword.
        let map = parse_to_unicode(
            b"1 begincodespacerange\x0c<00> <FF> endcodespacerange\x00 1 beginbfchar\x0c<41> <0058> endbfchar\x00",
            1,
            None,
        )
        .expect("well-formed ToUnicode CMap").codes;
        assert_eq!(text_of(&map, 0x41), Some("X"));
    }

    #[test]
    fn codespace_may_follow_the_mappings() {
        let map = parse_to_unicode(
            b"1 beginbfchar <80> <0061> endbfchar 1 begincodespacerange <00> <3F> endcodespacerange 1 begincodespacerange <40> <FF> endcodespacerange",
            1,
            None,
        )
        .expect("well-formed ToUnicode CMap").codes;
        assert_eq!(text_of(&map, 0x80), Some("a"));
    }

    #[test]
    fn two_byte_codespace_bounds_each_two_byte_source() {
        let map = parse_to_unicode(
            b"1 begincodespacerange <0000> <00FF> endcodespacerange 1 beginbfchar <00FF> <0041> endbfchar",
            2,
            None,
        )
        .expect("well-formed ToUnicode CMap").codes;
        assert_eq!(text_of(&map, 0xFF), Some("A"));
        // 0100 is past <00FF> in its first byte, so this two-byte source is outside the codespace.
        assert!(parse_to_unicode(
            b"1 begincodespacerange <0000> <00FF> endcodespacerange 1 beginbfchar <0100> <0041> endbfchar",
            2,
            None,
        )
        .is_err());
    }

    #[test]
    fn two_byte_font_rejects_a_codespace_of_one_byte_codes() {
        // The codespace declares one-byte codes only, so the two-byte source <0001> is not inside it.
        assert!(parse_to_unicode(
            b"1 begincodespacerange <00> <FF> endcodespacerange 1 beginbfchar <0001> <0041> endbfchar",
            2,
            None,
        )
        .is_err());
    }

    #[test]
    fn mixed_width_codespace_checks_each_width_against_its_own_ranges() {
        // One-byte <00>-<80> and two-byte <8140>-<9FFC> start with disjoint bytes. Each width checks its own sources.
        let space: &[u8] = b"2 begincodespacerange <00> <80> <8140> <9FFC> endcodespacerange ";
        let two_byte_body: &[u8] = b"1 beginbfchar <8140> <0041> endbfchar";
        let two_byte = parse_to_unicode(&[space, two_byte_body].concat(), 2, None).expect("well-formed ToUnicode CMap").codes;
        assert_eq!(text_of(&two_byte, 0x8140), Some("A"));
        let one_byte_body: &[u8] = b"1 beginbfchar <41> <0058> endbfchar";
        let one_byte = parse_to_unicode(&[space, one_byte_body].concat(), 1, None).expect("well-formed ToUnicode CMap").codes;
        assert_eq!(text_of(&one_byte, 0x41), Some("X"));
        // A one-byte source is not a two-byte code, even though its value is inside the one-byte range.
        assert!(parse_to_unicode(&[space, one_byte_body].concat(), 2, None).is_err());
    }

    #[test]
    fn mapping_work_beyond_the_limit_rejects_the_cmap() {
        // Each block expands 191 code positions, so 5000 blocks are inside the limit and 5600 are beyond it.
        let block: &[u8] = b"1 beginbfrange <0000> <00BE> <0041> endbfrange ";
        assert!(parse_body(2, &block.repeat(5000)).is_ok());
        assert!(parse_body(2, &block.repeat(5600)).is_err());
    }

    #[test]
    fn token_count_beyond_the_limit_rejects_the_cmap() {
        let tokens: &[u8] = b"x ";
        assert!(parse_to_unicode(&tokens.repeat(MAX_TOKENS + 1), 1, Some(ONE_BYTE_CODESPACE)).is_err());
    }

    #[test]
    fn declared_counts_and_array_lengths_must_match_the_entries() {
        // Two destinations for one code: the second has no code, so the whole CMap is rejected.
        assert!(parse_body(1, b"1 beginbfrange <41> <41> [<0058> <0059>] endbfrange").is_err());
        assert!(parse_body(1, b"1 beginbfrange <41> <41> [<0058>] endbfrange").is_ok());
        // A block that declares no entry holds one, so the whole CMap is rejected.
        assert!(parse_body(1, b"0 beginbfchar <41> <0058> endbfchar").is_err());
    }

    #[test]
    fn comments_hide_their_blocks_and_an_open_parenthesis_in_one_opens_no_string() {
        // The commented block is not a mapping. The "(" in the second comment does not hide the block after it.
        let map = parsed(b"% 1 beginbfchar <0002> <0042> endbfchar\n% note ( open\n1 beginbfchar <0001> <0041> endbfchar");
        assert_eq!(text_of(&map, 1), Some("A"));
        assert_eq!(text_of(&map, 2), None);
    }

    #[test]
    fn codespace_ranges_overlap_per_byte_not_between_integer_bounds() {
        // <8140>-<9FFC> and <8200> share no code: <8200> has second byte 00, outside 40 to FC.
        assert!(parse_to_unicode(b"2 begincodespacerange <8140> <9FFC> <8200> <8200> endcodespacerange", 2, None).is_ok());
        // <8200>-<82FF> shares the codes <8240> to <82FC> with the first range, so the codespace is ambiguous.
        assert!(parse_to_unicode(b"2 begincodespacerange <8140> <9FFC> <8200> <82FF> endcodespacerange", 2, None).is_err());
    }

    #[test]
    fn sequential_work_counts_every_destination_unit_of_every_code() {
        // 256 codes through a destination of 4096 UTF-16 units is 1,048,576 unit reads: exactly the limit.
        let within = format!("1 beginbfrange <0000> <00FF> <{}0000> endbfrange", "0041".repeat(4095));
        assert!(parse_body(2, within.as_bytes()).is_ok());
        // One more unit, 4097 in all, is 1,048,832 unit reads: beyond the limit.
        let beyond = format!("1 beginbfrange <0000> <00FF> <{}0000> endbfrange", "0041".repeat(4096));
        assert!(parse_body(2, beyond.as_bytes()).is_err());
    }

    /// Reads a /W array with the production limits, and returns the widths or the error.
    fn widths_of(w: &[Object]) -> PResult<CidWidths> {
        let doc = Document::new();
        let page = crate::content::Meter::page(crate::content::PAGE_BUDGET);
        let request = crate::content::Meter::request(crate::content::REQUEST_BUDGET);
        read_cid_widths(&doc, w, 1000.0, Meters { page: &page, request: &request })
    }

    #[test]
    fn a_w_range_is_one_interval_and_a_later_entry_overrides_the_codes_it_shares() {
        let mut widths = CidWidths::default();
        widths.assign(10, 0xFFFF, 500.0);
        widths.assign(20, 20, 700.0);
        assert_eq!(widths.get(9), None);
        assert_eq!(widths.get(10), Some(500.0));
        assert_eq!(widths.get(20), Some(700.0));
        assert_eq!(widths.get(21), Some(500.0));
        assert_eq!(widths.get(0xFFFF), Some(500.0));
        // Assigning inside a wide interval splits it, and keeps both sides.
        widths.assign(12, 13, 300.0);
        assert_eq!(widths.get(11), Some(500.0));
        assert_eq!(widths.get(12), Some(300.0));
        assert_eq!(widths.get(13), Some(300.0));
        assert_eq!(widths.get(14), Some(500.0));
    }

    #[test]
    fn a_w_range_of_4_billion_codes_is_one_entry_cut_to_the_two_byte_codes() {
        // /W [0 4294967295 500] used to expand into every code of its range, so it took minutes and gigabytes.
        let w = [Object::Integer(0), Object::Integer(4_294_967_295), Object::Integer(500)];
        let widths = widths_of(&w).expect("the range is read");
        assert_eq!(widths.get(0), Some(500.0));
        assert_eq!(widths.get(0xFFFF), Some(500.0));
        assert_eq!(widths.spans.len(), 1);
    }

    #[test]
    fn a_w_range_that_ends_before_it_starts_fails_the_font() {
        let w = [Object::Integer(5), Object::Integer(2), Object::Integer(500)];
        assert_eq!(widths_of(&w).unwrap_err().code(), "PDF_FONT_WIDTHS_INVALID");
    }

    #[test]
    fn w_codes_past_the_two_byte_range_are_cut_or_dropped_without_overflow() {
        // The array's codes start at 2^32 - 1, so the next code would pass a u32. No two-byte code reaches them.
        let w = [Object::Integer(4_294_967_295), Object::Array(vec![Object::Integer(1), Object::Integer(2)])];
        assert!(widths_of(&w).expect("read").spans.is_empty());
        // A huge real code names no code, in an array, and so does a range that starts and ends past 0xFFFF.
        let w = [Object::Real(1e30), Object::Array(vec![Object::Integer(1)])];
        assert!(widths_of(&w).expect("read").spans.is_empty());
        let w = [Object::Integer(0x20000), Object::Integer(0x30000), Object::Integer(9)];
        assert!(widths_of(&w).expect("read").spans.is_empty());
        // A range that runs past 0xFFFF is cut to it, and an array past it is dropped.
        let w = [Object::Integer(0xFFFE), Object::Array(vec![Object::Integer(7), Object::Integer(8), Object::Integer(9)])];
        let widths = widths_of(&w).expect("read");
        assert_eq!(widths.get(0xFFFE), Some(7.0));
        assert_eq!(widths.get(0xFFFF), Some(8.0));
        assert_eq!(widths.get(0x10000), None);
    }

    #[test]
    fn a_negative_or_non_numeric_w_code_fails_the_font() {
        let negative = [Object::Integer(-1), Object::Integer(2), Object::Integer(5)];
        assert_eq!(widths_of(&negative).unwrap_err().code(), "PDF_FONT_WIDTHS_INVALID");
        let named_end = [Object::Integer(1), Object::Name(b"X".to_vec()), Object::Integer(5)];
        assert_eq!(widths_of(&named_end).unwrap_err().code(), "PDF_FONT_WIDTHS_INVALID");
    }

    #[test]
    fn w_entries_are_charged_against_the_request() {
        let w = [Object::Integer(0), Object::Array(vec![Object::Integer(1); 10])];
        let doc = Document::new();
        let page = crate::content::Meter::page(crate::content::PAGE_BUDGET);
        let request = crate::content::Meter::request(crate::content::Budget { entries: 5, ..crate::content::REQUEST_BUDGET });
        let error = read_cid_widths(&doc, &w, 1000.0, Meters { page: &page, request: &request }).unwrap_err();
        assert_eq!(error.code(), "PDF_REQUEST_BUDGET");
    }

    #[test]
    fn a_leading_byte_order_mark_is_not_text_but_one_inside_the_text_is() {
        let map = parsed(b"2 beginbfchar <0001> <FEFF0041> <0002> <0041FEFF> endbfchar");
        assert_eq!(text_of(&map, 1), Some("A"));
        assert_eq!(text_of(&map, 2), Some("A\u{FEFF}"));
        // A destination that is only a mark has no text, so it is rejected like any empty destination.
        assert!(parse_body(2, b"1 beginbfchar <0001> <FEFF> endbfchar").is_err());
    }

    #[test]
    fn a_byte_order_mark_is_dropped_from_a_sequential_range_too() {
        let map = parsed(b"1 beginbfrange <0001> <0002> <FEFF00660069> endbfrange");
        assert_eq!(text_of(&map, 1), Some("fi"));
        assert_eq!(text_of(&map, 2), Some("fj"));
    }

    #[test]
    fn a_misspelled_block_keyword_rejects_the_whole_cmap_instead_of_leaving_it_empty() {
        // "beginbfchr" is not a keyword, so its pairs sit outside any block. The CMap is rejected, not read as empty.
        assert!(parse_body(1, b"1 beginbfchr <41> <0058> endbfchar").is_err());
        assert!(parse_body(2, b"1 beginbfchr <0041> <0058> endbfchar").is_err());
        // A stray block after a good one is refused too.
        assert!(parse_body(1, b"1 beginbfchar <41> <0058> endbfchar <42> <0059> endbfchar").is_err());
        // A simple font whose CMap is refused fails its text. It does not fall back to a WinAnsi guess.
        let font = simple_font_with(parse_body(1, b"1 beginbfchr <41> <0058> endbfchar"));
        assert_eq!(font.text(0x41).unwrap_err().code(), "PDF_INVALID_TOUNICODE");
    }

    #[test]
    fn a_w_entry_that_is_not_a_number_fails_the_font() {
        // A non-number inside /W is refused. It is not read as a width of 0, and it does not end the array.
        let in_array = [Object::Integer(1), Object::Array(vec![Object::Integer(500), Object::Name(b"X".to_vec())])];
        assert_eq!(widths_of(&in_array).unwrap_err().code(), "PDF_FONT_WIDTHS_INVALID");
        let range_width = [Object::Integer(1), Object::Integer(3), Object::Name(b"X".to_vec())];
        assert_eq!(widths_of(&range_width).unwrap_err().code(), "PDF_FONT_WIDTHS_INVALID");
        let first = [Object::Name(b"X".to_vec()), Object::Integer(3), Object::Integer(500)];
        assert_eq!(widths_of(&first).unwrap_err().code(), "PDF_FONT_WIDTHS_INVALID");
    }

    /// Loads a simple font whose /Widths is `widths`, under the given page and request meters.
    fn simple_font_with_widths(widths: Vec<Object>, page: &crate::content::Meter, request: &crate::content::Meter) -> PResult<Font> {
        let mut dict = Dictionary::new();
        dict.set("Subtype", Object::Name(b"Type1".to_vec()));
        dict.set("Widths", Object::Array(widths));
        Font::load(&Document::new(), &dict, Meters { page, request })
    }

    #[test]
    fn a_simple_font_with_a_non_number_in_widths_fails_the_font() {
        let page = crate::content::Meter::page(crate::content::PAGE_BUDGET);
        let request = crate::content::Meter::request(crate::content::REQUEST_BUDGET);
        let widths = vec![Object::Integer(600), Object::Name(b"X".to_vec())];
        assert_eq!(simple_font_with_widths(widths, &page, &request).unwrap_err().code(), "PDF_FONT_WIDTHS_INVALID");
    }

    #[test]
    fn a_widths_array_is_charged_to_the_request_and_refused_past_its_entries() {
        // Ten widths against a request of five entries: the array is refused, and none of it is kept for the request.
        let page = crate::content::Meter::page(crate::content::PAGE_BUDGET);
        let request = crate::content::Meter::request(crate::content::Budget { entries: 5, ..crate::content::REQUEST_BUDGET });
        let widths = vec![Object::Integer(600); 10];
        assert_eq!(simple_font_with_widths(widths, &page, &request).unwrap_err().code(), "PDF_REQUEST_BUDGET");
    }

    /// Loads a simple font whose dictionary holds `entries`, and whose ToUnicode is `to_unicode` when given.
    fn simple_font_from(entries: &[(&str, Object)], to_unicode: Option<&[u8]>) -> PResult<Font> {
        let mut doc = Document::new();
        let mut dict = Dictionary::new();
        dict.set("Subtype", Object::Name(b"Type1".to_vec()));
        for (key, value) in entries {
            dict.set(*key, value.clone());
        }
        if let Some(cmap) = to_unicode {
            let id = doc.add_object(lopdf::Stream::new(Dictionary::new(), cmap.to_vec()));
            dict.set("ToUnicode", Object::Reference(id));
        }
        let page = crate::content::Meter::page(crate::content::PAGE_BUDGET);
        let request = crate::content::Meter::request(crate::content::REQUEST_BUDGET);
        Font::load(&doc, &dict, Meters { page: &page, request: &request })
    }

    fn name(value: &str) -> Object {
        Object::Name(value.as_bytes().to_vec())
    }

    /// A FontDescriptor with the given /Flags.
    fn descriptor(flags: i64) -> Object {
        let mut fd = Dictionary::new();
        fd.set("Flags", Object::Integer(flags));
        Object::Dictionary(fd)
    }

    /// The text of a code, or the code of its error.
    fn text_or_code(font: &Font, code: u32) -> Result<String, &'static str> {
        font.text(code).map_err(|e| e.code())
    }

    #[test]
    fn win_ansi_reads_accents_dashes_and_quotes_without_a_tounicode() {
        let font = simple_font_from(&[("BaseFont", name("Verdana")), ("Encoding", name("WinAnsiEncoding"))], None).expect("loads");
        assert_eq!(text_or_code(&font, 0xE9), Ok("\u{E9}".to_string()));
        assert_eq!(text_or_code(&font, 0x96), Ok("\u{2013}".to_string()));
        assert_eq!(text_or_code(&font, 0x92), Ok("\u{2019}".to_string()));
        assert_eq!(text_or_code(&font, 0x80), Ok("\u{20AC}".to_string()));
        assert_eq!(text_or_code(&font, 0x27), Ok("'".to_string()));
        assert!(font.text_from_encoding(0x92));
    }

    #[test]
    fn a_standard_font_without_an_encoding_reads_standard_encoding() {
        let font = simple_font_from(&[("BaseFont", name("Times-Roman"))], None).expect("loads");
        // StandardEncoding gives code 0x27 the quoteright, not the quotesingle of WinAnsi.
        assert_eq!(text_or_code(&font, 0x27), Ok("\u{2019}".to_string()));
        assert_eq!(text_or_code(&font, 0x60), Ok("\u{2018}".to_string()));
        assert_eq!(text_or_code(&font, 0xD0), Ok("\u{2014}".to_string()));
        // 0x80 is undefined in StandardEncoding, so it fails as an unnamed code does.
        assert_eq!(text_or_code(&font, 0x80), Err("PDF_UNSUPPORTED_ENCODING"));
    }

    #[test]
    fn a_non_symbolic_standard_font_with_differences_takes_standard_encoding_as_its_base() {
        let mut enc = Dictionary::new();
        enc.set("Differences", Object::Array(vec![Object::Integer(65), name("Euro")]));
        let font = simple_font_from(&[("BaseFont", name("Helvetica")), ("Encoding", Object::Dictionary(enc))], None).expect("loads");
        assert_eq!(text_or_code(&font, 0x41), Ok("\u{20AC}".to_string()));
        assert_eq!(text_or_code(&font, 0x42), Ok("B".to_string()));
    }

    #[test]
    fn differences_name_each_code_with_agl_names_and_uni_and_u_names() {
        let mut enc = Dictionary::new();
        enc.set("BaseEncoding", name("WinAnsiEncoding"));
        enc.set(
            "Differences",
            Object::Array(vec![
                Object::Integer(65),
                name("Euro"),
                name("uni00E9"),
                name("u1F600"),
                Object::Integer(70),
                name("f_f"),
                name("a.sc"),
            ]),
        );
        let font = simple_font_from(&[("BaseFont", name("Verdana")), ("Encoding", Object::Dictionary(enc))], None).expect("loads");
        assert_eq!(text_or_code(&font, 65), Ok("\u{20AC}".to_string()));
        assert_eq!(text_or_code(&font, 66), Ok("\u{E9}".to_string()));
        assert_eq!(text_or_code(&font, 67), Ok("\u{1F600}".to_string()));
        // A code the Differences entry does not name keeps the base's glyph.
        assert_eq!(text_or_code(&font, 68), Ok("D".to_string()));
        assert_eq!(text_or_code(&font, 70), Ok("ff".to_string()));
        assert_eq!(text_or_code(&font, 71), Ok("a".to_string()));
    }

    #[test]
    fn a_code_that_the_tounicode_lacks_falls_back_to_the_encoding() {
        let cmap = b"1 begincodespacerange <00> <FF> endcodespacerange 1 beginbfchar <41> <0058> endbfchar";
        let font = simple_font_from(&[("BaseFont", name("Verdana")), ("Encoding", name("WinAnsiEncoding"))], Some(cmap)).expect("loads");
        assert_eq!(text_or_code(&font, 0x41), Ok("X".to_string()));
        assert_eq!(text_or_code(&font, 0x42), Ok("B".to_string()));
        assert!(!font.text_from_encoding(0x41));
        assert!(font.text_from_encoding(0x42));
    }

    #[test]
    fn a_damaged_tounicode_is_not_replaced_by_the_encoding() {
        // The misspelled block keyword leaves a hex string outside any block, so the CMap is rejected whole.
        let font = simple_font_from(
            &[("BaseFont", name("Verdana")), ("Encoding", name("WinAnsiEncoding"))],
            Some(b"1 beginbfchr <41> <0058> endbfchar"),
        )
        .expect("loads");
        assert_eq!(text_or_code(&font, 0x42), Err("PDF_INVALID_TOUNICODE"));
        assert!(!font.text_from_encoding(0x42));
    }

    #[test]
    fn an_overflowing_bfrange_code_is_refused_and_not_read_from_the_encoding() {
        // <61> maps to U+00FF. <62> would need 0x100, so it is refused, and WinAnsi's "b" must not be used for it.
        let cmap = b"1 begincodespacerange <00> <FF> endcodespacerange 1 beginbfrange <61> <62> <00FF> endbfrange 1 beginbfchar <41> <0058> endbfchar";
        let font = simple_font_from(&[("BaseFont", name("Verdana")), ("Encoding", name("WinAnsiEncoding"))], Some(cmap)).expect("loads");
        assert_eq!(font.ignored_tounicode_entries(), 1);
        assert_eq!(text_or_code(&font, 0x41), Ok("X".to_string()));
        assert_eq!(text_or_code(&font, 0x61), Ok("\u{FF}".to_string()));
        assert_eq!(text_or_code(&font, 0x62), Err("PDF_TOUNICODE_ENTRY_IGNORED"));
        assert!(!font.text_from_encoding(0x62));
    }

    #[test]
    fn a_symbolic_font_without_an_encoding_still_fails() {
        let symbolic = simple_font_from(&[("BaseFont", name("CMR10")), ("FontDescriptor", descriptor(4))], None).expect("loads");
        assert_eq!(text_or_code(&symbolic, 0x41), Err("PDF_UNSUPPORTED_ENCODING"));
        // A non-symbolic embedded font has a built-in encoding that is not read, so it has no implicit base either.
        let embedded = simple_font_from(&[("BaseFont", name("ABCDEF+NimbusRomNo9L")), ("FontDescriptor", descriptor(32))], None)
            .expect("loads");
        assert_eq!(text_or_code(&embedded, 0x41), Err("PDF_UNSUPPORTED_ENCODING"));
        for base in ["Symbol", "ZapfDingbats"] {
            let font = simple_font_from(&[("BaseFont", name(base))], None).expect("loads");
            assert_eq!(text_or_code(&font, 0x41), Err("PDF_UNSUPPORTED_ENCODING"), "{base}");
        }
    }

    #[test]
    fn a_symbolic_font_with_differences_reads_only_the_codes_that_differences_name() {
        let mut enc = Dictionary::new();
        enc.set("Differences", Object::Array(vec![Object::Integer(65), name("B")]));
        let font = simple_font_from(&[("BaseFont", name("CMR10")), ("FontDescriptor", descriptor(4)), ("Encoding", Object::Dictionary(enc))], None)
            .expect("loads");
        assert_eq!(text_or_code(&font, 0x41), Ok("B".to_string()));
        // 0x42 would come from the built-in encoding, which is not read.
        assert_eq!(text_or_code(&font, 0x42), Err("PDF_UNSUPPORTED_ENCODING"));
    }

    #[test]
    fn a_name_with_no_unicode_fails_only_its_own_code() {
        let mut enc = Dictionary::new();
        enc.set("Differences", Object::Array(vec![Object::Integer(65), name("g32"), Object::Integer(66), name("B")]));
        let font = simple_font_from(&[("BaseFont", name("Helvetica")), ("Encoding", Object::Dictionary(enc))], None).expect("loads");
        assert_eq!(text_or_code(&font, 0x41), Err("PDF_UNSUPPORTED_ENCODING"));
        assert_eq!(text_or_code(&font, 0x42), Ok("B".to_string()));
    }

    #[test]
    fn a_malformed_differences_array_disables_the_encoding_so_every_code_fails() {
        // The real entry 66.5 is not a code, so the array cannot be read. The encoding is then not used at all.
        let mut enc = Dictionary::new();
        enc.set(
            "Differences",
            Object::Array(vec![Object::Integer(65), name("B"), Object::Real(66.5), name("C")]),
        );
        let font = simple_font_from(&[("BaseFont", name("Helvetica")), ("Encoding", Object::Dictionary(enc))], None).expect("loads");
        assert_eq!(text_or_code(&font, 0x41), Err("PDF_UNSUPPORTED_ENCODING"));
        assert_eq!(text_or_code(&font, 0x43), Err("PDF_UNSUPPORTED_ENCODING"));
    }

    #[test]
    fn a_differences_entry_that_is_not_an_array_disables_the_encoding() {
        let mut enc = Dictionary::new();
        enc.set("Differences", name("B"));
        let font = simple_font_from(&[("BaseFont", name("Helvetica")), ("Encoding", Object::Dictionary(enc))], None).expect("loads");
        assert_eq!(text_or_code(&font, 0x41), Err("PDF_UNSUPPORTED_ENCODING"));
    }

    #[test]
    fn mac_roman_is_read_and_an_encoding_this_reader_lacks_is_not_guessed() {
        let font = simple_font_from(&[("BaseFont", name("Verdana")), ("Encoding", name("MacRomanEncoding"))], None).expect("loads");
        assert_eq!(text_or_code(&font, 0x8E), Ok("\u{E9}".to_string()));
        let expert = simple_font_from(&[("BaseFont", name("Verdana")), ("Encoding", name("MacExpertEncoding"))], None).expect("loads");
        assert_eq!(text_or_code(&expert, 0x41), Err("PDF_UNSUPPORTED_ENCODING"));
    }
}
