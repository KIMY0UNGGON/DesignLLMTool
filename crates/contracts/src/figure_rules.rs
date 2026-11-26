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
/// Okabe-Ito, colour-blind safe (the paper-figure guide names the same colours); the yellow is left out as too light for lines.
const DEFAULT_PALETTE: [&str; 6] = ["#0072B2", "#D55E00", "#009E73", "#E69F00", "#CC79A7", "#56B4E9"];
/// Near-black first, then Okabe-Ito, then a neutral grey: no stock framework colours (contracts `design_quality`).
const PRINT_PALETTE: [&str; 6] = ["#1C1C1C", "#0072B2", "#D55E00", "#009E73", "#CC79A7", "#6B6B6B"];
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

/// A Figure plot's series colours before the output profiles: the first is the panel's fill, the others these. Kept as they
/// were: a figure's visual review approves its stored content, so a new default would repaint figures already approved. New
/// palettes arrive through an output profile, which is a recorded change.
pub const LEGACY_SERIES_PALETTE: [&str; 6] = ["#1F4E79", "#C0504D", "#4F9B3C", "#7F6000", "#7030A0", "#2E75B6"];

/// The colours of a plot's first `count` series, as the Figure renderer draws them and the window predicts them. The first
/// is the panel's fill. When that fill is a colour of an output profile's palette (the first palette that holds it), the
/// others continue in that palette's order after it; otherwise they come from `LEGACY_SERIES_PALETTE`, except that a colour
/// equal to the fill is skipped, so no two series of one plot share a colour within a palette's length.
pub fn series_colors(fill: &str, count: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(count);
    if count == 0 {
        return out;
    }
    out.push(fill.to_owned());
    let profile = FIGURE_OUTPUT_PROFILES
        .iter()
        .find_map(|profile| profile.palette.iter().position(|colour| same_colour(colour, fill)).map(|index| (&profile.palette[..], index + 1)));
    let (palette, start): (&[&str], usize) = profile.unwrap_or((&LEGACY_SERIES_PALETTE[..], 1));
    let mut k = start;
    while out.len() < count {
        let colour = palette[k % palette.len()];
        k += 1;
        // A palette holds the fill at most once, so skipping it never loops; past a full turn, repeats are unavoidable.
        if same_colour(colour, fill) && k - start <= palette.len() {
            continue;
        }
        out.push(colour.to_owned());
    }
    out
}

/// A colour as the renderer reads it (designllm-render `paint::parse_color`): `#RGB`, `#RRGGBB`, `#RRGGBBAA`, its seven colour
/// names, and an empty value, `none` or `transparent` for no paint (`Some(None)`). `None` when it reads none of these.
fn painted(color: &str) -> Option<Option<(u8, u8, u8, u8)>> {
    let v = color.trim();
    if v.is_empty() || v.eq_ignore_ascii_case("none") || v.eq_ignore_ascii_case("transparent") {
        return Some(None);
    }
    if let Some(hex) = v.strip_prefix('#') {
        if hex.len() == 3 {
            let digits: Option<Vec<u8>> = hex.chars().map(|c| c.to_digit(16).map(|d| (d * 17) as u8)).collect();
            return digits.map(|d| Some((d[0], d[1], d[2], 255)));
        }
        return crate::design_quality::rgba(v).map(Some);
    }
    let named = match v.to_ascii_lowercase().as_str() {
        "black" => (0, 0, 0),
        "white" => (255, 255, 255),
        "red" => (255, 0, 0),
        "green" => (0, 128, 0),
        "blue" => (0, 0, 255),
        "gray" | "grey" => (128, 128, 128),
        "yellow" => (255, 255, 0),
        _ => return None,
    };
    Some(Some((named.0, named.1, named.2, 255)))
}

/// Whether two colours paint the same, read as the renderer reads them: `#F00`, `#ff0000`, `#FF0000FF` and `red` agree.
/// Text the renderer cannot read is compared as text.
pub fn same_colour(a: &str, b: &str) -> bool {
    match (painted(a), painted(b)) {
        (Some(x), Some(y)) => x == y,
        _ => a.trim().eq_ignore_ascii_case(b.trim()),
    }
}

/// The series colours before `series_colors`: the fill, then `LEGACY_SERIES_PALETTE[j]` whatever the fill. The Figure approval
/// digest compares the two, so a figure whose colours did not change keeps its approval.
pub fn legacy_series_colors(fill: &str, count: usize) -> Vec<String> {
    (0..count)
        .map(|j| if j == 0 { fill.to_owned() } else { LEGACY_SERIES_PALETTE[j % LEGACY_SERIES_PALETTE.len()].to_owned() })
        .collect()
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

#[cfg(test)]
mod series_colour_tests {
    use super::*;

    #[test]
    fn series_after_an_okabe_ito_fill_continue_in_okabe_ito_and_never_repeat_the_fill() {
        // A profile gives the second panel #D55E00: its second series must not be #D55E00 again.
        assert_eq!(series_colors("#D55E00", 3), ["#D55E00", "#009E73", "#E69F00"]);
        assert_eq!(series_colors("#56B4E9", 3), ["#56B4E9", "#0072B2", "#D55E00"]);
        // The print and presentation profiles start at near-black and continue in their own palette.
        assert_eq!(series_colors("#1C1C1C", 3), ["#1C1C1C", "#0072B2", "#D55E00"]);
        for profile in FIGURE_OUTPUT_PROFILES {
            for fill in profile.palette {
                let colours = series_colors(fill, 6);
                let distinct: std::collections::BTreeSet<&String> = colours.iter().collect();
                assert_eq!(distinct.len(), 6, "{}: {fill}: {colours:?}", profile.id);
            }
        }
    }

    #[test]
    fn other_fills_keep_the_earlier_colours_except_where_a_series_matched_the_fill() {
        assert_eq!(series_colors("#1F4E79", 6), legacy_series_colors("#1F4E79", 6), "the default fill keeps its look");
        assert_eq!(series_colors("#123456", 4), legacy_series_colors("#123456", 4));
        // The old rule drew a #C0504D-filled plot's second series in #C0504D too.
        assert_eq!(legacy_series_colors("#C0504D", 2), ["#C0504D", "#C0504D"]);
        assert_eq!(series_colors("#C0504D", 2), ["#C0504D", "#4F9B3C"]);
        // Colours are compared decoded: an opaque alpha or lower case is the same colour.
        assert_eq!(series_colors("#C0504DFF", 2), ["#C0504DFF", "#4F9B3C"]);
        assert_eq!(series_colors("#d55e00", 2), ["#d55e00", "#009E73"]);
        assert!(same_colour("#1F4E79", "#1f4e79ff") && !same_colour("#1F4E79", "#1F4E7980"));
        assert!(same_colour("#F00", "#FF0000") && same_colour("red", "#ff0000ff") && same_colour("none", "transparent") && !same_colour("#F00", "#F000"));
        // Seven series wrapped onto the fill before.
        assert_ne!(series_colors("#1F4E79", 7)[6], "#1F4E79");
        assert_eq!(series_colors("#000000", 0), Vec::<String>::new());
    }
}
