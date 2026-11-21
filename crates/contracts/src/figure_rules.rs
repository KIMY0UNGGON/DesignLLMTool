//! Figure rules that the figure engine, the bridge's review gate and the desktop share: the diagram origin codes, the
//! evidence review states, the visual review id, the image rotation bound and the decimal digit bound. Plain constants;
//! nothing here is serialized.

/// Origin code of a diagram element taken from paper evidence. It needs an evidence reference.
pub const PAPER_EVIDENCE_ORIGIN: &str = "paper_evidence";
/// Origin code of a diagram element the user wrote.
pub const USER_AUTHORED_ORIGIN: &str = "user_authored";
/// Origin code of a diagram element the model proposed. A connector with this origin and no evidence
/// raises UNSOURCED_PROPOSAL.
pub const MODEL_PROPOSAL_ORIGIN: &str = "model_proposal";

/// The origin codes a diagram node or edge may carry. The figure engine refuses any other code.
pub const DIAGRAM_ORIGINS: [&str; 3] = [PAPER_EVIDENCE_ORIGIN, USER_AUTHORED_ORIGIN, MODEL_PROPOSAL_ORIGIN];

/// `Evidence.review_state` of a record no person has reviewed yet.
pub const REVIEW_STATE_UNREVIEWED: &str = "unreviewed";
/// `Evidence.review_state` of a record a person provided.
pub const REVIEW_STATE_USER_PROVIDED: &str = "user_provided";
/// `Evidence.review_state` of a record a person reviewed.
pub const REVIEW_STATE_REVIEWED: &str = "reviewed";
/// The review states an evidence record may carry. The figure engine refuses any other state.
pub const REVIEW_STATES: [&str; 3] = [REVIEW_STATE_UNREVIEWED, REVIEW_STATE_USER_PROVIDED, REVIEW_STATE_REVIEWED];

/// The id the figure engine reports in `changed_ids` for its visual review. The review has no element of its own, so no
/// element may use it.
pub const VISUAL_REVIEW_ID: &str = "visual_review";

/// The bound on an image's rotation magnitude in millidegrees (1/1000 degree). A rotation is valid when its magnitude
/// is below this value, so the valid range lies strictly inside (-360, 360) degrees.
pub const IMAGE_ROTATION_LIMIT_MILLIDEG: i32 = 360_000;

/// The most digits a decimal value may have, integer and fraction digits together. Longer text is refused.
pub const DECIMAL_MAX_DIGITS: usize = 18;

use crate::{EMU_PER_MM, EMU_PER_PT};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FigureOutputProfile {
    pub id: &'static str,
    pub label: &'static str,
    pub width_mm: u16,
    pub height_mm: u16,
    pub width_emu: i64,
    pub height_emu: i64,
    pub font_size_emu: i64,
    pub stroke_width_emu: i64,
    pub default_dpi: u32,
    pub palette: [&'static str; 6],
}

pub const FIGURE_OUTPUT_PROFILE_DISCLAIMER: &str = "사용자 선택 출력 프로필입니다. 특정 학회·저널의 최신 공식 규정을 보장하지 않습니다.";
const DEFAULT_PALETTE: [&str; 6] = ["#1F4E79", "#C0504D", "#4F9B3C", "#7F6000", "#7030A0", "#2E75B6"];
const PRINT_PALETTE: [&str; 6] = ["#111827", "#1F4E79", "#9A3412", "#166534", "#5B21B6", "#4B5563"];
pub const FIGURE_OUTPUT_PROFILES: &[FigureOutputProfile] = &[
    FigureOutputProfile { id: "single_column", label: "단일 단 (85 × 65 mm)", width_mm: 85, height_mm: 65, width_emu: 85 * EMU_PER_MM, height_emu: 65 * EMU_PER_MM, font_size_emu: 8 * EMU_PER_PT, stroke_width_emu: EMU_PER_PT * 3 / 4, default_dpi: 300, palette: DEFAULT_PALETTE },
    FigureOutputProfile { id: "one_half_column", label: "1.5단 (120 × 90 mm)", width_mm: 120, height_mm: 90, width_emu: 120 * EMU_PER_MM, height_emu: 90 * EMU_PER_MM, font_size_emu: 9 * EMU_PER_PT, stroke_width_emu: EMU_PER_PT * 3 / 4, default_dpi: 300, palette: DEFAULT_PALETTE },
    FigureOutputProfile { id: "double_column", label: "2단 (175 × 120 mm)", width_mm: 175, height_mm: 120, width_emu: 175 * EMU_PER_MM, height_emu: 120 * EMU_PER_MM, font_size_emu: 9 * EMU_PER_PT, stroke_width_emu: EMU_PER_PT, default_dpi: 300, palette: DEFAULT_PALETTE },
    FigureOutputProfile { id: "high_res_print", label: "고해상도 인쇄 (175 × 120 mm · 600 DPI)", width_mm: 175, height_mm: 120, width_emu: 175 * EMU_PER_MM, height_emu: 120 * EMU_PER_MM, font_size_emu: 9 * EMU_PER_PT, stroke_width_emu: EMU_PER_PT, default_dpi: 600, palette: PRINT_PALETTE },
    FigureOutputProfile { id: "presentation_16_9", label: "발표 16:9 (254 × 143 mm · 150 DPI)", width_mm: 254, height_mm: 143, width_emu: 254 * EMU_PER_MM, height_emu: 143 * EMU_PER_MM, font_size_emu: 12 * EMU_PER_PT, stroke_width_emu: EMU_PER_PT * 3 / 2, default_dpi: 150, palette: PRINT_PALETTE },
];

pub fn figure_output_profile(id: &str) -> Option<&'static FigureOutputProfile> {
    FIGURE_OUTPUT_PROFILES.iter().find(|profile| profile.id == id)
}

pub fn figure_png_pixels(size: crate::Size, dpi: u32) -> crate::DltResult<(u32, u32)> {
    Ok((crate::pixels(size.width, dpi)?, crate::pixels(size.height, dpi)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn output_profiles_have_valid_dimensions_colors_and_pixels() {
        assert_eq!(FIGURE_OUTPUT_PROFILES.len(), 5);
        for profile in FIGURE_OUTPUT_PROFILES {
            assert_eq!(figure_output_profile(profile.id), Some(profile));
            assert_eq!(profile.width_emu, i64::from(profile.width_mm) * EMU_PER_MM);
            assert_eq!(profile.height_emu, i64::from(profile.height_mm) * EMU_PER_MM);
            assert!(profile.width_emu > 0 && profile.height_emu > 0 && profile.font_size_emu > 0 && profile.stroke_width_emu > 0);
            assert!((1..=9_600).contains(&profile.default_dpi));
            for color in profile.palette {
                assert!(color.len() == 7 && color.starts_with('#') && color.as_bytes()[1..].iter().all(u8::is_ascii_hexdigit));
            }
            let size = crate::Size { width: profile.width_emu, height: profile.height_emu };
            let expected = |emu: i64| ((i128::from(emu) * i128::from(profile.default_dpi) + i128::from(crate::EMU_PER_INCH / 2)) / i128::from(crate::EMU_PER_INCH)) as u32;
            assert_eq!(figure_png_pixels(size, profile.default_dpi).unwrap(), (expected(size.width), expected(size.height)));
        }
        assert_eq!(figure_output_profile("missing"), None);
        let size = crate::Size { width: crate::EMU_PER_INCH / 2, height: crate::EMU_PER_INCH * 3 / 2 };
        assert_eq!(figure_png_pixels(size, 1).unwrap(), (1, 2));
        let below_half = crate::Size { width: crate::EMU_PER_INCH * 3 / 2 - 1, height: size.height };
        assert_eq!(figure_png_pixels(below_half, 1).unwrap(), (1, 2));
        assert!(figure_png_pixels(size, 0).is_err());
        assert!(figure_png_pixels(size, 9_601).is_err());
        assert!(figure_png_pixels(crate::Size { width: 0, ..size }, 300).is_err());
        assert!(figure_png_pixels(crate::Size { height: -1, ..size }, 300).is_err());
    }
}
