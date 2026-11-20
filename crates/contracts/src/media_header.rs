//! Image and video header checks shared by the engines (task 49e). Only headers are read; pixel and
//! frame data are never decoded, so no codec dependency is needed.
//!
//! PNG follows one rule (decision D2): the 8-byte signature, then an IHDR chunk whose length field is 13,
//! then a positive width and height. [`png_size_with_iend`] adds the closing IEND chunk for the engine
//! that requires a complete file (DOCX). The aspect helpers only compute; each engine keeps its own
//! diagnostics and its own handling of a zero result (decision D7).

/// The 8-byte PNG file signature.
pub const PNG_SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

/// Figure render representations an engine can link a picture to (docs/FIGURE_LINKS.md).
pub const FIGURE_REPRESENTATIONS: &[&str] = &["png"];

/// Length field and type of the IHDR chunk, which must follow the signature.
const IHDR_HEADER: [u8; 8] = [0, 0, 0, 13, b'I', b'H', b'D', b'R'];

/// A complete IEND chunk: zero length, type, and its CRC.
const IEND_CHUNK: [u8; 12] = [0, 0, 0, 0, b'I', b'E', b'N', b'D', 0xAE, 0x42, 0x60, 0x82];

/// Pixel width and height from the IHDR chunk. `None` unless the bytes start with the PNG signature and
/// an IHDR chunk of length 13, and both sizes are positive.
pub fn png_size(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < 24 || bytes[..8] != PNG_SIGNATURE || bytes[8..16] != IHDR_HEADER {
        return None;
    }
    let width = u32::from_be_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
    let height = u32::from_be_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]);
    (width > 0 && height > 0).then_some((width, height))
}

/// [`png_size`] of a complete file: the signature, the whole IHDR chunk (25 bytes), and a closing IEND chunk.
pub fn png_size_with_iend(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < 8 + 25 + 12 || !bytes.ends_with(&IEND_CHUNK) {
        return None;
    }
    png_size(bytes)
}

/// Why [`aspect_height`] has no result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AspectError {
    /// The width is not positive, or a pixel size is zero.
    NonPositive,
    /// The height does not fit in `i64`.
    OutOfRange,
}

/// `numerator / denominator` rounded half up, as `floor((2n + d) / 2d)`. `None` when the denominator is not
/// positive, the numerator is negative, or the arithmetic overflows.
pub fn div_round_half_up(numerator: i128, denominator: i128) -> Option<i128> {
    if denominator <= 0 || numerator < 0 {
        return None;
    }
    numerator.checked_mul(2)?.checked_add(denominator)?.checked_div(denominator.checked_mul(2)?)
}

/// `width * pixel_height / pixel_width` (EMU), rounded half up with exact integer arithmetic. A result of 0
/// is returned as is; the caller decides whether it is refused or raised.
pub fn aspect_height(width: i64, pixel_width: u32, pixel_height: u32) -> Result<i64, AspectError> {
    if width <= 0 || pixel_width == 0 || pixel_height == 0 {
        return Err(AspectError::NonPositive);
    }
    let height = div_round_half_up(i128::from(width) * i128::from(pixel_height), i128::from(pixel_width)).ok_or(AspectError::OutOfRange)?;
    i64::try_from(height).map_err(|_| AspectError::OutOfRange)
}

const EBML_MAGIC: &[u8; 4] = b"\x1A\x45\xDF\xA3";

/// One EBML variable-length integer at `offset`: its value and its length in bytes. `keep_marker` keeps the
/// length marker bit (element IDs) instead of masking it (sizes).
fn ebml_vint(bytes: &[u8], offset: usize, keep_marker: bool) -> Option<(u64, usize)> {
    let first = *bytes.get(offset)?;
    if first == 0 { return None; }
    let length = first.leading_zeros() as usize + 1;
    if length > 8 || offset.checked_add(length)? > bytes.len() { return None; }
    let data_mask = if length == 8 { 0 } else { 0xFFu8 >> length };
    let mut value = if keep_marker { u64::from(first) } else { u64::from(first & data_mask) };
    for byte in bytes.get(offset + 1..offset + length)? {
        value = value.checked_shl(8)?.checked_add(u64::from(*byte))?;
    }
    Some((value, length))
}

/// True when the bytes start with a complete EBML header (at most 1 MiB) whose DocType is `webm`.
pub fn is_webm_container(bytes: &[u8]) -> bool {
    if !bytes.starts_with(EBML_MAGIC) { return false; }
    let Some((header_size, size_len)) = ebml_vint(bytes, 4, false) else { return false };
    let Some(mut offset) = 4usize.checked_add(size_len) else { return false };
    let Some(header_end) = offset.checked_add(usize::try_from(header_size).ok().unwrap_or(usize::MAX)).filter(|header_end| *header_end <= bytes.len() && *header_end <= 1 << 20) else {
        return false;
    };
    while offset < header_end {
        let Some((id, id_len)) = ebml_vint(bytes, offset, true) else { return false };
        let Some(size_offset) = offset.checked_add(id_len) else { return false };
        let Some((element_size, element_size_len)) = ebml_vint(bytes, size_offset, false) else { return false };
        let Some(data_start) = size_offset.checked_add(element_size_len) else { return false };
        let Some(data_end) = data_start.checked_add(usize::try_from(element_size).ok().unwrap_or(usize::MAX)).filter(|data_end| *data_end <= header_end) else {
            return false;
        };
        if id == 0x4282 { return &bytes[data_start..data_end] == b"webm"; }
        offset = data_end;
    }
    false
}

struct EbmlElement { id: u64, data: std::ops::Range<usize>, next: usize }

fn ebml_element_at(bytes: &[u8], offset: usize, parent_end: usize) -> Option<EbmlElement> {
    let (id, id_len) = ebml_vint(bytes, offset, true)?;
    let size_offset = offset.checked_add(id_len)?;
    let (size, size_len) = ebml_vint(bytes, size_offset, false)?;
    let data_start = size_offset.checked_add(size_len)?;
    let size_bits = size_len.checked_mul(7)?;
    let unknown_size = size_bits < 64 && size == ((1u64 << size_bits) - 1);
    let data_end = if unknown_size {
        // An element header that crosses its parent's end would give a reversed range.
        if data_start > parent_end { return None; }
        parent_end
    } else {
        data_start.checked_add(usize::try_from(size).ok()?).filter(|end| *end <= parent_end)?
    };
    Some(EbmlElement { id, data: data_start..data_end, next: data_end })
}

/// True when [`is_webm_container`] holds and the Segment's Tracks hold a TrackEntry whose TrackType is 1
/// (video). Every element on the way must lie inside its parent, so a truncated file is refused.
pub fn webm_has_video_track(bytes: &[u8]) -> bool {
    const SEGMENT: u64 = 0x18538067;
    const TRACKS: u64 = 0x1654AE6B;
    const TRACK_ENTRY: u64 = 0xAE;
    const TRACK_TYPE: u64 = 0x83;
    if !is_webm_container(bytes) { return false; }
    let Some((header_size, size_len)) = ebml_vint(bytes, 4, false) else { return false };
    let Some(mut offset) = 4usize.checked_add(size_len)
        .and_then(|start| start.checked_add(usize::try_from(header_size).ok().unwrap_or(usize::MAX)))
    else { return false };
    let mut segment = None;
    while offset < bytes.len() {
        let Some(element) = ebml_element_at(bytes, offset, bytes.len()) else { return false };
        if element.id == SEGMENT { segment = Some(element.data); break; }
        if element.next <= offset { return false; }
        offset = element.next;
    }
    let Some(segment) = segment else { return false };
    let mut offset = segment.start;
    while offset < segment.end {
        let Some(element) = ebml_element_at(bytes, offset, segment.end) else { return false };
        if element.id == TRACKS {
            let mut track_offset = element.data.start;
            while track_offset < element.data.end {
                let Some(track) = ebml_element_at(bytes, track_offset, element.data.end) else { return false };
                if track.id == TRACK_ENTRY {
                    let mut field_offset = track.data.start;
                    while field_offset < track.data.end {
                        let Some(field) = ebml_element_at(bytes, field_offset, track.data.end) else { return false };
                        if field.id == TRACK_TYPE {
                            let value = bytes.get(field.data.clone())
                                .filter(|payload| (1..=8).contains(&payload.len()))
                                .and_then(|payload| payload.iter().try_fold(0u64, |value, byte| {
                                    value.checked_mul(256)?.checked_add(u64::from(*byte))
                                }));
                            if value == Some(1) {
                                return true;
                            }
                        }
                        if field.next <= field_offset { return false; }
                        field_offset = field.next;
                    }
                }
                if track.next <= track_offset { return false; }
                track_offset = track.next;
            }
        }
        if element.next <= offset { return false; }
        offset = element.next;
    }
    false
}

/// True when the bytes start with a complete `ftyp` box (16 bytes or more, inside the file, compatible
/// brands in whole 4-byte entries) whose major brand is not one of the audio-only, QuickTime, or HEIF/AVIF
/// brands that share ISO-BMFF.
pub fn is_mp4_container(bytes: &[u8]) -> bool {
    if bytes.len() < 16 || !bytes.get(4..8).is_some_and(|box_type| box_type == b"ftyp") { return false; }
    let box_size = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
    if box_size < 16 || box_size > bytes.len() || (box_size - 16) % 4 != 0 { return false; }
    let major_brand = std::str::from_utf8(&bytes[8..12]).ok();
    !major_brand.is_some_and(|brand| matches!(brand, "M4A " | "M4B " | "M4P " | "F4A " | "F4B " | "qt  " | "heic" | "heix" | "hevc" | "hevx" | "mif1" | "msf1" | "avif" | "avis"))
}

/// The ISO-BMFF box at `offset`: its type, its payload range, and the offset after it. Size 0 runs to
/// `limit`, size 1 reads the 64-bit size; the box must end inside `limit`.
fn mp4_box_at(bytes: &[u8], offset: usize, limit: usize) -> Option<(&[u8], std::ops::Range<usize>, usize)> {
    let header_end = offset.checked_add(8).filter(|end| *end <= limit)?;
    let size32 = u32::from_be_bytes(bytes.get(offset..offset + 4)?.try_into().ok()?);
    let kind = bytes.get(offset + 4..header_end)?;
    let (size, header_size) = match size32 {
        0 => (limit.checked_sub(offset)?, 8usize),
        1 => {
            let extended_end = offset.checked_add(16).filter(|end| *end <= limit)?;
            let extended = u64::from_be_bytes(bytes.get(offset + 8..extended_end)?.try_into().ok()?);
            (usize::try_from(extended).ok()?, 16usize)
        }
        size => (usize::try_from(size).ok()?, 8usize),
    };
    let next = offset.checked_add(size).filter(|end| *end <= limit && size >= header_size)?;
    Some((kind, offset.checked_add(header_size)?..next, next))
}

fn mp4_child_box(bytes: &[u8], range: std::ops::Range<usize>, wanted: &[u8; 4]) -> Option<std::ops::Range<usize>> {
    let mut offset = range.start;
    while offset < range.end {
        let (kind, payload, next) = mp4_box_at(bytes, offset, range.end)?;
        if kind == wanted { return Some(payload); }
        if next <= offset { return None; }
        offset = next;
    }
    None
}

/// True when [`is_mp4_container`] holds and a top-level `moov` box holds a `trak` whose `mdia/hdlr`
/// handler type is `vide`. Every top-level box and every `trak` sibling must lie inside its parent.
pub fn mp4_has_video_track(bytes: &[u8]) -> bool {
    if !is_mp4_container(bytes) { return false; }
    let mut top = 0usize;
    while top < bytes.len() {
        let Some((kind, payload, next)) = mp4_box_at(bytes, top, bytes.len()) else { return false };
        if kind == b"moov" {
            let mut track_offset = payload.start;
            while track_offset < payload.end {
                let Some((track_kind, track_payload, next_track)) = mp4_box_at(bytes, track_offset, payload.end) else { return false };
                if track_kind == b"trak" {
                    if let Some(media) = mp4_child_box(bytes, track_payload, b"mdia") {
                        if let Some(handler) = mp4_child_box(bytes, media, b"hdlr") {
                            if handler.end.saturating_sub(handler.start) >= 12
                                && bytes.get(handler.start + 8..handler.start + 12) == Some(&b"vide"[..])
                            {
                                return true;
                            }
                        }
                    }
                }
                if next_track <= track_offset { return false; }
                track_offset = next_track;
            }
        }
        if next <= top { return false; }
        top = next;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Signature, IHDR chunk (25 bytes) and IEND chunk; the IHDR CRC is not read.
    fn png_file(width: u32, ihdr_length: u32) -> Vec<u8> {
        let mut out = PNG_SIGNATURE.to_vec();
        out.extend_from_slice(&ihdr_length.to_be_bytes());
        out.extend_from_slice(b"IHDR");
        out.extend_from_slice(&width.to_be_bytes());
        out.extend_from_slice(&2u32.to_be_bytes());
        out.extend_from_slice(&[8, 6, 0, 0, 0, 0, 0, 0, 0]);
        out.extend_from_slice(&IEND_CHUNK);
        out
    }

    /// The task 49e input set, also used by the engine tests.
    fn png_input_set() -> [(&'static str, Vec<u8>); 5] {
        let normal = png_file(4, 13);
        let mut no_signature = normal.clone();
        no_signature[..8].fill(0);
        let no_iend = normal[..normal.len() - 12].to_vec();
        [
            ("P1 normal", normal),
            ("P2 no signature", no_signature),
            ("P3 IHDR length 14", png_file(4, 14)),
            ("P4 zero width", png_file(0, 13)),
            ("P5 no IEND", no_iend),
        ]
    }

    #[test]
    fn png_input_set_follows_the_strict_rule_and_the_iend_variant() {
        let strict: Vec<_> = png_input_set().iter().map(|(name, bytes)| (*name, png_size(bytes).is_some())).collect();
        assert_eq!(
            strict,
            [("P1 normal", true), ("P2 no signature", false), ("P3 IHDR length 14", false), ("P4 zero width", false), ("P5 no IEND", true)]
        );
        let complete: Vec<_> = png_input_set().iter().map(|(name, bytes)| (*name, png_size_with_iend(bytes).is_some())).collect();
        assert_eq!(
            complete,
            [("P1 normal", true), ("P2 no signature", false), ("P3 IHDR length 14", false), ("P4 zero width", false), ("P5 no IEND", false)]
        );
        assert_eq!(png_size(&png_file(4, 13)), Some((4, 2)));
        assert_eq!(png_size(&png_file(4, 13)[..24]), Some((4, 2)), "a header cut before IDAT still has its size");
        assert_eq!(png_size(&png_file(4, 13)[..23]), None);
        let mut short_with_iend = png_file(4, 13)[..24].to_vec();
        short_with_iend.extend_from_slice(&IEND_CHUNK);
        assert_eq!(png_size_with_iend(&short_with_iend), None, "IEND must follow a whole IHDR chunk");
    }

    #[test]
    fn aspect_height_rounds_half_up_and_leaves_zero_to_the_caller() {
        assert_eq!(aspect_height(1000, 3, 2), Ok(667));
        assert_eq!(aspect_height(5, 2, 1), Ok(3), "ties go up");
        assert_eq!(aspect_height(1000, 6, 2), Ok(333));
        assert_eq!(aspect_height(3_810_000, 300, 150), Ok(1_905_000));
        assert_eq!(aspect_height(127, 1000, 1), Ok(0), "a zero result is returned, not refused");
        assert_eq!(aspect_height(0, 1, 1), Err(AspectError::NonPositive));
        assert_eq!(aspect_height(10, 0, 1), Err(AspectError::NonPositive));
        assert_eq!(aspect_height(10, 1, 0), Err(AspectError::NonPositive));
        assert_eq!(aspect_height(i64::MAX, 1, 2), Err(AspectError::OutOfRange));
        assert_eq!(div_round_half_up(7, 0), None, "a zero denominator has no result instead of a panic");
        assert_eq!(div_round_half_up(-1, 2), None);
        assert_eq!(div_round_half_up(i128::MAX, 1), None);
        assert_eq!(div_round_half_up(1, 2), Some(1));
        assert_eq!(div_round_half_up(0, 5), Some(0));
    }

    fn ebml(id: &[u8], data: &[u8]) -> Vec<u8> {
        assert!(data.len() < 127, "the tiny fixture uses a one-byte EBML size");
        let mut bytes = id.to_vec();
        bytes.push(0x80 | data.len() as u8);
        bytes.extend_from_slice(data);
        bytes
    }

    fn webm_header(doc_type: &[u8]) -> Vec<u8> {
        ebml(EBML_MAGIC, &ebml(&[0x42, 0x82], doc_type))
    }

    fn webm_with_track_type(track_type: &[u8]) -> Vec<u8> {
        let track_entry = ebml(&[0xAE], &ebml(&[0x83], track_type));
        let segment = ebml(&[0x18, 0x53, 0x80, 0x67], &ebml(&[0x16, 0x54, 0xAE, 0x6B], &track_entry));
        [webm_header(b"webm"), segment].concat()
    }

    #[test]
    fn webm_input_set_is_judged_by_doc_type_and_video_track() {
        let normal = webm_with_track_type(&[1]);
        let truncated = normal[..normal.len() - 1].to_vec();
        let header_cut = webm_header(b"webm")[..6].to_vec();
        let inputs = [("W1 normal", normal), ("W2 truncated", truncated), ("W3 header only", webm_header(b"webm")), ("W4 header cut", header_cut)];
        let container: Vec<_> = inputs.iter().map(|(name, bytes)| (*name, is_webm_container(bytes))).collect();
        assert_eq!(container, [("W1 normal", true), ("W2 truncated", true), ("W3 header only", true), ("W4 header cut", false)]);
        let video: Vec<_> = inputs.iter().map(|(name, bytes)| (*name, webm_has_video_track(bytes))).collect();
        assert_eq!(video, [("W1 normal", true), ("W2 truncated", false), ("W3 header only", false), ("W4 header cut", false)]);
    }

    #[test]
    fn webm_doc_type_track_type_and_sizes_are_checked() {
        assert!(!is_webm_container(&webm_header(b"matroska")), "EBML Matroska DocType is not WebM");
        assert!(!is_webm_container(EBML_MAGIC), "an EBML magic prefix alone is insufficient");
        let mut oversized = webm_header(b"webm");
        oversized[4] = 0xFF;
        assert!(!is_webm_container(&oversized), "truncated or oversized EBML headers are refused");
        assert!(!webm_has_video_track(&webm_with_track_type(&[2])), "an audio-only track is not a video source");
        assert!(webm_has_video_track(&webm_with_track_type(&[0, 0, 0, 0, 0, 0, 0, 1])), "an eight-byte TrackType of one is valid");
        assert!(!webm_has_video_track(&webm_with_track_type(&[0xFF; 9])), "TrackType values wider than u64 are malformed");
        assert!(!webm_has_video_track(&webm_with_track_type(&[])), "an empty TrackType integer is malformed");

        let tracks = ebml(&[0x16, 0x54, 0xAE, 0x6B], &ebml(&[0xAE], &ebml(&[0x83], &[1])));
        let mut unknown_size_segment = b"\x18\x53\x80\x67\x01\xFF\xFF\xFF\xFF\xFF\xFF\xFF".to_vec();
        unknown_size_segment.extend_from_slice(&tracks);
        assert!(webm_has_video_track(&[webm_header(b"webm"), unknown_size_segment].concat()), "eight-byte unknown segment sizes are valid EBML");
        let truncated_segment_size = [webm_header(b"webm"), b"\x18\x53\x80\x67\x01".to_vec()].concat();
        assert!(!webm_has_video_track(&truncated_segment_size), "truncated eight-byte sizes are refused without a panic");
    }

    #[test]
    fn an_unknown_size_element_whose_header_crosses_its_parent_is_refused() {
        // TrackEntry ID inside a one-byte parent, its unknown size (0xFF) just outside it.
        assert!(ebml_element_at(&[0xAE, 0xFF], 0, 1).is_none());
        let inside = ebml_element_at(&[0xAE, 0xFF], 0, 2).expect("an unknown size ending at its parent is valid");
        assert_eq!((inside.id, inside.data, inside.next), (0xAE, 2..2, 2));
    }

    #[test]
    fn ebml_vint_handles_eight_byte_values_and_truncated_or_zero_prefixes() {
        let mut unknown_size = [0xFF; 8];
        unknown_size[0] = 0x01;
        assert_eq!(ebml_vint(&unknown_size, 0, false), Some((0x00FF_FFFF_FFFF_FFFF, 8)));
        assert_eq!(ebml_vint(&unknown_size, 0, true), Some((0x01FF_FFFF_FFFF_FFFF, 8)));
        assert_eq!(ebml_vint(&[0x01], 0, false), None, "a truncated eight-byte VINT is refused");
        assert_eq!(ebml_vint(&[0x00], 0, false), None, "a zero prefix has no EBML marker");
    }

    fn mp4_box(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        [&u32::try_from(8 + payload.len()).unwrap().to_be_bytes()[..], kind, payload].concat()
    }

    /// A 16-byte ftyp box, then a moov box with one track whose handler type is `handler`.
    fn mp4_with_handler(brand: &[u8; 4], handler: &[u8; 4]) -> Vec<u8> {
        let file_type = mp4_box(b"ftyp", &[&brand[..], &[0; 4]].concat());
        let handler = mp4_box(b"hdlr", &[&[0u8; 8][..], handler, &[0; 13]].concat());
        [file_type, mp4_box(b"moov", &mp4_box(b"trak", &mp4_box(b"mdia", &handler)))].concat()
    }

    #[test]
    fn mp4_input_set_is_judged_by_brand_and_video_track() {
        let normal = mp4_with_handler(b"isom", b"vide");
        let mut odd_brand_list = normal.clone();
        odd_brand_list[3] = 18;
        let inputs = [
            ("M1 normal", normal.clone()),
            ("M2 audio track", mp4_with_handler(b"isom", b"soun")),
            ("M3 audio brand", mp4_with_handler(b"M4A ", b"vide")),
            ("M4 QuickTime", mp4_with_handler(b"qt  ", b"vide")),
            ("M5 ftyp only", normal[..16].to_vec()),
            ("M6 truncated", normal[..normal.len() - 1].to_vec()),
            ("M7 ftyp cut", normal[..12].to_vec()),
            ("M8 brand list not in 4-byte entries", odd_brand_list),
        ];
        let container: Vec<_> = inputs.iter().map(|(name, bytes)| (*name, is_mp4_container(bytes))).collect();
        assert_eq!(container, [
            ("M1 normal", true), ("M2 audio track", true), ("M3 audio brand", false), ("M4 QuickTime", false),
            ("M5 ftyp only", true), ("M6 truncated", true), ("M7 ftyp cut", false), ("M8 brand list not in 4-byte entries", false),
        ]);
        let video: Vec<_> = inputs.iter().map(|(name, bytes)| (*name, mp4_has_video_track(bytes))).collect();
        assert_eq!(video, [
            ("M1 normal", true), ("M2 audio track", false), ("M3 audio brand", false), ("M4 QuickTime", false),
            ("M5 ftyp only", false), ("M6 truncated", false), ("M7 ftyp cut", false), ("M8 brand list not in 4-byte entries", false),
        ]);
    }
}
