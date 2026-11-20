//! Pixel size of PNG, JPEG, GIF and WebP images from their headers (asset.import/lookup report it;
//! figure images need it). No decoding: only the header fields are read. A GIF is also walked block by block at import: an
//! animated GIF, or one whose blocks do not parse to the trailer, is refused, because the figure and the canvas keep only the
//! first frame of an animation.

use designllm_contracts::{Diagnostic, DltError, ErrorCode};

/// Bytes of an asset read to find a JPEG frame header (EXIF and other segments come first).
pub(crate) const HEADER_BYTES: usize = 1 << 20;

/// (width, height) in pixels, or None for another format, a truncated header or a zero size.
pub(crate) fn image_size(b: &[u8]) -> Option<(u32, u32)> {
    let le16 = |i: usize| -> Option<u32> { Some(u32::from(u16::from_le_bytes([*b.get(i)?, *b.get(i + 1)?]))) };
    let le24 = |i: usize| -> Option<u32> { Some(u32::from(*b.get(i)?) | u32::from(*b.get(i + 1)?) << 8 | u32::from(*b.get(i + 2)?) << 16) };
    let be32 = |i: usize| -> Option<u32> { Some(u32::from_be_bytes([*b.get(i)?, *b.get(i + 1)?, *b.get(i + 2)?, *b.get(i + 3)?])) };
    let size = if b.starts_with(&designllm_contracts::media_header::PNG_SIGNATURE) {
        if b.get(12..16)? != b"IHDR" { return None; }
        (be32(16)?, be32(20)?)
    } else if b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a") {
        (le16(6)?, le16(8)?)
    } else if b.starts_with(&[0xFF, 0xD8]) {
        jpeg(b)?
    } else if b.get(0..4)? == b"RIFF" && b.get(8..12)? == b"WEBP" {
        match b.get(12..16)? {
            b"VP8 " => {
                if b.get(23..26)? != [0x9D, 0x01, 0x2A] { return None; }
                (le16(26)? & 0x3FFF, le16(28)? & 0x3FFF)
            }
            b"VP8L" => {
                if *b.get(20)? != 0x2F { return None; }
                let bits = u32::from_le_bytes([*b.get(21)?, *b.get(22)?, *b.get(23)?, *b.get(24)?]);
                ((bits & 0x3FFF) + 1, ((bits >> 14) & 0x3FFF) + 1)
            }
            b"VP8X" => (le24(24)? + 1, le24(27)? + 1),
            _ => return None,
        }
    } else {
        return None;
    };
    (size.0 > 0 && size.1 > 0).then_some(size)
}

/// The first start-of-frame segment (SOF0..SOF15 except DHT, JPG and DAC) of a JPEG.
fn jpeg(b: &[u8]) -> Option<(u32, u32)> {
    let mut i = 2;
    loop {
        if *b.get(i)? != 0xFF { return None; }
        while *b.get(i + 1)? == 0xFF { i += 1; }
        let marker = *b.get(i + 1)?;
        if marker == 0x01 || (0xD0..=0xD8).contains(&marker) { i += 2; continue; }
        if marker == 0xD9 || marker == 0xDA { return None; }
        let len = usize::from(u16::from_be_bytes([*b.get(i + 2)?, *b.get(i + 3)?]));
        if (0xC0..=0xCF).contains(&marker) && !matches!(marker, 0xC4 | 0xC8 | 0xCC) {
            let h = u32::from(u16::from_be_bytes([*b.get(i + 5)?, *b.get(i + 6)?]));
            let w = u32::from(u16::from_be_bytes([*b.get(i + 7)?, *b.get(i + 8)?]));
            return Some((w, h));
        }
        if len < 2 { return None; }
        i += 2 + len;
    }
}

/// Diagnostic code of an animated GIF refused at import. It is an unsupported operation, as the canvas refuses an animated PNG crop.
pub(crate) const ANIMATED_CODE: &str = "IMAGE_ANIMATION_UNSUPPORTED";
/// Diagnostic code of a GIF refused at import because its blocks do not parse to the trailer, or because it holds no image.
pub(crate) const CORRUPT_CODE: &str = "IMAGE_CORRUPT";

/// Why an imported GIF is refused.
#[derive(Debug, PartialEq, Eq)]
enum GifRefusal {
    /// Two or more frames. The figure and the canvas would keep only the first one.
    Animated(u32),
    /// The stream is complete but holds no image frame.
    NoFrame,
    /// Declared or named as GIF, but missing the 6-byte GIF87a/GIF89a signature.
    BadSignature,
    /// Logical screen or image frame has a zero width or height.
    ZeroSize,
    /// A block does not parse, or the data ends before the trailer: the byte offset where the walk stopped.
    Malformed(usize),
}

/// The import refusal of a GIF as the core reports it: a stable diagnostic code, the error class of the canvas refusals for the
/// same reason, and the message. `None` for a GIF with one complete frame, and for bytes that are not a GIF.
#[allow(dead_code)]
pub(crate) fn import_refusal(b: &[u8]) -> Option<DltError> {
    import_refusal_for(b, false)
}

/// Same as `import_refusal`, and also refuses bytes without a GIF signature as corrupt when `expect_gif` is true.
pub(crate) fn import_refusal_for(b: &[u8], expect_gif: bool) -> Option<DltError> {
    let (code, class, message) = match gif_refusal(b, expect_gif)? {
        GifRefusal::Animated(frames) => (
            ANIMATED_CODE,
            ErrorCode::UnsupportedOperation,
            format!("animated GIF is not supported: it has {frames} frames, and only the first would be kept. Import a still image instead"),
        ),
        GifRefusal::NoFrame => (CORRUPT_CODE, ErrorCode::InvalidArgument, "GIF has no image frame".to_owned()),
        GifRefusal::BadSignature => (
            CORRUPT_CODE,
            ErrorCode::InvalidArgument,
            "GIF header is missing or truncated (expected GIF87a or GIF89a)".to_owned(),
        ),
        GifRefusal::ZeroSize => (CORRUPT_CODE, ErrorCode::InvalidArgument, "GIF has zero width or height".to_owned()),
        GifRefusal::Malformed(at) => (
            CORRUPT_CODE,
            ErrorCode::InvalidArgument,
            format!("GIF data is malformed or truncated at byte {at}, so its frames cannot be checked"),
        ),
    };
    let mut error = DltError::new(class, message.clone());
    error.diagnostics.push(Diagnostic { code: code.to_owned(), message, node_id: None, blocking: true, params: Default::default() });
    Some(error)
}

/// The refusal of a GIF signature (or non-GIF bytes when `expect_gif` is true). `None` for a stream of one complete frame.
fn gif_refusal(b: &[u8], expect_gif: bool) -> Option<GifRefusal> {
    if !(b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a")) {
        return expect_gif.then_some(GifRefusal::BadSignature);
    }
    match gif_frames(b) {
        Ok(0) => Some(GifRefusal::NoFrame),
        Ok(1) => None,
        Ok(frames) => Some(GifRefusal::Animated(frames)),
        Err(refusal) => Some(refusal),
    }
}

/// Walks the blocks of a GIF to its trailer and counts its image descriptors.
fn gif_frames(b: &[u8]) -> Result<u32, GifRefusal> {
    let at = |i: usize| b.get(i).copied().ok_or(GifRefusal::Malformed(i));
    // The 7-byte logical screen descriptor follows the 6-byte signature: width (6..8), height (8..10), packed (10),
    // background colour index (11), and pixel aspect ratio (12).
    at(12)?;
    let screen_w = u16::from_le_bytes([b[6], b[7]]);
    let screen_h = u16::from_le_bytes([b[8], b[9]]);
    if screen_w == 0 || screen_h == 0 {
        return Err(GifRefusal::ZeroSize);
    }
    let mut i = 13;
    if (b[10] & 0x80) != 0 {
        i += color_table_len(b[10]);
    }
    let mut frames = 0u32;
    loop {
        match at(i)? {
            0x3B => return Ok(frames),
            // An extension: its label, then data sub-blocks. Known fixed-size headers are checked before skipping.
            0x21 => {
                let label = at(i + 1)?;
                let block_size = at(i + 2)?;
                let expected_size = match label {
                    0xF9 => Some(4),
                    0xFF => Some(11),
                    0x01 => Some(12),
                    _ => None,
                };
                if expected_size.is_some_and(|want| block_size != want) {
                    return Err(GifRefusal::Malformed(i + 2));
                }
                i = if label == 0xF9 {
                    // The graphic control block is one 4-byte block and no other sub-block: its 0x00 terminator follows at once.
                    let terminator = i + 3 + usize::from(block_size);
                    if at(terminator)? != 0 {
                        return Err(GifRefusal::Malformed(terminator));
                    }
                    terminator + 1
                } else {
                    // A fixed-size application or plain text header is the first sub-block; its data sub-blocks follow.
                    skip_sub_blocks(b, i + 2).map_err(GifRefusal::Malformed)?
                };
            }
            // An image: nine descriptor bytes, a colour table when the local flag is set, the LZW minimum code size, then data.
            0x2C => {
                let packed = at(i + 9)?;
                let frame_w = u16::from_le_bytes([b[i + 5], b[i + 6]]);
                let frame_h = u16::from_le_bytes([b[i + 7], b[i + 8]]);
                if frame_w == 0 || frame_h == 0 {
                    return Err(GifRefusal::ZeroSize);
                }
                let mut data = i + 10;
                if (packed & 0x80) != 0 {
                    data += color_table_len(packed);
                }
                at(data)?;
                i = skip_sub_blocks(b, data + 1).map_err(GifRefusal::Malformed)?;
                frames = frames.saturating_add(1);
            }
            _ => return Err(GifRefusal::Malformed(i)),
        }
    }
}

/// Skips the data sub-blocks that start at `start`, through their zero-length terminator. Returns the offset after it.
fn skip_sub_blocks(b: &[u8], start: usize) -> Result<usize, usize> {
    let mut i = start;
    loop {
        let size = usize::from(*b.get(i).ok_or(i)?);
        if size == 0 {
            return Ok(i + 1);
        }
        i += 1 + size;
        if i > b.len() {
            return Err(b.len());
        }
    }
}

/// Bytes of a colour table whose size is in the low three bits of `packed`: 2^(size + 1) entries of three bytes each.
fn color_table_len(packed: u8) -> usize {
    3usize << ((packed & 0x07) + 1)
}

#[cfg(test)]
mod tests {
    use super::image_size;

    #[test]
    fn headers_of_the_four_formats_give_their_pixel_size() {
        let mut png = b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR".to_vec();
        png.extend_from_slice(&640u32.to_be_bytes());
        png.extend_from_slice(&480u32.to_be_bytes());
        assert_eq!(image_size(&png), Some((640, 480)));
        assert_eq!(image_size(b"GIF89a\x20\x03\x58\x02"), Some((800, 600)));
        // SOI, an APP0 segment, then SOF0 (precision 8, height 300, width 400).
        let jpeg = [0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x04, 0x4A, 0x46, 0xFF, 0xC0, 0x00, 0x11, 0x08, 0x01, 0x2C, 0x01, 0x90, 0x03];
        assert_eq!(image_size(&jpeg), Some((400, 300)));
        let mut lossy = b"RIFF\0\0\0\0WEBPVP8 \0\0\0\0".to_vec();
        lossy.extend_from_slice(&[0x00, 0x00, 0x00, 0x9D, 0x01, 0x2A, 0x80, 0x02, 0xE0, 0x01]);
        assert_eq!(image_size(&lossy), Some((640, 480)));
        let mut lossless = b"RIFF\0\0\0\0WEBPVP8L\0\0\0\0\x2F".to_vec();
        let bits: u32 = 99 | (49 << 14);
        lossless.extend_from_slice(&bits.to_le_bytes());
        assert_eq!(image_size(&lossless), Some((100, 50)));
        let mut extended = b"RIFF\0\0\0\0WEBPVP8X\0\0\0\0\0\0\0\0".to_vec();
        extended.extend_from_slice(&[0x1F, 0x03, 0x00, 0x57, 0x02, 0x00]);
        assert_eq!(image_size(&extended), Some((800, 600)));
    }

    #[test]
    fn other_or_truncated_data_has_no_size() {
        for bad in [&b"%PDF-1.7"[..], b"\x89PNG\r\n\x1a\n\0\0", b"GIF89a\x00\x00\x00\x00", &[0xFF, 0xD8, 0xFF, 0xDA, 0x00, 0x02], b"RIFF\0\0\0\0WEBPVP8 ", b"x,y\n1,2\n"] {
            assert_eq!(image_size(bad), None, "{bad:?}");
        }
    }
}
