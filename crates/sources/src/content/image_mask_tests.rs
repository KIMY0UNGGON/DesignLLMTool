//! Focused tests for BitsPerComponent of inline image masks (ISO 32000-1 Table 89).
//! A mask may omit BitsPerComponent, which then means 1 bit per pixel. A given value must be 1. Other images still need
//! a legal value. The payload end always comes from the dictionary, so bytes that look like EI, operators, strings or
//! comments inside the payload are data.

use super::*;

const BOUNDARY: &str = "PDF_INLINE_IMAGE_BOUNDARY";

fn operators(data: &[u8]) -> Vec<String> {
    decode_operations(data).expect("the stream parses").into_iter().map(|op| op.operator).collect()
}

fn refusal(data: &[u8]) -> &'static str {
    decode_operations(data).err().expect("the stream is refused").code()
}

/// `BI /IM true` with the given size and no BitsPerComponent, then `count` data bytes of 0x55, then `tail`.
fn image_mask(width: u32, height: u32, count: usize, tail: &[u8]) -> Vec<u8> {
    let mut data = format!("BI /IM true /W {width} /H {height} ID ").into_bytes();
    data.extend(std::iter::repeat(0x55u8).take(count));
    data.extend_from_slice(tail);
    data
}

#[test]
fn an_image_mask_may_omit_bits_per_component() {
    // 8 pixels in one row are one data byte.
    assert_eq!(operators(b"BI /IM true /W 8 /H 1 ID \xff EI Q"), vec!["BI", "Q"]);
    assert_eq!(operators(b"BI /ImageMask true /Width 8 /Height 1 ID \xff EI Q"), vec!["BI", "Q"]);
    assert_eq!(operators(b"BI /Decode [1 0] /IM true /W 8 /H 1 ID \xff EI Q"), vec!["BI", "Q"]);
}

#[test]
fn an_image_mask_with_bits_per_component_one_is_accepted() {
    assert_eq!(operators(b"BI /IM true /W 8 /H 1 /BPC 1 ID \xff EI Q"), vec!["BI", "Q"]);
    assert_eq!(operators(b"BI /IM true /W 9 /H 2 /BPC 1 ID \xff\x80\x7f\x01 EI Q"), vec!["BI", "Q"]);
    assert_eq!(
        operators(b"BI /ImageMask true /Width 8 /Height 1 /BitsPerComponent 1 ID \xff EI Q"),
        vec!["BI", "Q"]
    );
}

#[test]
fn an_image_mask_with_another_bits_per_component_is_refused() {
    for value in ["-1", "0", "2", "3", "4", "8", "16"] {
        let mut data = format!("BI /IM true /W 8 /H 1 /BPC {value} ID ").into_bytes();
        data.extend_from_slice(b"\xff EI Q");
        assert_eq!(refusal(&data), BOUNDARY, "BPC {value}");
    }
    assert_eq!(refusal(b"BI /ImageMask true /Width 8 /Height 1 /BitsPerComponent 8 ID \xff EI Q"), BOUNDARY);
}

#[test]
fn an_image_that_is_not_a_mask_still_needs_bits_per_component() {
    assert_eq!(refusal(b"BI /W 8 /H 1 /CS /DeviceGray ID \xff EI Q"), BOUNDARY);
    assert_eq!(refusal(b"BI /IM false /W 8 /H 1 /CS /DeviceGray ID \xff EI Q"), BOUNDARY);
    assert_eq!(operators(b"BI /IM false /W 8 /H 1 /BPC 1 /CS /DeviceGray ID \xff EI Q"), vec!["BI", "Q"]);
}

#[test]
fn image_mask_rows_are_whole_bytes_and_the_payload_ends_where_the_dictionary_says() {
    // 9 pixels need 2 bytes per row, so 2 rows are 4 bytes.
    assert_eq!(operators(b"BI /IM true /W 9 /H 2 ID \xff\x80\x7f\x01 EI Q"), vec!["BI", "Q"]);
    // Only 2 bytes come before EI.
    assert_eq!(refusal(b"BI /IM true /W 9 /H 2 ID \xff\x80EI Q"), BOUNDARY);
    // A 5th byte comes before EI.
    assert_eq!(refusal(b"BI /IM true /W 9 /H 2 ID \xff\x80\x7f\x01\x02 EI Q"), BOUNDARY);
}

#[test]
fn image_mask_length_is_whole_bytes_per_row_for_any_width() {
    // (width, height, data bytes): each row is ceil(width / 8) bytes.
    for (width, height, bytes) in [(1, 1, 1), (7, 3, 3), (8, 2, 2), (9, 2, 4), (17, 1, 3)] {
        assert_eq!(operators(&image_mask(width, height, bytes, b" EI Q")), vec!["BI", "Q"], "W {width} H {height}");
        assert_eq!(refusal(&image_mask(width, height, bytes - 1, b"EI Q")), BOUNDARY, "one byte short, W {width}");
        assert_eq!(refusal(&image_mask(width, height, bytes + 1, b" EI Q")), BOUNDARY, "one byte long, W {width}");
    }
}

#[test]
fn bytes_that_look_like_ei_or_operators_inside_a_mask_are_data() {
    // The 2 data bytes are "EI". The EI after them is the real end.
    assert_eq!(operators(b"BI /IM true /W 16 /H 1 ID EI EI Q"), vec!["BI", "Q"]);
    // The 4 data bytes are "EI Q", and neither word is read as an operator.
    assert_eq!(operators(b"BI /IM true /W 32 /H 1 ID EI Q EI Q"), vec!["BI", "Q"]);
    // A data byte that opens a string or a comment opens nothing.
    assert_eq!(operators(b"BI /IM true /W 16 /H 1 ID (% EI Q"), vec!["BI", "Q"]);
    // NUL and FF right before EI are data, not the gap.
    assert_eq!(operators(b"BI /IM true /W 16 /H 1 ID \x00\x0c EI Q"), vec!["BI", "Q"]);
}

#[test]
fn an_image_mask_needs_its_own_ei_after_the_payload() {
    assert_eq!(refusal(b"BI /IM true /W 8 /H 1 ID \xff Q"), BOUNDARY);
    assert_eq!(refusal(b"BI /IM true /W 8 /H 1 ID \xff EIQ"), BOUNDARY);
}

#[test]
fn bounded_copy_of_an_image_mask_replaces_only_the_image() {
    let (bounded, images) =
        bound_inline_images(b"q BI /IM true /W 9 /H 2 ID \xff\x80\x7f\x01 EI Q").expect("bounded");
    assert_eq!(images, 1);
    assert_eq!(&*bounded, &b"q BI /W 1 /H 1 /BPC 8 /CS /DeviceGray ID 0 EI Q"[..]);
}
