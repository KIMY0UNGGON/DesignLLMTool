//! Stream decoding under the work limits. Every stream that the reader decodes goes through `decode_complete`: its filters run
//! one after another, and the output of each filter is capped by the decoded bytes that the page and the request have left. A
//! filter stops at the cap, before it holds the excess, so a small stream cannot expand past the limits.
//!
//! Supported: FlateDecode, LZWDecode and ASCII85Decode, with the PNG predictors 10 to 15 on Flate and LZW, and predictor 1 (none).
//! Any other filter, and the TIFF predictor (2), is an explicit unsupported error. A damaged stream is an error, so its readable
//! prefix is never used as the whole stream.

use crate::boxes::resolve;
use crate::content::{Meters, Unit};
use crate::error::{PResult, PdfErr};
use flate2::read::ZlibDecoder;
use lopdf::filters::png::{decode_row, FilterType};
use lopdf::{Dictionary, Document, Object, Stream};
use std::borrow::Cow;
use std::io::Read;

/// The DecodeParms values that the stages read. Absent or null keys take the PDF defaults.
struct Params {
    predictor: i64,
    colors: i64,
    bits: i64,
    columns: i64,
    early_change: bool,
}

impl Params {
    fn read(dict: Option<&Dictionary>, doc: &Document) -> PResult<Params> {
        let int = |key: &[u8], default: i64| -> PResult<i64> {
            let Some(dict) = dict else { return Ok(default) };
            match dict.get(key) {
                Err(_) => Ok(default),
                Ok(value) => match resolve(doc, value)? {
                    Object::Null => Ok(default),
                    other => other.as_i64().map_err(|_| invalid_parameter()),
                },
            }
        };
        Ok(Params {
            predictor: int(b"Predictor", 1)?,
            colors: int(b"Colors", 1)?,
            bits: int(b"BitsPerComponent", 8)?,
            columns: int(b"Columns", 1)?,
            early_change: int(b"EarlyChange", 1)? != 0,
        })
    }
}

fn invalid_parameter() -> PdfErr {
    PdfErr::invalid("PDF_STREAM_DECODE", "a stream decode parameter is not an integer, or is out of range")
}

/// Decodes a whole stream. The decoded bytes are charged to the page and the request. An unfiltered stream is returned as it is,
/// without a copy. A damaged stream is an error.
pub(crate) fn decode_complete<'s>(stream: &'s Stream, doc: &Document, meters: Meters<'_>) -> PResult<Cow<'s, [u8]>> {
    let filters = filter_names(stream, doc)?;
    if filters.is_empty() {
        meters.charge(Unit::Decoded, stream.content.len() as u64)?;
        return Ok(Cow::Borrowed(stream.content.as_slice()));
    }
    let params = stage_params(stream, doc, filters.len())?;
    let mut data: Cow<'s, [u8]> = Cow::Borrowed(stream.content.as_slice());
    for (filter, params) in filters.into_iter().zip(params) {
        let output = run_stage(filter, &data, &params, meters)?;
        meters.charge(Unit::Decoded, output.len() as u64)?;
        data = Cow::Owned(output);
    }
    Ok(data)
}

/// One filter and the predictor that follows it. The output stops at the room that the meters have left.
fn run_stage(filter: &[u8], input: &[u8], params: &Params, meters: Meters<'_>) -> PResult<Vec<u8>> {
    let room = meters.room(Unit::Decoded);
    match filter {
        b"FlateDecode" => predict(inflate(input, room, meters)?, params),
        b"LZWDecode" => predict(lzw(input, params.early_change, room, meters)?, params),
        b"ASCII85Decode" => ascii85(input, room, meters),
        other => Err(PdfErr::unsupported(
            "PDF_STREAM_DECODE",
            format!("filter {} is not supported", String::from_utf8_lossy(other)),
        )),
    }
}

/// Undoes the predictor of a Flate or LZW stage. Predictor 1 is none. The TIFF predictor is not supported.
fn predict(data: Vec<u8>, params: &Params) -> PResult<Vec<u8>> {
    match params.predictor {
        1 => Ok(data),
        10..=15 => png_predict(&data, params),
        2 => Err(PdfErr::unsupported("PDF_STREAM_DECODE", "the TIFF predictor is not supported")),
        other => Err(PdfErr::unsupported("PDF_STREAM_DECODE", format!("predictor {other} is not supported"))),
    }
}

/// FlateDecode (zlib). The output stops one byte past the room, so an output that passes the room is told apart from one
/// that ends exactly at it. A stream that fails after some output is damaged, and one that fails with none cannot be decoded.
fn inflate(input: &[u8], room: u64, meters: Meters<'_>) -> PResult<Vec<u8>> {
    if input.is_empty() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    let read = ZlibDecoder::new(input).take(room.saturating_add(1)).read_to_end(&mut out);
    if out.len() as u64 > room {
        return Err(meters.exhausted(Unit::Decoded));
    }
    match read {
        Ok(_) => Ok(out),
        Err(error) if out.is_empty() => Err(PdfErr::invalid(
            "PDF_STREAM_DECODE",
            format!("FlateDecode stream cannot be decoded: {error}"),
        )),
        Err(error) => Err(PdfErr::invalid(
            "PDF_STREAM_DAMAGED",
            format!("FlateDecode stream damaged after {} bytes: {error}", out.len()),
        )),
    }
}

/// The bit reader of LZW: codes are read most significant bit first.
struct BitReader<'a> {
    data: &'a [u8],
    next: usize,
    acc: u32,
    count: u32,
}

impl BitReader<'_> {
    /// The next `width` bits, or None when fewer bits are left.
    fn read(&mut self, width: u32) -> Option<u32> {
        while self.count < width {
            self.acc = (self.acc << 8) | u32::from(*self.data.get(self.next)?);
            self.next += 1;
            self.count += 8;
        }
        self.count -= width;
        let value = (self.acc >> self.count) & ((1 << width) - 1);
        self.acc &= (1 << self.count) - 1;
        Some(value)
    }
}

fn lzw_error() -> PdfErr {
    PdfErr::invalid("PDF_STREAM_DECODE", "an LZW stream has a code that the table does not define")
}

/// LZWDecode (ISO 32000-1, 7.4.4): codes of 9 to 12 bits, most significant bit first. Code 256 clears the table and code 257
/// ends the data. With `early_change`, the code width grows one code early. Data that ends without code 257 is read to its
/// last whole code.
fn lzw(input: &[u8], early_change: bool, room: u64, meters: Meters<'_>) -> PResult<Vec<u8>> {
    const CLEAR: u32 = 256;
    const END: u32 = 257;
    const FIRST_FREE: usize = 258;
    const MAX_TABLE: usize = 4096;
    // Entry k is (prefix code, last byte, first byte, length). A code below 256 stands for its own single byte.
    let mut table: Vec<(u32, u8, u8, u64)> = (0..=255u8).map(|b| (0, b, b, 1)).collect();
    // Codes 256 and 257 are never expanded.
    table.extend([(0, 0, 0, 0), (0, 0, 0, 0)]);
    let mut width = 9u32;
    let mut bits = BitReader { data: input, next: 0, acc: 0, count: 0 };
    let mut out: Vec<u8> = Vec::new();
    let mut previous: Option<u32> = None;
    while let Some(code) = bits.read(width) {
        if code == CLEAR {
            table.truncate(FIRST_FREE);
            width = 9;
            previous = None;
            continue;
        }
        if code == END {
            break;
        }
        let next = table.len() as u32;
        // The string of this code: its first byte and its length. The code being defined is the previous string followed by
        // its own first byte.
        let (first, length) = if code < next {
            let entry = table[code as usize];
            (entry.2, entry.3)
        } else if code == next {
            let Some(prev) = previous else { return Err(lzw_error()) };
            let entry = table[prev as usize];
            (entry.2, entry.3 + 1)
        } else {
            return Err(lzw_error());
        };
        if out.len() as u64 + length > room {
            return Err(meters.exhausted(Unit::Decoded));
        }
        let start = out.len();
        out.resize(start + length as usize, 0);
        match previous {
            Some(prev) if code == next => {
                let body = length as usize - 1;
                write_string(&table, prev, &mut out[start..start + body]);
                out[start + body] = first;
            }
            _ => write_string(&table, code, &mut out[start..]),
        }
        if let Some(prev) = previous {
            if table.len() < MAX_TABLE {
                let entry = table[prev as usize];
                table.push((prev, first, entry.2, entry.3 + 1));
            }
        }
        previous = Some(code);
        if width < 12 && table.len() as u32 + u32::from(early_change) >= 1 << width {
            width += 1;
        }
    }
    Ok(out)
}

/// Writes the string of table entry `code` into `dest`, which is exactly as long as that string. The chain of prefixes is
/// walked from the end.
fn write_string(table: &[(u32, u8, u8, u64)], mut code: u32, dest: &mut [u8]) {
    let mut end = dest.len();
    while code >= 258 {
        let entry = table[code as usize];
        end -= 1;
        dest[end] = entry.1;
        code = entry.0;
    }
    debug_assert_eq!(end, 1);
    dest[0] = code as u8;
}

/// ASCII85Decode. The output is counted as it is made, because `z` turns one character into four bytes: a run of them can
/// expand past the room from a short input, so the input length alone does not decide it.
fn ascii85(input: &[u8], room: u64, meters: Meters<'_>) -> PResult<Vec<u8>> {
    let exhausted = || meters.exhausted(Unit::Decoded);
    let mut out: Vec<u8> = Vec::new();
    let mut group = [0u8; 5];
    let mut count = 0usize;
    for &byte in input {
        match byte {
            b'~' => break,
            b'\0' | b'\t' | b'\n' | 0x0C | b'\r' | b' ' => {}
            b'z' if count == 0 => out.extend_from_slice(&[0; 4]),
            b'z' => {
                return Err(PdfErr::invalid("PDF_STREAM_DECODE", "an ASCII85 stream has z inside a group"));
            }
            b'!'..=b'u' => {
                group[count] = byte - b'!';
                count += 1;
                if count == 5 {
                    out.extend_from_slice(&ascii85_group(&group)?);
                    count = 0;
                }
            }
            _ => return Err(PdfErr::invalid("PDF_STREAM_DECODE", "an ASCII85 stream has a character outside its alphabet")),
        }
        if out.len() as u64 > room {
            return Err(exhausted());
        }
    }
    match count {
        0 => {}
        1 => return Err(PdfErr::invalid("PDF_STREAM_DECODE", "an ASCII85 stream ends with a group of one character")),
        _ => {
            // A final partial group is padded with `u`, and then only count - 1 of its four bytes are kept.
            for slot in group.iter_mut().skip(count) {
                *slot = 84;
            }
            let bytes = ascii85_group(&group)?;
            out.extend_from_slice(&bytes[..count - 1]);
        }
    }
    if out.len() as u64 > room {
        return Err(exhausted());
    }
    Ok(out)
}

fn ascii85_group(digits: &[u8; 5]) -> PResult<[u8; 4]> {
    let value = digits.iter().fold(0u64, |acc, &d| acc * 85 + u64::from(d));
    u32::try_from(value)
        .map(u32::to_be_bytes)
        .map_err(|_| PdfErr::invalid("PDF_STREAM_DECODE", "an ASCII85 group is out of range"))
}

/// The PNG predictor (ISO 32000-1, 7.4.4.4): each row starts with a filter type byte. A row is `columns * colors * bits`
/// bits, rounded up to whole bytes, and the bytes per pixel for the filters are at least 1.
fn png_predict(data: &[u8], params: &Params) -> PResult<Vec<u8>> {
    let bad = |reason: &str| PdfErr::unsupported("PDF_STREAM_DECODE", format!("PNG predictor: {reason}"));
    if !matches!(params.bits, 1 | 2 | 4 | 8 | 16) || !(1..=32).contains(&params.colors) || params.columns < 1 {
        return Err(bad("colors, bits or columns are out of range"));
    }
    if data.is_empty() {
        return Ok(Vec::new());
    }
    let bits_per_pixel = (params.colors * params.bits) as u64;
    let row = (params.columns as u64)
        .checked_mul(bits_per_pixel)
        .map(|bits| bits.div_ceil(8))
        .and_then(|bytes| usize::try_from(bytes).ok())
        .ok_or_else(|| bad("a row is too long"))?;
    // A row needs its tag byte and its bytes. Checked before any buffer of the row is allocated.
    if row.checked_add(1).is_none_or(|needed| needed > data.len()) {
        return Err(bad("a row is longer than the data"));
    }
    let bytes_per_pixel = bits_per_pixel.div_ceil(8).max(1) as usize;
    // Each row is decoded in place at the end of `out`. `previous` keeps the last decoded row, which the next row needs.
    let mut out = Vec::with_capacity(data.len());
    let mut previous = vec![0u8; row];
    for chunk in data.chunks(row + 1) {
        if chunk.len() != row + 1 {
            return Err(bad("the last row is incomplete"));
        }
        let filter = FilterType::try_from(chunk[0]).map_err(|()| bad("a row has an unknown filter type"))?;
        let start = out.len();
        out.extend_from_slice(&chunk[1..]);
        decode_row(filter, bytes_per_pixel, &previous, &mut out[start..]);
        previous.copy_from_slice(&out[start..]);
    }
    Ok(out)
}

/// The filter names of a stream, after references are followed. No /Filter, or a null one, means no filter.
fn filter_names<'d>(stream: &'d Stream, doc: &'d Document) -> PResult<Vec<&'d [u8]>> {
    let value = match stream.dict.get(b"Filter") {
        Ok(value) => resolve(doc, value)?,
        Err(_) => return Ok(Vec::new()),
    };
    let not_names = || PdfErr::invalid("PDF_STREAM_DECODE", "stream /Filter is neither a name nor an array of names");
    match value {
        Object::Null => Ok(Vec::new()),
        Object::Name(name) => Ok(vec![name.as_slice()]),
        Object::Array(items) => {
            let mut names = Vec::with_capacity(items.len());
            for item in items {
                match resolve(doc, item)? {
                    Object::Name(name) => names.push(name.as_slice()),
                    _ => return Err(not_names()),
                }
            }
            Ok(names)
        }
        _ => Err(not_names()),
    }
}

/// The parameters of each filter. DecodeParms is a dictionary for a single filter, or an array with one entry per filter, where
/// a null entry means the defaults. Any other shape does not match the filters, and is an error.
fn stage_params(stream: &Stream, doc: &Document, count: usize) -> PResult<Vec<Params>> {
    let mismatch = || PdfErr::invalid("PDF_STREAM_DECODE", "DecodeParms does not match the filters of the stream");
    let dicts: Vec<Option<&Dictionary>> = match stream.dict.get(b"DecodeParms") {
        Err(_) => vec![None; count],
        Ok(value) => match resolve(doc, value)? {
            Object::Null => vec![None; count],
            Object::Dictionary(dict) if count == 1 => vec![Some(dict)],
            Object::Array(items) if items.len() == count => {
                let mut dicts = Vec::with_capacity(count);
                for item in items {
                    dicts.push(match resolve(doc, item)? {
                        Object::Null => None,
                        Object::Dictionary(dict) => Some(dict),
                        _ => return Err(mismatch()),
                    });
                }
                dicts
            }
            _ => return Err(mismatch()),
        },
    };
    dicts.into_iter().map(|dict| Params::read(dict, doc)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::content::{Budget, Meter, PAGE_BUDGET, REQUEST_BUDGET};
    use flate2::write::ZlibEncoder;
    use flate2::Compression;
    use std::io::Write;

    /// The page budget with its decoded-bytes limit changed.
    fn with_decoded(decoded: u64) -> Budget {
        Budget { decoded, ..PAGE_BUDGET }
    }

    /// Decodes under the given page budget and the production request budget.
    fn decode_within(stream: &Stream, page: Budget) -> PResult<Vec<u8>> {
        let doc = Document::new();
        let page = Meter::page(page);
        let request = Meter::request(REQUEST_BUDGET);
        decode_complete(stream, &doc, Meters { page: &page, request: &request }).map(Cow::into_owned)
    }

    fn decode(stream: &Stream) -> PResult<Vec<u8>> {
        decode_within(stream, PAGE_BUDGET)
    }

    fn zlib(data: &[u8]) -> Vec<u8> {
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(data).expect("compress");
        encoder.finish().expect("finish")
    }

    /// A stream with one filter, and DecodeParms when `params` is given.
    fn filtered(filter: &str, params: Option<Dictionary>, payload: Vec<u8>) -> Stream {
        let mut dict = Dictionary::new();
        dict.set("Filter", Object::Name(filter.as_bytes().to_vec()));
        if let Some(params) = params {
            dict.set("DecodeParms", Object::Dictionary(params));
        }
        Stream::new(dict, payload)
    }

    /// A DecodeParms dictionary whose entries are integers.
    fn params(pairs: &[(&str, i64)]) -> Dictionary {
        let mut dict = Dictionary::new();
        for (key, value) in pairs {
            dict.set(*key, Object::Integer(*value));
        }
        dict
    }

    /// The bytes that lopdf decodes for a stream. An oracle only for the cases that lopdf decodes correctly.
    fn lopdf_decode(stream: &Stream) -> Vec<u8> {
        stream.decompressed_content().expect("lopdf decodes the oracle stream")
    }

    #[test]
    fn unfiltered_stream_is_returned_as_is() {
        let stream = Stream::new(Dictionary::new(), b"BT ET".to_vec());
        assert_eq!(decode(&stream).unwrap(), b"BT ET".to_vec());
    }

    #[test]
    fn corrupt_flate_with_no_output_is_an_error() {
        let stream = filtered("FlateDecode", None, b"definitely not zlib data".to_vec());
        assert_eq!(decode(&stream).unwrap_err().code(), "PDF_STREAM_DECODE");
    }

    #[test]
    fn valid_flate_round_trips() {
        let stream = filtered("FlateDecode", None, zlib(b"BT /F1 10 Tf ET"));
        assert_eq!(decode(&stream).unwrap(), b"BT /F1 10 Tf ET");
    }

    #[test]
    fn a_flate_stream_that_ends_damaged_is_an_error() {
        let text: String = (0..300).map(|n| format!("BT /F1 10 Tf 1 0 0 1 20 700 Tm (Line {n}) Tj ET\n")).collect();
        let payload = zlib(text.as_bytes());
        // Dropping the last bytes leaves a deflate stream with no final block: the output so far is a prefix.
        let stream = filtered("FlateDecode", None, payload[..payload.len() - 12].to_vec());
        let code = decode(&stream).unwrap_err().code();
        assert!(code == "PDF_STREAM_DAMAGED" || code == "PDF_STREAM_DECODE", "unexpected code {code}");
    }

    #[test]
    fn a_flate_stream_is_capped_by_the_page_budget() {
        // Two mebibytes of zeros compress to a few kilobytes, so a small input can expand far past any limit.
        let stream = filtered("FlateDecode", None, zlib(&vec![0u8; 2 << 20]));
        assert_eq!(decode_within(&stream, with_decoded(1000)).unwrap_err().code(), "PDF_DECODE_BUDGET");
        assert_eq!(decode_within(&stream, with_decoded(3 << 20)).unwrap().len(), 2 << 20);
    }

    #[test]
    fn a_flate_stream_with_a_predictor_one_is_capped_too() {
        // H1: a DecodeParms dictionary used to send the stream to an uncapped decoder.
        let stream = filtered("FlateDecode", Some(params(&[("Predictor", 1)])), zlib(&vec![b' '; 2 << 20]));
        assert_eq!(decode_within(&stream, with_decoded(1000)).unwrap_err().code(), "PDF_DECODE_BUDGET");
    }

    #[test]
    fn the_request_budget_is_charged_as_well_as_the_page_budget() {
        let stream = filtered("FlateDecode", None, zlib(&vec![0u8; 4096]));
        let doc = Document::new();
        let page = Meter::page(PAGE_BUDGET);
        let request = Meter::request(with_decoded(1000));
        let error = decode_complete(&stream, &doc, Meters { page: &page, request: &request }).unwrap_err();
        assert_eq!(error.code(), "PDF_REQUEST_BUDGET");
    }

    #[test]
    fn a_filter_that_is_not_supported_is_an_explicit_error() {
        let stream = filtered("DCTDecode", None, vec![0xFF, 0xD8, 0xFF]);
        assert_eq!(decode(&stream).unwrap_err().code(), "PDF_STREAM_DECODE");
    }

    #[test]
    fn the_tiff_predictor_is_refused_rather_than_passed_through() {
        let stream = filtered("FlateDecode", Some(params(&[("Predictor", 2), ("Columns", 2)])), zlib(b"abcd"));
        assert_eq!(decode(&stream).unwrap_err().code(), "PDF_STREAM_DECODE");
    }

    #[test]
    fn png_predictor_matches_lopdf_for_eight_bit_rows() {
        // Five rows of four one-byte columns, one row for each PNG filter type.
        let rows: Vec<u8> = vec![
            0, 10, 20, 30, 40, //
            1, 5, 5, 5, 5, //
            2, 1, 2, 3, 4, //
            3, 9, 8, 7, 6, //
            4, 0, 255, 128, 64,
        ];
        let stream = filtered("FlateDecode", Some(params(&[("Predictor", 12), ("Columns", 4)])), zlib(&rows));
        assert_eq!(decode(&stream).unwrap(), lopdf_decode(&stream));
    }

    #[test]
    fn a_png_row_with_an_unknown_filter_type_is_an_error() {
        let stream = filtered("FlateDecode", Some(params(&[("Predictor", 12), ("Columns", 2)])), zlib(&[9, 1, 2]));
        assert_eq!(decode(&stream).unwrap_err().code(), "PDF_STREAM_DECODE");
    }

    /// LZW encoder for the tests, following ISO 32000-1, 7.4.4. The decoder adds one table entry per code after the first,
    /// so the width after code k is set from the encoder's table size minus one.
    fn lzw_encode(data: &[u8], early_change: bool) -> Vec<u8> {
        use std::collections::HashMap;
        fn put(out: &mut Vec<u8>, acc: &mut u64, count: &mut u32, code: u32, width: u32) {
            *acc = (*acc << width) | u64::from(code);
            *count += width;
            while *count >= 8 {
                *count -= 8;
                out.push((*acc >> *count) as u8);
            }
            *acc &= (1 << *count) - 1;
        }
        let early = u32::from(early_change);
        let (mut out, mut acc, mut count) = (Vec::new(), 0u64, 0u32);
        let mut width = 9u32;
        let mut table: HashMap<(u32, u8), u32> = HashMap::new();
        let mut next = 258u32;
        put(&mut out, &mut acc, &mut count, 256, width);
        let mut current: Option<u32> = None;
        for &byte in data {
            match current {
                None => current = Some(u32::from(byte)),
                Some(prefix) => {
                    if let Some(&code) = table.get(&(prefix, byte)) {
                        current = Some(code);
                    } else {
                        put(&mut out, &mut acc, &mut count, prefix, width);
                        if next < 4096 {
                            table.insert((prefix, byte), next);
                            next += 1;
                        }
                        if width < 12 && next - 1 + early >= 1 << width {
                            width += 1;
                        }
                        current = Some(u32::from(byte));
                    }
                }
            }
        }
        if let Some(prefix) = current {
            put(&mut out, &mut acc, &mut count, prefix, width);
        }
        put(&mut out, &mut acc, &mut count, 257, width);
        if count > 0 {
            out.push((acc << (8 - count)) as u8);
        }
        out
    }

    /// Deterministic text of `len` bytes made of repeated words, so that the LZW table grows past 512 and 1024 entries.
    fn sample_text(len: usize) -> Vec<u8> {
        let words = ["BT ", "/F1 ", "10 Tf ", "72 720 Td ", "(Hello) Tj ", "ET ", "0.5 w ", "1 0 0 RG ", "re f ", "q Q "];
        let mut state: u32 = 12345;
        let mut out = Vec::new();
        while out.len() < len {
            state = state.wrapping_mul(1_103_515_245).wrapping_add(12345);
            out.extend_from_slice(words[((state >> 16) % words.len() as u32) as usize].as_bytes());
            out.push(b'0' + ((state >> 8) % 10) as u8);
        }
        out.truncate(len);
        out
    }

    #[test]
    fn the_pdf_specification_lzw_example_decodes() {
        // ISO 32000-1, 7.4.4.2: "-----A---B" is encoded as these nine bytes.
        let stream = filtered("LZWDecode", None, vec![0x80, 0x0B, 0x60, 0x50, 0x22, 0x0C, 0x0C, 0x85, 0x01]);
        assert_eq!(decode(&stream).unwrap(), b"-----A---B");
    }

    #[test]
    fn lzw_round_trips_across_the_width_changes_and_matches_lopdf() {
        let text = sample_text(6000);
        for early in [true, false] {
            let encoded = lzw_encode(&text, early);
            let decode_parms = if early { None } else { Some(params(&[("EarlyChange", 0)])) };
            let stream = filtered("LZWDecode", decode_parms, encoded);
            assert_eq!(decode(&stream).unwrap(), text, "early change {early}");
            assert_eq!(lopdf_decode(&stream), text, "lopdf, early change {early}");
        }
    }

    #[test]
    fn lzw_output_is_capped_before_it_is_built() {
        let text = vec![b'A'; 5000];
        let stream = filtered("LZWDecode", None, lzw_encode(&text, true));
        assert_eq!(decode_within(&stream, with_decoded(1000)).unwrap_err().code(), "PDF_DECODE_BUDGET");
        assert_eq!(decode_within(&stream, with_decoded(5000)).unwrap(), text);
    }

    #[test]
    fn lzw_code_outside_the_table_is_an_error() {
        // A clear, then code 300 where the table defines only the 256 single bytes and the codes 256 and 257.
        let mut out = Vec::new();
        let (mut acc, mut count) = (0u64, 0u32);
        for code in [256u32, 300, 257] {
            acc = (acc << 9) | u64::from(code);
            count += 9;
            while count >= 8 {
                count -= 8;
                out.push((acc >> count) as u8);
            }
            acc &= (1 << count) - 1;
        }
        if count > 0 {
            out.push((acc << (8 - count)) as u8);
        }
        assert_eq!(decode(&filtered("LZWDecode", None, out)).unwrap_err().code(), "PDF_STREAM_DECODE");
    }

    #[test]
    fn ascii85_decodes_full_partial_and_zero_groups() {
        let decode_text = |text: &str| decode(&filtered("ASCII85Decode", None, text.as_bytes().to_vec()));
        // "9jqo^" is "Man ". "9jq" is the first two bytes of that group, "Ma", because a partial group of n characters gives n - 1.
        assert_eq!(decode_text("9jqo^~>").unwrap(), b"Man ");
        assert_eq!(decode_text("9jqo^9jqo^~>").unwrap(), b"Man Man ");
        assert_eq!(decode_text("9jq~>").unwrap(), b"Ma");
        assert_eq!(decode_text("z~>").unwrap(), vec![0, 0, 0, 0]);
        assert_eq!(decode_text("9jqo^ 9jqo^\n~>").unwrap(), b"Man Man ");
        // lopdf agrees on the full group.
        let mut dict = Dictionary::new();
        dict.set("Filter", Object::Name(b"ASCII85Decode".to_vec()));
        assert_eq!(lopdf_decode(&Stream::new(dict, b"9jqo^~>".to_vec())), b"Man ");
    }

    #[test]
    fn ascii85_rejects_a_lone_character_and_a_z_inside_a_group() {
        assert_eq!(decode_text_error("9~>"), "PDF_STREAM_DECODE");
        assert_eq!(decode_text_error("9jzqo~>"), "PDF_STREAM_DECODE");
        assert_eq!(decode_text_error("s~>"), "PDF_STREAM_DECODE");
    }

    #[test]
    fn ascii85_z_groups_expand_past_the_room_and_are_refused_as_they_are_made() {
        // Ten `z` characters are 40 bytes from a 12-byte stream, so the input length alone does not stop them at a room of 16.
        let stream = filtered("ASCII85Decode", None, b"zzzzzzzzzz~>".to_vec());
        assert_eq!(decode_within(&stream, with_decoded(16)).unwrap_err().code(), "PDF_DECODE_BUDGET");
        assert_eq!(decode_within(&stream, with_decoded(40)).unwrap().len(), 40);
    }

    fn decode_text_error(text: &str) -> &'static str {
        decode(&filtered("ASCII85Decode", None, text.as_bytes().to_vec())).unwrap_err().code()
    }

    #[test]
    fn filter_arrays_pair_each_filter_with_its_parameters() {
        // Filters in order: LZW, then Flate. Decoding runs LZW first, then Flate, so the data is encoded in the reverse order.
        let plain = b"BT /F1 10 Tf ET\n".repeat(20);
        let outer = lzw_encode(&zlib(&plain), true);
        let mut dict = Dictionary::new();
        dict.set(
            "Filter",
            Object::Array(vec![Object::Name(b"LZWDecode".to_vec()), Object::Name(b"FlateDecode".to_vec())]),
        );
        dict.set("DecodeParms", Object::Array(vec![Object::Null, Object::Null]));
        assert_eq!(decode(&Stream::new(dict, outer)).unwrap(), plain);
    }

    #[test]
    fn a_decode_parms_array_of_the_wrong_length_is_an_error() {
        let mut dict = Dictionary::new();
        dict.set(
            "Filter",
            Object::Array(vec![Object::Name(b"LZWDecode".to_vec()), Object::Name(b"FlateDecode".to_vec())]),
        );
        dict.set("DecodeParms", Object::Array(vec![Object::Null]));
        assert_eq!(decode(&Stream::new(dict, b"x".to_vec())).unwrap_err().code(), "PDF_STREAM_DECODE");
    }
}
