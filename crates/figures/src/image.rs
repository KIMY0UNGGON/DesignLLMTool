//! Header-only readers for PNG and JPEG. They confirm the format and the pixel size of an image asset.
//! Pixel data is never decoded, so no image codec dependency is needed.

/// Detected image format and pixel size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub mime: &'static str,
    pub width: u32,
    pub height: u32,
}

pub fn header(bytes: &[u8]) -> Option<Header> {
    png(bytes).or_else(|| jpeg(bytes))
}

fn be32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

fn be16(b: &[u8]) -> u16 {
    u16::from_be_bytes([b[0], b[1]])
}

/// Signature, IHDR chunk type and a positive size; the IHDR length field is not checked. Opening a stored
/// project validates its images with this reader, so it keeps the rule the projects were saved under instead
/// of the stricter `media_header::png_size`.
fn png(bytes: &[u8]) -> Option<Header> {
    if bytes.len() < 24 || bytes[..8] != designllm_contracts::media_header::PNG_SIGNATURE || &bytes[12..16] != b"IHDR" {
        return None;
    }
    let (width, height) = (be32(&bytes[16..20]), be32(&bytes[20..24]));
    if width == 0 || height == 0 {
        return None;
    }
    Some(Header { mime: "image/png", width, height })
}

fn jpeg(bytes: &[u8]) -> Option<Header> {
    if bytes.len() < 4 || bytes[0] != 0xFF || bytes[1] != 0xD8 {
        return None;
    }
    let len = bytes.len();
    let mut i = 2;
    loop {
        while i < len && bytes[i] != 0xFF {
            i += 1;
        }
        while i < len && bytes[i] == 0xFF {
            i += 1;
        }
        if i >= len {
            return None;
        }
        let marker = bytes[i];
        i += 1;
        if marker == 0xD9 || marker == 0xDA {
            return None;
        }
        if marker == 0x01 || (0xD0..=0xD7).contains(&marker) {
            continue;
        }
        if i + 2 > len {
            return None;
        }
        let seg_len = usize::from(be16(&bytes[i..i + 2]));
        let is_sof = (0xC0..=0xCF).contains(&marker) && !matches!(marker, 0xC4 | 0xC8 | 0xCC);
        if is_sof {
            if i + 7 > len {
                return None;
            }
            let height = u32::from(be16(&bytes[i + 3..i + 5]));
            let width = u32::from(be16(&bytes[i + 5..i + 7]));
            if width == 0 || height == 0 {
                return None;
            }
            return Some(Header { mime: "image/jpeg", width, height });
        }
        if seg_len < 2 {
            return None;
        }
        i += seg_len;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_png_dimensions_from_ihdr() {
        let mut png = vec![137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13];
        png.extend_from_slice(b"IHDR");
        png.extend_from_slice(&7u32.to_be_bytes());
        png.extend_from_slice(&9u32.to_be_bytes());
        png.extend_from_slice(&[8, 6, 0, 0, 0]);
        assert_eq!(header(&png), Some(Header { mime: "image/png", width: 7, height: 9 }));
    }

    #[test]
    fn reads_jpeg_dimensions_from_sof() {
        let jpg = [
            0xFF, 0xD8, // SOI
            0xFF, 0xE0, 0x00, 0x04, 0x00, 0x00, // APP0 segment, length 4
            0xFF, 0xC0, 0x00, 0x11, 0x08, 0x00, 0x20, 0x01, 0x00, // SOF0: 0x20 rows, 0x100 columns
        ];
        assert_eq!(header(&jpg), Some(Header { mime: "image/jpeg", width: 256, height: 32 }));
    }

    /// Task 49e input set: signature, IHDR chunk (25 bytes) and IEND chunk; the IHDR CRC is not read.
    fn png_file(width: u32, ihdr_length: u32) -> Vec<u8> {
        let mut out = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        out.extend_from_slice(&ihdr_length.to_be_bytes());
        out.extend_from_slice(b"IHDR");
        out.extend_from_slice(&width.to_be_bytes());
        out.extend_from_slice(&2u32.to_be_bytes());
        out.extend_from_slice(&[8, 6, 0, 0, 0, 0, 0, 0, 0]);
        out.extend_from_slice(&[0, 0, 0, 0, b'I', b'E', b'N', b'D', 0xAE, 0x42, 0x60, 0x82]);
        out
    }

    #[test]
    fn png_input_set_acceptance_through_header() {
        let normal = png_file(4, 13);
        let mut no_signature = normal.clone();
        no_signature[..8].fill(0);
        let no_iend = normal[..normal.len() - 12].to_vec();
        let inputs = [
            ("P1 normal", normal),
            ("P2 no signature", no_signature),
            ("P3 IHDR length 14", png_file(4, 14)),
            ("P4 zero width", png_file(0, 13)),
            ("P5 no IEND", no_iend),
        ];
        let table: Vec<_> = inputs.iter().map(|(name, bytes)| (*name, header(bytes).is_some())).collect();
        println!("49e figures image::header accepts: {table:?}");
        assert_eq!(
            table,
            [("P1 normal", true), ("P2 no signature", false), ("P3 IHDR length 14", true), ("P4 zero width", false), ("P5 no IEND", true)]
        );
    }

    #[test]
    fn rejects_unknown_and_truncated_data() {
        assert_eq!(header(b"GIF89a-not-supported-here-........"), None);
        assert_eq!(header(&[0xFF, 0xD8, 0xFF]), None);
    }
}
