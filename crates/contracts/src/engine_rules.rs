//! Engine rule constants that the canvas engine and the desktop both use. Plain constants; nothing here is serialized.

/// Touch target minimum in CSS px (WCAG 2.5.5 and the platform guidelines). An integer, so the lint parameter `min`
/// stays `44` in the diagnostic JSON.
pub const TOUCH_MIN_PX: i64 = 44;
/// Text contrast minimum (WCAG AA for normal text).
pub const CONTRAST_MIN: f64 = 4.5;
/// Largest grid column count. The canvas model refuses more columns.
pub const MAX_GRID_COLUMNS: u32 = 24;
/// Most breakpoints a layout takes. The canvas model refuses more.
pub const MAX_BREAKPOINTS: usize = 16;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::EMU_PER_CSS_PX;

    /// The values the canvas engine and the desktop each defined before these constants moved here.
    #[test]
    fn values_match_the_former_engine_definitions() {
        assert_eq!(EMU_PER_CSS_PX, 9_525);
        assert_eq!(TOUCH_MIN_PX, 44);
        assert_eq!(CONTRAST_MIN, 4.5);
        assert_eq!(MAX_GRID_COLUMNS, 24);
        assert_eq!(MAX_BREAKPOINTS, 16);
    }
}
