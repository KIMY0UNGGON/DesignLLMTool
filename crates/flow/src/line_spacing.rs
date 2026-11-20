//! Proportional line spacing (`ParagraphFormat.line_spacing_percent`), shared by the flow model and the HWP and HWPX
//! adapters so that both formats derive the same pitch (`05-decisions/flow-formats.md`, line spacing).
//!
//! - A percentage scales the largest font size of the paragraph, rounded half away from zero. 100 percent is single
//!   spacing, the model's 0, so it is `None` in the model.
//! - With a percentage, `line_spacing` holds the pitch derived from it (for readers that know only the pitch). The
//!   percentage decides: an edit that changes the font sizes derives the pitch again.
//! - A paragraph without runs scales the base character shape that its format adapter reads it with (HWPX derives
//!   its pitches in `prepare`; an HWP paragraph always has a run). Only without such a value does it scale the
//!   default style, the size the layout gives an empty line.
use designllm_contracts::{DltError, DltResult, Paragraph, ParagraphFormat, Style, EMU_PER_HWPUNIT};
use designllm_render::mul_div_round;

/// Range of a requested percentage. A percentage read from a file is kept even outside it.
pub const PERCENT_RANGE: std::ops::RangeInclusive<u32> = 1..=1000;

/// HWPUNIT (1/7200 inch) to EMU.
pub fn hu_to_emu(hu: i64) -> i64 {
    hu * EMU_PER_HWPUNIT
}

/// Line pitch (EMU) of `percent` of `font_size`: 0 for 100 percent (single spacing) and for a percentage of 0 or less.
pub fn percent_line_pitch(font_size: i64, percent: i64) -> i64 {
    if percent == 100 || percent <= 0 {
        return 0;
    }
    if (i128::from(font_size) * i128::from(percent) / 100).abs() >= i128::from(i64::MAX) {
        return i64::MAX;
    }
    mul_div_round(font_size, percent, 100).max(0)
}

/// The model value of a proportional spacing read from a file: `None` for 100 percent, which is single spacing.
pub fn model_percent(percent: i64) -> Option<u32> {
    (percent != 100).then(|| u32::try_from(percent.max(0)).unwrap_or(u32::MAX))
}

/// Equal line spacing: the same percentage, or no percentage and the same pitch.
pub fn same_line_spacing(a: &ParagraphFormat, b: &ParagraphFormat) -> bool {
    match (a.line_spacing_percent, b.line_spacing_percent) {
        (Some(x), Some(y)) => x == y,
        (None, None) => a.line_spacing == b.line_spacing,
        _ => false,
    }
}

/// Font size that the percentage of `p` scales: its largest run, else the default style's size.
pub fn percent_base_size(p: &Paragraph) -> i64 {
    p.runs.iter().map(|r| r.style.font_size).max().unwrap_or_else(|| Style::default().font_size)
}

/// Pitch that the layout uses for `p`: the stored pitch, or the one its percentage gives when none is stored.
pub fn layout_pitch(p: &Paragraph) -> i64 {
    match p.format.line_spacing_percent {
        Some(percent) if p.format.line_spacing == 0 => percent_line_pitch(percent_base_size(p), i64::from(percent)),
        _ => p.format.line_spacing,
    }
}

/// Derives the stored pitch of a proportional paragraph again. A paragraph with runs follows its largest run; one
/// without runs keeps a stored pitch (its format adapter derived it from the paragraph's base character shape) and
/// gets one from the default style only when it has none.
pub fn refresh_pitch(p: &mut Paragraph) {
    let Some(percent) = p.format.line_spacing_percent else { return };
    if p.runs.is_empty() && p.format.line_spacing != 0 {
        return;
    }
    p.format.line_spacing = percent_line_pitch(percent_base_size(p), i64::from(percent));
}

/// An edit request's percentage must lie in [`PERCENT_RANGE`] (INVALID_ARGUMENT otherwise).
pub fn validate_percent(format: &ParagraphFormat) -> DltResult<()> {
    match format.line_spacing_percent {
        Some(percent) if !PERCENT_RANGE.contains(&percent) => Err(DltError::invalid(format!(
            "Paragraph line_spacing_percent must be {}..={}, got {percent}",
            PERCENT_RANGE.start(),
            PERCENT_RANGE.end()
        ))),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use designllm_contracts::EMU_PER_PT;

    #[test]
    fn a_percentage_scales_the_font_size_with_half_away_from_zero_rounding() {
        assert_eq!(percent_line_pitch(10 * EMU_PER_PT, 160), 16 * EMU_PER_PT);
        assert_eq!(percent_line_pitch(10 * EMU_PER_PT, 100), 0, "100 percent is single spacing");
        assert_eq!(percent_line_pitch(10 * EMU_PER_PT, 0), 0);
        assert_eq!(percent_line_pitch(10 * EMU_PER_PT, -50), 0, "a negative percentage from a file is single spacing");
        assert_eq!(percent_line_pitch(3, 150), 5, "4.5 rounds away from zero");
        assert_eq!(percent_line_pitch(i64::MAX / 2, 1000), i64::MAX, "a huge pitch saturates");
    }

    #[test]
    fn model_percent_drops_100_and_keeps_values_read_from_files() {
        assert_eq!(model_percent(100), None);
        assert_eq!(model_percent(160), Some(160));
        assert_eq!(model_percent(5000), Some(5000), "out of the edit range, but read values are kept");
        assert_eq!(model_percent(-3), Some(0));
        assert_eq!(model_percent(i64::MAX), Some(u32::MAX));
    }
}
