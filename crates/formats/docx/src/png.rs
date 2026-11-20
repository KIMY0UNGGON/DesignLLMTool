//! Header check for figure render assets (PNG): the shared rule plus the closing IEND chunk
//! (`media_header::png_size_with_iend`, decision D2). Pixel data is not decoded.

/// Pixel width and height of a complete PNG. `None` when the bytes are not a PNG with a readable IHDR chunk and a
/// closing IEND chunk, or when either dimension is zero.
pub use designllm_contracts::media_header::png_size_with_iend as pixel_size;

#[cfg(test)]
mod tests {
    use super::*;
    use designllm_contracts::media_header::PNG_SIGNATURE as SIGNATURE;
    const IHDR_LENGTH: [u8; 4] = [0, 0, 0, 13];
    const IEND_CHUNK: [u8; 12] = [0, 0, 0, 0, b'I', b'E', b'N', b'D', 0xAE, 0x42, 0x60, 0x82];

    /// A PNG whose pixel data is empty. The reader does not decode pixels, so only the chunk layout matters.
    fn minimal(width: u32, height: u32) -> Vec<u8> {
        let mut out = SIGNATURE.to_vec();
        out.extend_from_slice(&IHDR_LENGTH);
        out.extend_from_slice(b"IHDR");
        out.extend_from_slice(&width.to_be_bytes());
        out.extend_from_slice(&height.to_be_bytes());
        out.extend_from_slice(&[8, 2, 0, 0, 0]);
        out.extend_from_slice(&[0, 0, 0, 0]);
        out.extend_from_slice(&IEND_CHUNK);
        out
    }

    #[test]
    fn reads_the_pixel_size_of_a_well_formed_png() {
        assert_eq!(pixel_size(&minimal(3, 2)), Some((3, 2)));
    }

    #[test]
    fn rejects_other_bytes_zero_sizes_and_truncated_files() {
        assert_eq!(pixel_size(b"not an image at all, just text padding to reach length"), None);
        assert_eq!(pixel_size(&minimal(0, 2)), None);
        assert_eq!(pixel_size(&minimal(3, 0)), None);
        let mut truncated = minimal(3, 2);
        truncated.truncate(truncated.len() - 4);
        assert_eq!(pixel_size(&truncated), None, "a missing IEND means a truncated file");
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

    /// The PNG gate of `update_figure_link` (ops.rs) on the input set.
    #[test]
    fn png_input_set_acceptance_of_the_figure_link_gate() {
        let normal = png_file(4, 13);
        let mut no_signature = normal.clone();
        no_signature[..8].fill(0);
        let no_iend = normal[..normal.len() - 12].to_vec();
        let inputs = [("P1 normal", normal), ("P2 no signature", no_signature), ("P3 IHDR length 14", png_file(4, 14)), ("P4 zero width", png_file(0, 13)), ("P5 no IEND", no_iend)];
        let table: Vec<_> = inputs.iter().map(|(name, bytes)| (*name, pixel_size(bytes).is_some())).collect();
        println!("49e docx UpdateFigureLink PNG gate accepts: {table:?}");
        assert_eq!(table, [("P1 normal", true), ("P2 no signature", false), ("P3 IHDR length 14", false), ("P4 zero width", false), ("P5 no IEND", false)]);
    }
}
