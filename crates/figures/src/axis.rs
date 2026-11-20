//! Axis resolution: explicit or automatic ranges, tick steps and tick labels.
//! Automatic ranges come only from plotted values and their error bounds, so the same data always
//! yields the same range. The resolved range is recorded in scene metadata. Nothing is clipped.

use crate::decimal::{Exact, ceil_to_multiple, floor_to_multiple, nice_at_least, round_div};
use crate::diag::code;

pub const MAX_TICKS: usize = 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scale {
    Linear,
    Log10,
}

impl Scale {
    pub fn parse(text: &str) -> Option<Scale> {
        match text {
            "linear" => Some(Scale::Linear),
            "log10" => Some(Scale::Log10),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Scale::Linear => "linear",
            Scale::Log10 => "log10",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedAxis {
    pub scale: Scale,
    pub min: Exact,
    pub max: Exact,
    pub tick_step: Option<Exact>,
    pub ticks: Vec<Exact>,
    pub tick_labels: Vec<String>,
    pub explicit: bool,
}

/// (diagnostic code, message) for an axis that cannot be resolved.
pub type AxisError = (&'static str, String);

fn fail(code: &'static str, message: impl Into<String>) -> AxisError {
    (code, message.into())
}

/// Resolves one axis.
/// `explicit` is (minimum, maximum) when both are given; `None` means automatic.
/// `data` is the (lowest, highest) plotted value including error bounds. `include_zero` is set for bars.
pub fn resolve(
    scale: Scale,
    explicit: Option<(Exact, Exact)>,
    tick_step: Option<Exact>,
    data: (Exact, Exact),
    include_zero: bool,
) -> Result<ResolvedAxis, AxisError> {
    match scale {
        Scale::Linear => resolve_linear(explicit, tick_step, data, include_zero),
        Scale::Log10 => resolve_log(explicit, tick_step, data),
    }
}

fn resolve_linear(
    explicit: Option<(Exact, Exact)>,
    tick_step: Option<Exact>,
    data: (Exact, Exact),
    include_zero: bool,
) -> Result<ResolvedAxis, AxisError> {
    let numeric = |o: Option<Exact>| o.ok_or_else(|| fail(code::NUMERIC_RANGE, "axis arithmetic left the supported range"));
    if let Some(step) = tick_step {
        if !step.is_positive() {
            return Err(fail(code::TICK_STEP_INVALID, "tick step must be positive"));
        }
    }
    let (min, max, is_explicit) = match explicit {
        Some((mn, mx)) => {
            if mx <= mn {
                return Err(fail(code::AXIS_RANGE_INVALID, "axis maximum must be greater than minimum"));
            }
            (mn, mx, true)
        }
        None => {
            let (mn, mx) = auto_linear(data.0, data.1, include_zero)
                .ok_or_else(|| fail(code::NUMERIC_RANGE, "automatic axis range left the supported range"))?;
            (mn, mx, false)
        }
    };
    let span = numeric(max.sub(min))?;
    let step = match tick_step {
        Some(s) => s,
        None => {
            // span / 5 = span * 2 / 10, exact.
            let fifth = Exact {
                v: span.v.checked_mul(2).ok_or_else(|| fail(code::NUMERIC_RANGE, "tick step left the supported range"))?,
                s: span.s + 1,
            };
            nice_at_least(fifth).ok_or_else(|| fail(code::NUMERIC_RANGE, "tick step left the supported range"))?
        }
    };
    let mut ticks = Vec::new();
    let mut t = min;
    loop {
        if t > max {
            break;
        }
        ticks.push(t);
        if ticks.len() > MAX_TICKS {
            return Err(fail(code::TICK_LIMIT, format!("more than {MAX_TICKS} ticks; choose a larger tick step")));
        }
        t = numeric(t.add(step))?;
    }
    let decimals = if is_explicit {
        let mut d = min.s.max(max.s);
        if tick_step.is_some() {
            d = d.max(step.s);
        }
        d
    } else {
        min.normalized().s.max(max.normalized().s).max(step.normalized().s)
    };
    let mut tick_labels = Vec::with_capacity(ticks.len());
    for t in &ticks {
        tick_labels.push(t.fixed(decimals).ok_or_else(|| fail(code::NUMERIC_RANGE, "tick label left the supported range"))?);
    }
    Ok(ResolvedAxis { scale: Scale::Linear, min, max, tick_step: Some(step), ticks, tick_labels, explicit: is_explicit })
}

fn auto_linear(lo: Exact, hi: Exact, include_zero: bool) -> Option<(Exact, Exact)> {
    let (mut lo, mut hi) = (lo, hi);
    if include_zero {
        if lo.is_positive() {
            lo = Exact::ZERO;
        }
        if hi.is_negative() {
            hi = Exact::ZERO;
        }
    }
    if lo == hi {
        if hi.is_zero() {
            hi = Exact::int(1);
        } else {
            // half = |hi| / 2 = |hi| * 5 / 10, exact in decimal.
            let a = hi.abs();
            let half = Exact { v: a.v.checked_mul(5)?, s: a.s + 1 };
            lo = lo.sub(half)?;
            hi = hi.add(half)?;
        }
    }
    let span = hi.sub(lo)?;
    // 10% headroom on each side: span * 0.1 is exact by moving the decimal point.
    let pad = Exact { v: span.v, s: span.s + 1 };
    let mut lo2 = lo.sub(pad)?;
    let hi2 = hi.add(pad)?;
    if !lo.is_negative() && lo2.is_negative() {
        lo2 = Exact::ZERO;
    }
    let span2 = hi2.sub(lo2)?;
    // span / 5 = span * 2 / 10, exact.
    let target = Exact { v: span2.v.checked_mul(2)?, s: span2.s + 1 };
    let step = nice_at_least(target)?;
    let min = floor_to_multiple(lo2, step)?;
    let mut max = ceil_to_multiple(hi2, step)?;
    if max <= min {
        max = min.add(step)?;
    }
    Some((min, max))
}

fn resolve_log(explicit: Option<(Exact, Exact)>, tick_step: Option<Exact>, data: (Exact, Exact)) -> Result<ResolvedAxis, AxisError> {
    if tick_step.is_some() {
        return Err(fail(code::TICK_STEP_NOT_APPLICABLE, "a log axis uses decades; tick_step must be omitted"));
    }
    let (min, max, is_explicit) = match explicit {
        Some((mn, mx)) => {
            if !mn.is_positive() {
                return Err(fail(code::LOG_AXIS_MIN_NOT_POSITIVE, "log axis minimum must be greater than 0"));
            }
            if mx <= mn {
                return Err(fail(code::AXIS_RANGE_INVALID, "axis maximum must be greater than minimum"));
            }
            (mn, mx, true)
        }
        None => {
            let (lo, hi) = data;
            if !lo.is_positive() {
                return Err(fail(code::LOG_DOMAIN_VALUE, "log axis needs plotted values greater than 0"));
            }
            let k_min = lo.decade_floor().ok_or_else(|| fail(code::NUMERIC_RANGE, "log decade out of range"))?;
            let k_max = hi.decade_floor().ok_or_else(|| fail(code::NUMERIC_RANGE, "log decade out of range"))?;
            let k_top = if hi.is_power_of_ten() { k_max } else { k_max + 1 };
            let mn = Exact::pow10(k_min).ok_or_else(|| fail(code::NUMERIC_RANGE, "log decade out of range"))?;
            let mut mx = Exact::pow10(k_top).ok_or_else(|| fail(code::NUMERIC_RANGE, "log decade out of range"))?;
            if mx <= mn {
                mx = mn.mul(Exact::int(10)).ok_or_else(|| fail(code::NUMERIC_RANGE, "log decade out of range"))?;
            }
            (mn, mx, false)
        }
    };
    let k_lo = min.decade_floor().ok_or_else(|| fail(code::NUMERIC_RANGE, "log decade out of range"))?;
    let k_hi = max.decade_floor().ok_or_else(|| fail(code::NUMERIC_RANGE, "log decade out of range"))? + 1;
    let mut ticks = Vec::new();
    for k in k_lo..=k_hi {
        let t = Exact::pow10(k).ok_or_else(|| fail(code::NUMERIC_RANGE, "log decade out of range"))?;
        if t >= min && t <= max {
            ticks.push(t);
            if ticks.len() > MAX_TICKS {
                return Err(fail(code::TICK_LIMIT, format!("more than {MAX_TICKS} decades")));
            }
        }
    }
    let tick_labels = ticks.iter().map(|t| t.canonical()).collect();
    Ok(ResolvedAxis { scale: Scale::Log10, min, max, tick_step: None, ticks, tick_labels, explicit: is_explicit })
}

/// Distance in EMU from the axis minimum to `v`, for an axis drawn `len` EMU long.
/// Linear placement is exact integer arithmetic. Log placement uses log10 and rounds to the nearest EMU.
pub fn distance(axis: &ResolvedAxis, v: Exact, len: i64) -> Option<i64> {
    match axis.scale {
        Scale::Linear => {
            let s = v.s.max(axis.min.s).max(axis.max.s);
            let vv = v.at_scale(s)?;
            let mn = axis.min.at_scale(s)?;
            let mx = axis.max.at_scale(s)?;
            let num = vv.checked_sub(mn)?.checked_mul(i128::from(len))?;
            let den = mx.checked_sub(mn)?;
            if den <= 0 {
                return None;
            }
            i64::try_from(round_div(num, den)?).ok()
        }
        Scale::Log10 => {
            if !v.is_positive() {
                return None;
            }
            let f = (v.to_f64().log10() - axis.min.to_f64().log10()) / (axis.max.to_f64().log10() - axis.min.to_f64().log10());
            if !f.is_finite() {
                return None;
            }
            Some((f * len as f64).round() as i64)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decimal::Dec;

    fn e(text: &str) -> Exact {
        Dec::parse(text).unwrap().exact()
    }

    #[test]
    fn automatic_linear_range_is_deterministic_and_covers_errors() {
        // Means 0.10..0.30 with SD 0.01..0.03: data span 0.09..0.33.
        let axis = resolve(Scale::Linear, None, None, (e("0.09"), e("0.33")), false).unwrap();
        assert_eq!(axis.min.canonical(), "0");
        assert_eq!(axis.max.canonical(), "0.4");
        assert_eq!(axis.tick_labels, vec!["0.0", "0.1", "0.2", "0.3", "0.4"]);
        assert!(!axis.explicit);
        let again = resolve(Scale::Linear, None, None, (e("0.09"), e("0.33")), false).unwrap();
        assert_eq!(axis, again);
    }

    #[test]
    fn bars_include_zero_and_explicit_ranges_are_kept() {
        let bar = resolve(Scale::Linear, None, None, (e("0.10"), e("0.20")), true).unwrap();
        assert_eq!(bar.min.canonical(), "0");
        let fixed = resolve(Scale::Linear, Some((e("0.00"), e("0.40"))), Some(e("0.10")), (e("0.1"), e("0.2")), false).unwrap();
        assert_eq!(fixed.tick_labels, vec!["0.00", "0.10", "0.20", "0.30", "0.40"]);
        assert!(fixed.explicit);
    }

    #[test]
    fn invalid_ranges_are_named_not_repaired() {
        let err = resolve(Scale::Linear, Some((e("0.4"), e("0.0"))), None, (e("0"), e("1")), false).unwrap_err();
        assert_eq!(err.0, code::AXIS_RANGE_INVALID);
        let err = resolve(Scale::Log10, Some((e("0.0"), e("1"))), None, (e("1"), e("1")), false).unwrap_err();
        assert_eq!(err.0, code::LOG_AXIS_MIN_NOT_POSITIVE);
        let err = resolve(Scale::Linear, None, Some(e("0")), (e("0"), e("1")), false).unwrap_err();
        assert_eq!(err.0, code::TICK_STEP_INVALID);
    }

    #[test]
    fn log_axis_uses_decades_and_refuses_nonpositive_data() {
        let axis = resolve(Scale::Log10, None, None, (e("0.09"), e("0.33")), false).unwrap();
        assert_eq!(axis.min.canonical(), "0.01");
        assert_eq!(axis.max.canonical(), "1");
        assert_eq!(axis.tick_labels, vec!["0.01", "0.1", "1"]);
        let err = resolve(Scale::Log10, None, None, (e("0"), e("1")), false).unwrap_err();
        assert_eq!(err.0, code::LOG_DOMAIN_VALUE);
    }

    #[test]
    fn distance_is_exact_for_linear_axes() {
        let axis = resolve(Scale::Linear, Some((e("0"), e("0.4"))), Some(e("0.1")), (e("0"), e("0.4")), false).unwrap();
        assert_eq!(distance(&axis, e("0.1"), 4_000_000), Some(1_000_000));
        assert_eq!(distance(&axis, e("0.4"), 4_000_000), Some(4_000_000));
        assert_eq!(distance(&axis, e("0.3"), 4_000_000), Some(3_000_000));
    }
}
