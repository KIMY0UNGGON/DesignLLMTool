//! Regression tests for three inline image boundary rules (orchestrator policy, 2026-11-16 batch):
//! 1. CR LF after `ID` has two readings. One is taken only when it is the single reading whose data end is proven.
//! 2. A key and its abbreviation in one dictionary are one key, so giving both is refused.
//! 3. An `EI` with no white space before it is accepted only when the data length is known from an unfiltered dictionary.
//! A refused image fails its page as a boundary or filter error, so the page is reported as not read.

use super::*;

const BOUNDARY: &str = "PDF_INLINE_IMAGE_BOUNDARY";
const FILTER: &str = "PDF_INLINE_IMAGE_FILTER";

fn operators(data: &[u8]) -> Vec<String> {
    decode_operations(data).expect("the stream parses").into_iter().map(|op| op.operator).collect()
}

fn refusal(data: &[u8]) -> &'static str {
    decode_operations(data).err().expect("the stream is refused").code()
}

/// The bytes after the one image in `data`, which starts with `BI`. The image end must be proven.
fn tail_after_image(data: &[u8]) -> &[u8] {
    let end = inline_image_end(data, 2).expect("the image end is proven");
    &data[end..]
}

// Rule 1: CR LF after ID.

#[test]
fn crlf_after_id_is_accepted_for_a_typical_producer_layout() {
    // RGB, 2 pixels: 6 data bytes, then a space before EI.
    assert_eq!(operators(b"BI /W 2 /H 1 /BPC 8 /CS /RGB ID\r\nabcdef EI Q"), vec!["BI", "Q"]);
    // Grey, 3 bytes, then a line feed before EI.
    let grey = b"BI /W 3 /H 1 /BPC 8 /CS /DeviceGray ID\r\nabc\nEI Q";
    assert_eq!(operators(grey), vec!["BI", "Q"]);
    assert_eq!(tail_after_image(grey), &b" Q"[..]);
}

#[test]
fn crlf_after_id_with_a_fake_ei_in_the_data_takes_the_real_end() {
    // The data is "a EI b". Only the reading that skips CR LF ends at the real EI. The fake " EI " stays data.
    let data = b"BI /W 6 /H 1 /BPC 8 /CS /DeviceGray ID\r\na EI b EI Q";
    assert_eq!(tail_after_image(data), &b" Q"[..]);
    assert_eq!(operators(data), vec!["BI", "Q"]);
}

#[test]
fn crlf_after_id_is_refused_when_a_trailing_white_space_byte_makes_both_readings_end_at_ei() {
    // Skipping only the CR reads LF and A as the data. NUL is then white space before the LF and EI. Skipping CR LF
    // reads A and NUL as the data, and LF is white space before EI. Both readings end at EI, so the end is ambiguous.
    assert_eq!(refusal(b"BI /W 2 /H 1 /BPC 8 /CS /DeviceGray ID\r\nA\x00\nEI Q"), BOUNDARY);
}

#[test]
fn crlf_after_id_is_refused_when_both_readings_end_at_ei_for_an_image_mask() {
    // Skipping only the CR reads LF as the one data byte, and the space is white space before EI. Skipping CR LF reads
    // the space as the data byte, and EI follows it. Both readings end at EI.
    assert_eq!(refusal(b"BI /IM true /W 8 /H 1 ID\r\n EI Q"), BOUNDARY);
}

#[test]
fn crlf_after_id_is_refused_when_neither_reading_ends_at_ei() {
    // Skipping only the CR gives LF and A, which end at B. Skipping CR LF gives A and B, which end at C. Neither end is EI.
    assert_eq!(refusal(b"BI /W 2 /H 1 /BPC 8 /CS /DeviceGray ID\r\nABC EI Q"), BOUNDARY);
}

#[test]
fn crlf_after_id_is_accepted_for_an_image_mask_when_only_the_crlf_reading_ends_at_ei() {
    // The data is 0xff. Skipping only the CR reads the LF as the data, and 0xff is then not white space before EI.
    assert_eq!(operators(b"BI /IM true /W 8 /H 1 ID\r\n\xff EI Q"), vec!["BI", "Q"]);
}

#[test]
fn crlf_after_id_is_refused_for_a_filtered_image() {
    assert_eq!(refusal(b"BI /W 1 /H 1 /BPC 8 /CS /DeviceGray /Filter /FlateDecode ID\r\nx EI Q"), FILTER);
    assert_eq!(refusal(b"BI /W 1 /H 1 /BPC 8 /CS /DeviceGray /F /AHx ID\r\n00> EI Q"), FILTER);
}

#[test]
fn crlf_after_id_is_refused_when_the_data_length_is_not_known() {
    // An unknown colour space name, and no colour space at all.
    assert_eq!(refusal(b"BI /W 1 /H 1 /BPC 8 /CS /ICCBased ID\r\nX EI Q"), BOUNDARY);
    assert_eq!(refusal(b"BI /W 1 /H 1 /BPC 8 ID\r\nX EI Q"), BOUNDARY);
}

// Rule 2: a key and its abbreviation are one key.

#[test]
fn a_key_and_its_abbreviation_in_one_dictionary_are_refused() {
    let cases: [&[u8]; 8] = [
        b"BI /W 1 /Width 1 /H 1 /BPC 8 /CS /DeviceGray ID X EI Q",
        b"BI /W 1 /H 1 /Height 1 /BPC 8 /CS /DeviceGray ID X EI Q",
        b"BI /W 1 /H 1 /BPC 8 /BitsPerComponent 8 /CS /DeviceGray ID X EI Q",
        b"BI /W 1 /H 1 /BPC 8 /CS /DeviceGray /ColorSpace /DeviceGray ID X EI Q",
        b"BI /IM true /ImageMask true /W 8 /H 1 ID \xff EI Q",
        b"BI /W 1 /H 1 /BPC 8 /CS /DeviceGray /D [1 0] /Decode [1 0] ID X EI Q",
        b"BI /W 1 /H 1 /BPC 8 /CS /DeviceGray /DP << >> /DecodeParms << >> ID X EI Q",
        b"BI /W 1 /H 1 /BPC 8 /CS /DeviceGray /I true /Interpolate true ID X EI Q",
    ];
    for data in cases {
        assert_eq!(refusal(data), BOUNDARY, "{}", String::from_utf8_lossy(data));
    }
}

#[test]
fn a_filter_key_and_its_abbreviation_are_refused_as_a_filter() {
    assert_eq!(refusal(b"BI /W 1 /H 1 /BPC 8 /CS /DeviceGray /F /AHx /Filter /AHx ID 00> EI Q"), FILTER);
}

#[test]
fn an_escaped_key_is_compared_after_decoding_with_its_abbreviation() {
    let cases: [&[u8]; 5] = [
        // #57 is W.
        b"BI /#57 1 /Width 1 /H 1 /BPC 8 /CS /DeviceGray ID X EI Q",
        // #48eight is Height.
        b"BI /H 1 /#48eight 1 /W 1 /BPC 8 /CS /DeviceGray ID X EI Q",
        // #42PC is BPC.
        b"BI /W 1 /H 1 /#42PC 8 /BPC 8 /CS /DeviceGray ID X EI Q",
        // #44ecode is Decode.
        b"BI /W 1 /H 1 /BPC 8 /CS /DeviceGray /#44ecode [1 0] /D [1 0] ID X EI Q",
        // #49nterpolate is Interpolate.
        b"BI /W 1 /H 1 /BPC 8 /CS /DeviceGray /#49nterpolate true /I true ID X EI Q",
    ];
    for data in cases {
        assert_eq!(refusal(data), BOUNDARY, "{}", String::from_utf8_lossy(data));
    }
}

#[test]
fn keys_with_different_meanings_are_not_repeats() {
    let full = b"BI /Decode [1 0] /Interpolate true /Width 1 /Height 1 \
                 /BitsPerComponent 8 /ColorSpace /DeviceGray ID X EI Q";
    assert_eq!(operators(full), vec!["BI", "Q"]);
    assert_eq!(operators(b"BI /D [1 0] /I true /W 1 /H 1 /BPC 8 /CS /DeviceGray ID X EI Q"), vec!["BI", "Q"]);
}

#[test]
fn a_colour_space_name_that_is_not_a_device_name_is_refused() {
    // Only the device names and their inline abbreviations have a known component count. A page resource could give
    // any other name a different count.
    assert_eq!(refusal(b"BI /W 1 /H 1 /BPC 8 /CS /Gray ID X EI Q"), BOUNDARY);
    assert_eq!(refusal(b"BI /W 1 /H 1 /BPC 8 /CS /MyCS ID X EI Q"), BOUNDARY);
}

// Rule 3: EI with no white space before it.

#[test]
fn an_ei_glued_to_unfiltered_data_is_accepted_when_the_length_proves_the_end() {
    assert_eq!(tail_after_image(b"BI /W 1 /H 1 /BPC 8 /CS /DeviceGray ID XEI Q"), &b" Q"[..]);
    assert_eq!(operators(b"BI /W 1 /H 1 /BPC 8 /CS /DeviceGray ID XEI Q"), vec!["BI", "Q"]);
    assert_eq!(operators(b"BI /IM true /W 8 /H 1 ID \xffEI Q"), vec!["BI", "Q"]);
    // A delimiter may follow EI with no white space, as "(" does here, and then the next operator is read.
    assert_eq!(operators(b"BI /W 2 /H 1 /BPC 8 /CS /DeviceGray ID\nXYEI(Tail) Tj"), vec!["BI", "Tj"]);
}

#[test]
fn a_glued_ei_is_refused_when_the_length_does_not_end_there() {
    // Two bytes are given, so the data is X and E. The I that follows is not EI.
    assert_eq!(refusal(b"BI /W 2 /H 1 /BPC 8 /CS /DeviceGray ID XEI Q"), BOUNDARY);
    // One byte is given, and Y follows X.
    assert_eq!(refusal(b"BI /W 1 /H 1 /BPC 8 /CS /DeviceGray ID XYEI Q"), BOUNDARY);
}

#[test]
fn a_glued_ei_must_still_be_a_token_of_its_own() {
    assert_eq!(refusal(b"BI /W 1 /H 1 /BPC 8 /CS /DeviceGray ID XEIQ"), BOUNDARY);
    assert_eq!(refusal(b"BI /W 1 /H 1 /BPC 8 /CS /DeviceGray ID XEIEI Q"), BOUNDARY);
}

#[test]
fn a_glued_ei_on_a_filtered_image_is_refused() {
    assert_eq!(refusal(b"BI /W 1 /H 1 /BPC 8 /CS /DeviceGray /F /AHx ID 00>EI Q"), FILTER);
}

#[test]
fn a_fake_glued_ei_inside_the_data_does_not_end_the_image() {
    // The data is "AEIB". Its EI is data, and the EI after it is the real end.
    let data = b"BI /W 4 /H 1 /BPC 8 /CS /DeviceGray ID AEIBEI Q";
    assert_eq!(operators(data), vec!["BI", "Q"]);
    assert_eq!(tail_after_image(data), &b" Q"[..]);
    // The data is "EI(x". The EI followed by "(" is data, so the image is not cut there.
    assert_eq!(operators(b"BI /W 4 /H 1 /BPC 8 /CS /DeviceGray ID EI(x EI Q"), vec!["BI", "Q"]);
    // The data is "EIX". Its EI is followed by X, so it is not a token, and the real EI ends the image.
    assert_eq!(operators(b"BI /W 3 /H 1 /BPC 8 /CS /DeviceGray ID\nEIXEI Q"), vec!["BI", "Q"]);
}

// Dictionary values, and the Indexed colour space.

#[test]
fn a_dictionary_value_closes_each_bracket_with_its_own_kind() {
    // A "]" closes only "[", and ">>" closes only "<<". A crossed pair makes the value malformed.
    assert_eq!(refusal(b"BI /W 1 /H 1 /BPC 8 /CS /DeviceGray /DP [ << ] >> ID X EI Q"), BOUNDARY);
    assert_eq!(refusal(b"BI /W 1 /H 1 /BPC 8 /CS /DeviceGray /D [1 0 >> ID X EI Q"), BOUNDARY);
    assert_eq!(refusal(b"BI /W 1 /H 1 /BPC 8 /CS /DeviceGray /DP << /A 1 ] ID X EI Q"), BOUNDARY);
    // Nested containers that close in order are skipped as a whole.
    assert_eq!(
        operators(b"BI /W 1 /H 1 /BPC 8 /CS /DeviceGray /DP << /A [1 << /B 2 >>] >> ID X EI Q"),
        vec!["BI", "Q"]
    );
}

#[test]
fn an_indexed_colour_space_needs_a_base_a_hival_and_a_lookup_string() {
    assert_eq!(operators(b"BI /W 1 /H 1 /BPC 8 /CS [/Indexed /RGB 0 <00>] ID X EI Q"), vec!["BI", "Q"]);
    assert_eq!(operators(b"BI /W 1 /H 1 /BPC 8 /CS [/I [/CalRGB << >>] 255 (abc)] ID X EI Q"), vec!["BI", "Q"]);
    let refused: [&[u8]; 11] = [
        b"BI /W 1 /H 1 /BPC 8 /CS [/I] ID X EI Q",
        b"BI /W 1 /H 1 /BPC 8 /CS [/Indexed /UnknownCS] ID X EI Q",
        b"BI /W 1 /H 1 /BPC 8 /CS [/I 5 1 <00>] ID X EI Q",
        b"BI /W 1 /H 1 /BPC 8 /CS [/I << >> 1 <00>] ID X EI Q",
        b"BI /W 1 /H 1 /BPC 8 /CS [/I /RGB 256 <00>] ID X EI Q",
        b"BI /W 1 /H 1 /BPC 8 /CS [/I /RGB -1 <00>] ID X EI Q",
        b"BI /W 1 /H 1 /BPC 8 /CS [/I /RGB 1.5 <00>] ID X EI Q",
        b"BI /W 1 /H 1 /BPC 8 /CS [/I /RGB 1] ID X EI Q",
        b"BI /W 1 /H 1 /BPC 8 /CS [/I /RGB 1 /x] ID X EI Q",
        b"BI /W 1 /H 1 /BPC 8 /CS [/I /RGB 1 << >>] ID X EI Q",
        b"BI /W 1 /H 1 /BPC 8 /CS [/I /RGB 1 <00> /x] ID X EI Q",
    ];
    for data in refused {
        assert_eq!(refusal(data), BOUNDARY, "{}", String::from_utf8_lossy(data));
    }
}

// A filter given as an array, bits per component, overflowing lengths, and a lone CR.

#[test]
fn a_filter_given_as_an_empty_array_is_refused() {
    assert_eq!(refusal(b"BI /W 1 /H 1 /BPC 8 /CS /DeviceGray /F [] ID x EI Q"), FILTER);
    assert_eq!(refusal(b"BI /W 1 /H 1 /BPC 8 /CS /DeviceGray /Filter [] ID x EI Q"), FILTER);
}

#[test]
fn bits_per_component_of_an_unmasked_image_must_be_1_2_4_8_or_16() {
    assert_eq!(refusal(b"BI /W 1 /H 1 /BPC 0 /CS /DeviceGray ID x EI Q"), BOUNDARY);
    assert_eq!(refusal(b"BI /W 1 /H 1 /BPC 3 /CS /DeviceGray ID x EI Q"), BOUNDARY);
    // 16 bits is legal, and one pixel is 2 bytes.
    assert_eq!(operators(b"BI /W 1 /H 1 /BPC 16 /CS /DeviceGray ID \x00\x01 EI Q"), vec!["BI", "Q"]);
    // The 2 bytes are "xy", and "z" follows them instead of EI.
    assert_eq!(refusal(b"BI /W 1 /H 1 /BPC 16 /CS /DeviceGray ID xyz EI Q"), BOUNDARY);
}

#[test]
fn a_length_whose_product_overflows_is_refused() {
    // Width times components is larger than u64::MAX.
    assert_eq!(refusal(b"BI /W 9223372036854775807 /H 2 /BPC 8 /CS /DeviceRGB ID x EI Q"), BOUNDARY);
    // Each product fits, but height times the row size does not.
    assert_eq!(refusal(b"BI /W 4294967296 /H 4294967296 /BPC 8 /CS /DeviceGray ID x EI Q"), BOUNDARY);
    // The length fits in a usize, but the data does not reach it.
    assert_eq!(refusal(b"BI /W 1 /H 9223372036854775807 /BPC 8 /CS /DeviceGray ID x EI Q"), BOUNDARY);
}

#[test]
fn a_lone_cr_after_id_is_the_separator_and_a_fake_ei_stays_data() {
    // The CR is not followed by LF, so it alone is the separator. The 6 data bytes are "a EI b".
    let data = b"BI /W 6 /H 1 /BPC 8 /CS /DeviceGray ID\ra EI b EI Q";
    assert_eq!(operators(data), vec!["BI", "Q"]);
    assert_eq!(tail_after_image(data), &b" Q"[..]);
}
