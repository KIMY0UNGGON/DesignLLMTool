//! PDF user space to EMU. One default user unit is `UserUnit / 72` inch, so `1 unit = UserUnit * 12700 EMU`.
//! The contract (`PdfBox`) stores absolute user-space coordinates after this conversion.

use crate::error::{PdfErr, PResult};

/// EMU per 1/72 inch (one PDF point at UserUnit 1).
pub(crate) const EMU_PER_POINT: f64 = 12_700.0;
/// Largest UserUnit accepted. PDF 1.7 caps page dimensions at 14400 units, so larger values cannot describe a page.
pub(crate) const MAX_USER_UNIT: f64 = 14_400.0;

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Units {
    user_unit: f64,
}

impl Units {
    pub(crate) fn new(user_unit: f64) -> PResult<Self> {
        if !user_unit.is_finite() || user_unit <= 0.0 || user_unit > MAX_USER_UNIT {
            return Err(PdfErr::invalid(
                "PDF_BAD_USERUNIT",
                format!("UserUnit must be a positive number up to {MAX_USER_UNIT}, found {user_unit}"),
            ));
        }
        Ok(Self { user_unit })
    }

    /// Nearest integer EMU for a user-space coordinate. Exact halves round away from zero (`f64::round`).
    pub(crate) fn emu(&self, user: f64) -> PResult<i64> {
        let value = (user * self.user_unit) * EMU_PER_POINT;
        if !value.is_finite() || value.abs() > 9.0e15 {
            return Err(PdfErr::invalid("PDF_COORDINATE_RANGE", "PDF coordinate is outside the EMU range"));
        }
        Ok(value.round() as i64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_user_unit_is_one_point_per_unit() {
        let units = Units::new(1.0).unwrap();
        assert_eq!(units.emu(72.0).unwrap(), 914_400);
        assert_eq!(units.emu(18.0).unwrap(), 228_600);
        assert_eq!(units.emu(-1.0).unwrap(), -12_700);
    }

    #[test]
    fn user_unit_scales_every_coordinate() {
        let units = Units::new(2.0).unwrap();
        assert_eq!(units.emu(18.0).unwrap(), 457_200);
        assert_eq!(units.emu(0.5).unwrap(), 12_700);
    }

    #[test]
    fn invalid_user_units_are_rejected() {
        assert!(Units::new(0.0).is_err());
        assert!(Units::new(-1.0).is_err());
        assert!(Units::new(f64::NAN).is_err());
        assert!(Units::new(MAX_USER_UNIT + 1.0).is_err());
    }

    #[test]
    fn exact_half_emu_rounds_away_from_zero() {
        let units = Units::new(1.0).unwrap();
        // 0.005 pt is 63.5 EMU before rounding.
        assert_eq!(units.emu(0.005).unwrap(), 64);
        assert_eq!(units.emu(-0.005).unwrap(), -64);
    }
}
