//! Native WordprocessingML units. Model values are EMU; the file stores twips (1/20 pt) for
//! page and paragraph geometry and half-points for font size. Writing a value rounds it to the
//! native grid with the contracts' `quantize` (nearest, ties away from zero). The applied value
//! is what the model keeps, so a reopened file matches the model exactly.

use designllm_contracts::{quantize, DltError, DltResult, EMU_PER_TWIP};

/// EMU per half-point (`w:sz` unit): 12700 / 2.
pub const EMU_PER_HALF_POINT: i64 = 6_350;

/// Rounds an EMU length to whole twips. Returns `(twips, applied_emu)`.
pub fn to_twips(emu: i64) -> DltResult<(i64, i64)> {
    let applied = quantize(emu, EMU_PER_TWIP)?;
    Ok((applied / EMU_PER_TWIP, applied))
}

/// Rounds an EMU length to whole half-points. Returns `(half_points, applied_emu)`.
pub fn to_half_points(emu: i64) -> DltResult<(i64, i64)> {
    let applied = quantize(emu, EMU_PER_HALF_POINT)?;
    Ok((applied / EMU_PER_HALF_POINT, applied))
}

/// EMU value of a stored twip count.
pub fn twips_to_emu(twips: i64) -> DltResult<i64> {
    twips.checked_mul(EMU_PER_TWIP).ok_or_else(|| DltError::invalid("twip value overflows EMU"))
}

/// EMU value of a stored half-point count.
pub fn half_points_to_emu(half_points: i64) -> DltResult<i64> {
    half_points.checked_mul(EMU_PER_HALF_POINT).ok_or_else(|| DltError::invalid("half-point value overflows EMU"))
}

/// Parses a decimal integer attribute. Rejects empty, signed-overflow, and non-numeric values.
pub fn parse_int(value: &str) -> Option<i64> {
    value.trim().parse::<i64>().ok()
}

/// Lowercase six-digit hex color for model storage (`#RRGGBB`), or `None` for an unusable value.
pub fn color_model(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.eq_ignore_ascii_case("auto") {
        return Some("#000000".to_string());
    }
    if trimmed.len() == 6 && trimmed.chars().all(|c| c.is_ascii_hexdigit()) {
        return Some(format!("#{}", trimmed.to_ascii_uppercase()));
    }
    None
}

/// `w:val` form of a model color (`RRGGBB`), accepting `#RRGGBB` only.
pub fn color_file(model: &str) -> DltResult<String> {
    let hex = model.strip_prefix('#').unwrap_or(model);
    if hex.len() == 6 && hex.chars().all(|c| c.is_ascii_hexdigit()) {
        Ok(hex.to_ascii_uppercase())
    } else {
        Err(DltError::invalid(format!("color must be #RRGGBB, got {model:?}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use designllm_contracts::EMU_PER_MM;

    #[test]
    fn twip_rounding_is_nearest_not_truncation() {
        // 20 mm = 720000 EMU -> 1134 twips = 720090 EMU (the contract example).
        assert_eq!(to_twips(20 * EMU_PER_MM).unwrap(), (1134, 720_090));
        assert_eq!(to_twips(-20 * EMU_PER_MM).unwrap(), (-1134, -720_090));
        assert_eq!(to_twips(635).unwrap(), (1, 635));
        assert_eq!(to_twips(317).unwrap(), (0, 0));
        assert_eq!(to_twips(318).unwrap(), (1, 635));
    }

    #[test]
    fn half_point_rounding_matches_font_size_grid() {
        assert_eq!(to_half_points(12 * 12_700).unwrap(), (24, 24 * 6_350));
        assert_eq!(to_half_points(12_700 * 10 + 1).unwrap(), (20, 20 * 6_350));
    }

    #[test]
    fn color_model_and_file_round_trip() {
        assert_eq!(color_model("17365d").as_deref(), Some("#17365D"));
        assert_eq!(color_model("auto").as_deref(), Some("#000000"));
        assert_eq!(color_model("xyz"), None);
        assert_eq!(color_file("#17365D").unwrap(), "17365D");
        assert!(color_file("#17365").is_err());
    }
}
