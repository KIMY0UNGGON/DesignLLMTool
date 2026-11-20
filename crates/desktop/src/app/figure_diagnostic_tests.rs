//! The words the window shows for an image the core refuses (`diagnostic_message` in app.rs): an animated or cut-off file
//! says what to pick instead. Only a blocking diagnostic gives its words; any other refusal keeps the generic ones.

use designllm_contracts::{Diagnostic, DltError, ErrorCode};

use super::user_error;

/// A refusal of an image import that carries one diagnostic.
fn refused(code: &str, blocking: bool) -> DltError {
    let mut error = DltError::new(ErrorCode::InvalidArgument, "the image was refused");
    error.diagnostics = vec![Diagnostic {
        code: code.into(),
        message: "the core refused the image".into(),
        node_id: None,
        blocking,
        params: Default::default(),
    }];
    error
}

#[test]
fn an_animated_gif_is_refused_in_words_that_say_what_to_pick() {
    assert_eq!(user_error(&refused("IMAGE_ANIMATION_UNSUPPORTED", true)), "애니메이션 GIF는 가져올 수 없습니다. 움직이지 않는 이미지를 고르세요");
}

#[test]
fn a_cut_off_image_is_refused_in_words_that_say_what_to_pick() {
    assert_eq!(user_error(&refused("IMAGE_CORRUPT", true)), "이미지 파일이 손상됐거나 끝까지 읽히지 않습니다. 다른 파일을 고르세요");
}

#[test]
fn a_refusal_without_a_blocking_diagnostic_keeps_the_generic_words() {
    assert_eq!(user_error(&refused("IMAGE_CORRUPT", false)), "입력 내용을 확인해 주세요");
}
