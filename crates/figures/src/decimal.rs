//! Exact decimal text and arithmetic for figure data.
//! Source text is never rewritten: `Dec` keeps the parsed digits and `Exact` is the value used for
//! ranges, ticks and comparisons. No binary floating point is used except log10 placement in layout.

use std::cmp::Ordering;

use designllm_contracts::figure_rules::DECIMAL_MAX_DIGITS;

/// Longest digit string accepted (integer + fraction digits), the contract bound. Keeps i128 products exact.
pub const MAX_DIGITS: usize = DECIMAL_MAX_DIGITS;
/// Largest decimal scale produced by arithmetic before the operation is refused.
pub const MAX_SCALE: u32 = 30;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseClass {
    /// Empty or blank cell. Never converted to 0.
    Missing,
    /// Readable two ways (thousands separator or decimal comma). Never interpreted.
    Ambiguous,
    /// Outside the accepted grammar.
    Malformed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub class: ParseClass,
    pub reason: String,
}

fn perr(class: ParseClass, reason: impl Into<String>) -> ParseError {
    ParseError { class, reason: reason.into() }
}

/// A parsed decimal. Value = (-1)^neg * coef / 10^scale, where `scale` is the number of fraction digits written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Dec {
    pub neg: bool,
    pub coef: u128,
    pub scale: u32,
}

impl Dec {
    /// Grammar: optional sign, ASCII digits with at most one '.', at least one digit, at most `MAX_DIGITS` digits.
    /// Rejected: exponent notation, whitespace inside the number, non-ASCII digits, NaN and Infinity.
    /// Thousands separators and decimal commas are Ambiguous, not interpreted.
    pub fn parse(raw: &str) -> Result<Dec, ParseError> {
        if raw.trim().is_empty() {
            return Err(perr(ParseClass::Missing, "empty cell"));
        }
        if let Some(c) = raw.chars().find(|c| !c.is_ascii()) {
            return Err(perr(ParseClass::Malformed, format!("non-ASCII character U+{:04X}", u32::from(c))));
        }
        if raw.bytes().any(|b| b.is_ascii_whitespace()) {
            return Err(perr(ParseClass::Malformed, "whitespace is not part of a number"));
        }
        let (neg, body) = match raw.as_bytes()[0] {
            b'-' => (true, &raw[1..]),
            b'+' => (false, &raw[1..]),
            _ => (false, raw),
        };
        if body.is_empty() {
            return Err(perr(ParseClass::Malformed, "sign without digits"));
        }
        if body.contains(',') {
            let numeric_like = body.bytes().all(|b| b.is_ascii_digit() || b == b'.' || b == b',');
            return Err(if numeric_like {
                perr(ParseClass::Ambiguous, "',' may be a thousands separator or a decimal comma")
            } else {
                perr(ParseClass::Malformed, "',' inside non-numeric text")
            });
        }
        if body.bytes().any(|b| b == b'e' || b == b'E') {
            return Err(perr(ParseClass::Malformed, "exponent notation is not supported"));
        }
        if body.bytes().filter(|&b| b == b'.').count() > 1 {
            return Err(perr(ParseClass::Malformed, "more than one decimal point"));
        }
        let (int_part, frac_part) = body.split_once('.').unwrap_or((body, ""));
        if !int_part.bytes().all(|b| b.is_ascii_digit()) || !frac_part.bytes().all(|b| b.is_ascii_digit()) {
            return Err(perr(ParseClass::Malformed, "only ASCII digits, one '.', and a leading sign are accepted"));
        }
        if int_part.is_empty() && frac_part.is_empty() {
            return Err(perr(ParseClass::Malformed, "no digits"));
        }
        let digits = int_part.len() + frac_part.len();
        if digits > MAX_DIGITS {
            return Err(perr(ParseClass::Malformed, format!("more than {MAX_DIGITS} digits is outside the supported range")));
        }
        let coef = format!("{int_part}{frac_part}")
            .parse::<u128>()
            .map_err(|_| perr(ParseClass::Malformed, "digit overflow"))?;
        Ok(Dec { neg, coef, scale: frac_part.len() as u32 })
    }

    pub fn exact(self) -> Exact {
        let v = i128::try_from(self.coef).unwrap_or(i128::MAX);
        Exact { v: if self.neg { -v } else { v }, s: self.scale }
    }

    /// Canonical text used for computation and protocol values: "0.10" -> "0.1", ".20" -> "0.2".
    pub fn canonical(self) -> String {
        self.exact().canonical()
    }
}

/// Exact value = v / 10^s.
#[derive(Debug, Clone, Copy)]
pub struct Exact {
    pub v: i128,
    pub s: u32,
}

impl PartialEq for Exact {
    fn eq(&self, other: &Exact) -> bool {
        self.cmp_exact(*other) == Some(Ordering::Equal)
    }
}

impl PartialOrd for Exact {
    fn partial_cmp(&self, other: &Exact) -> Option<Ordering> {
        self.cmp_exact(*other)
    }
}

impl Exact {
    pub const ZERO: Exact = Exact { v: 0, s: 0 };

    pub fn int(n: i128) -> Exact {
        Exact { v: n, s: 0 }
    }

    /// 10^e as an exact value.
    pub fn pow10(e: i32) -> Option<Exact> {
        if e >= 0 {
            Some(Exact { v: 10i128.checked_pow(e as u32)?, s: 0 })
        } else {
            Some(Exact { v: 1, s: e.unsigned_abs() })
        }
    }

    /// The raw integer at scale `s`, which must be at least `self.s`.
    pub fn at_scale(self, s: u32) -> Option<i128> {
        let k = s.checked_sub(self.s)?;
        self.v.checked_mul(10i128.checked_pow(k)?)
    }

    fn align(self, o: Exact) -> Option<(i128, i128, u32)> {
        let s = self.s.max(o.s);
        Some((self.at_scale(s)?, o.at_scale(s)?, s))
    }

    fn limited(self) -> Option<Exact> {
        if self.s <= MAX_SCALE { Some(self) } else { None }
    }

    pub fn add(self, o: Exact) -> Option<Exact> {
        let (a, b, s) = self.align(o)?;
        Exact { v: a.checked_add(b)?, s }.limited()
    }

    pub fn sub(self, o: Exact) -> Option<Exact> {
        let (a, b, s) = self.align(o)?;
        Exact { v: a.checked_sub(b)?, s }.limited()
    }

    pub fn mul(self, o: Exact) -> Option<Exact> {
        Exact { v: self.v.checked_mul(o.v)?, s: self.s.checked_add(o.s)? }.limited()
    }

    pub fn neg(self) -> Exact {
        Exact { v: -self.v, s: self.s }
    }

    pub fn abs(self) -> Exact {
        Exact { v: self.v.abs(), s: self.s }
    }

    pub fn cmp_exact(self, o: Exact) -> Option<Ordering> {
        let (a, b, _) = self.align(o)?;
        Some(a.cmp(&b))
    }

    pub fn is_zero(self) -> bool {
        self.v == 0
    }

    pub fn is_negative(self) -> bool {
        self.v < 0
    }

    pub fn is_positive(self) -> bool {
        self.v > 0
    }

    /// Same value with trailing fraction zeros removed.
    pub fn normalized(self) -> Exact {
        let (mut v, mut s) = (self.v, self.s);
        while s > 0 && v % 10 == 0 {
            v /= 10;
            s -= 1;
        }
        if v == 0 {
            s = 0;
        }
        Exact { v, s }
    }

    /// Canonical decimal text without trailing zeros: "0.10" -> "0.1", "-0.00" -> "0".
    pub fn canonical(self) -> String {
        let n = self.normalized();
        let mag = n.v.unsigned_abs();
        let sign = if n.v < 0 { "-" } else { "" };
        if n.s == 0 {
            return format!("{sign}{mag}");
        }
        let pow = 10u128.pow(n.s);
        format!("{sign}{}.{:0w$}", mag / pow, mag % pow, w = n.s as usize)
    }

    /// Value rounded half away from zero to `decimals` fraction digits, as fixed-point text.
    pub fn fixed(self, decimals: u32) -> Option<String> {
        let r = self.round_to(decimals)?;
        let mag = r.v.unsigned_abs();
        let sign = if r.v < 0 { "-" } else { "" };
        if decimals == 0 {
            return Some(format!("{sign}{mag}"));
        }
        let pow = 10u128.checked_pow(decimals)?;
        Some(format!("{sign}{}.{:0w$}", mag / pow, mag % pow, w = decimals as usize))
    }

    /// Rounds half away from zero to `decimals` fraction digits. Exact when `decimals >= self.s`.
    pub fn round_to(self, decimals: u32) -> Option<Exact> {
        if decimals >= self.s {
            return Some(Exact { v: self.at_scale(decimals)?, s: decimals });
        }
        let div = 10i128.checked_pow(self.s - decimals)?;
        Some(Exact { v: round_div(self.v, div)?, s: decimals })
    }

    /// floor(log10(value)) for a positive value, computed from the digit count (exact).
    pub fn decade_floor(self) -> Option<i32> {
        if self.v <= 0 {
            return None;
        }
        let digits = self.v.to_string().len() as i32;
        Some(digits - 1 - self.s as i32)
    }

    /// True when a positive value equals 10^k for some integer k.
    pub fn is_power_of_ten(self) -> bool {
        if self.v <= 0 {
            return false;
        }
        let t = self.v.to_string();
        t.starts_with('1') && t[1..].bytes().all(|b| b == b'0')
    }

    /// Lossy conversion for logarithmic placement only. Never written back to data.
    pub fn to_f64(self) -> f64 {
        self.v as f64 / 10f64.powi(self.s as i32)
    }
}

/// Integer division rounding half away from zero. `den` must be positive.
pub fn round_div(num: i128, den: i128) -> Option<i128> {
    if den <= 0 {
        return None;
    }
    let mag = num.unsigned_abs();
    let d = den as u128;
    let q = mag / d;
    let r = mag % d;
    let q = if r >= d - r { q + 1 } else { q };
    let q = i128::try_from(q).ok()?;
    Some(if num < 0 { -q } else { q })
}

/// Largest multiple of `step` that is <= x. `step` must be positive.
pub fn floor_to_multiple(x: Exact, step: Exact) -> Option<Exact> {
    if !step.is_positive() {
        return None;
    }
    let s = x.s.max(step.s);
    let xa = x.at_scale(s)?;
    let sa = step.at_scale(s)?;
    let k = xa.div_euclid(sa);
    Exact { v: k.checked_mul(sa)?, s }.limited()
}

/// Smallest multiple of `step` that is >= x. `step` must be positive.
pub fn ceil_to_multiple(x: Exact, step: Exact) -> Option<Exact> {
    if !step.is_positive() {
        return None;
    }
    let s = x.s.max(step.s);
    let xa = x.at_scale(s)?;
    let sa = step.at_scale(s)?;
    let k = -((-xa).div_euclid(sa));
    Exact { v: k.checked_mul(sa)?, s }.limited()
}

/// Smallest value of the form {1, 2, 2.5, 5} x 10^k that is >= x. `x` must be positive.
pub fn nice_at_least(x: Exact) -> Option<Exact> {
    if !x.is_positive() {
        return None;
    }
    let k = x.decade_floor()?;
    let mut best: Option<Exact> = None;
    for e in [k - 1, k, k + 1] {
        for (m, ms) in [(1i128, 0u32), (2, 0), (25, 1), (5, 0)] {
            let cand = Exact { v: m, s: ms }.mul(Exact::pow10(e)?)?.normalized();
            if cand.cmp_exact(x)? != Ordering::Less {
                best = Some(match best {
                    None => cand,
                    Some(b) => {
                        if cand.cmp_exact(b)? == Ordering::Less {
                            cand
                        } else {
                            b
                        }
                    }
                });
            }
        }
    }
    best
}

/// Exact unit scaling: value * numerator / denominator. Returns None unless the result is a
/// terminating decimal within the supported scale (no rounding is ever applied).
pub fn unit_scale_exact(value: Exact, numerator: Dec, denominator: Dec) -> Option<Exact> {
    if denominator.coef == 0 {
        return None;
    }
    let negative = (value.v < 0) ^ numerator.neg ^ denominator.neg;
    let vv = value.v.checked_abs()?;
    let nn = i128::try_from(numerator.coef).ok()?;
    let dd = i128::try_from(denominator.coef).ok()?;
    // value * numerator / denominator = vv*nn*10^ds / (10^(sv+sn) * dd)
    let num = vv.checked_mul(nn)?.checked_mul(10i128.checked_pow(denominator.scale)?)?;
    let den = 10i128.checked_pow(value.s + numerator.scale)?.checked_mul(dd)?;
    let g = gcd(num, den);
    let (n, d) = (num / g, den / g);
    let (mut rest, mut twos, mut fives) = (d, 0u32, 0u32);
    while rest % 2 == 0 {
        rest /= 2;
        twos += 1;
    }
    while rest % 5 == 0 {
        rest /= 5;
        fives += 1;
    }
    if rest != 1 {
        return None;
    }
    let k = twos.max(fives);
    if k > MAX_SCALE {
        return None;
    }
    let factor = 10i128.checked_pow(k)? / d;
    let m = n.checked_mul(factor)?;
    Some(Exact { v: if negative { -m } else { m }, s: k }.normalized())
}

fn gcd(a: i128, b: i128) -> i128 {
    let (mut a, mut b) = (a.abs(), b.abs());
    while b != 0 {
        let t = a % b;
        a = b;
        b = t;
    }
    if a == 0 { 1 } else { a }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_text_is_separate_from_canonical_value() {
        assert_eq!(Dec::parse(".20").unwrap().canonical(), "0.2");
        assert_eq!(Dec::parse("0.10").unwrap().canonical(), "0.1");
        assert_eq!(Dec::parse("-0.00").unwrap().canonical(), "0");
        assert_eq!(Dec::parse("1.").unwrap().canonical(), "1");
    }

    #[test]
    fn exact_arithmetic_has_no_binary_error() {
        let mean = Dec::parse("0.10").unwrap().exact();
        let sd = Dec::parse(".01").unwrap().exact();
        assert_eq!(mean.sub(sd).unwrap().fixed(2).unwrap(), "0.09");
        assert_eq!(mean.add(sd).unwrap().fixed(2).unwrap(), "0.11");
    }

    #[test]
    fn classifies_missing_ambiguous_and_malformed_separately() {
        assert_eq!(Dec::parse("").unwrap_err().class, ParseClass::Missing);
        assert_eq!(Dec::parse("   ").unwrap_err().class, ParseClass::Missing);
        assert_eq!(Dec::parse("1,234").unwrap_err().class, ParseClass::Ambiguous);
        assert_eq!(Dec::parse("0,10").unwrap_err().class, ParseClass::Ambiguous);
        assert_eq!(Dec::parse("1e-3").unwrap_err().class, ParseClass::Malformed);
        assert_eq!(Dec::parse("0.1.2").unwrap_err().class, ParseClass::Malformed);
        assert_eq!(Dec::parse("NaN").unwrap_err().class, ParseClass::Malformed);
        assert_eq!(Dec::parse("\u{2212}0.10").unwrap_err().class, ParseClass::Malformed);
        assert_eq!(Dec::parse("1234567890123456789").unwrap_err().class, ParseClass::Malformed);
    }

    #[test]
    fn nice_steps_and_multiples_are_exact() {
        assert_eq!(nice_at_least(Exact { v: 576, s: 4 }).unwrap().canonical(), "0.1");
        assert_eq!(nice_at_least(Exact { v: 2, s: 1 }).unwrap().canonical(), "0.2");
        assert_eq!(nice_at_least(Exact { v: 22, s: 1 }).unwrap().canonical(), "2.5");
        let step = Exact { v: 1, s: 1 };
        assert_eq!(floor_to_multiple(Exact { v: 66, s: 3 }, step).unwrap().canonical(), "0");
        assert_eq!(ceil_to_multiple(Exact { v: 354, s: 3 }, step).unwrap().canonical(), "0.4");
    }

    #[test]
    fn unit_scale_is_exact_or_refused() {
        let v = Dec::parse("0.25").unwrap().exact();
        let num = Dec::parse("1000").unwrap();
        let den = Dec::parse("1").unwrap();
        assert_eq!(unit_scale_exact(v, num, den).unwrap().canonical(), "250");
        let third = Dec::parse("3").unwrap();
        assert!(unit_scale_exact(v, Dec::parse("1").unwrap(), third).is_none(), "1/3 does not terminate");
        let half = Dec::parse("2").unwrap();
        assert_eq!(unit_scale_exact(v, Dec::parse("1").unwrap(), half).unwrap().canonical(), "0.125");
    }

    #[test]
    fn decade_and_rounding_helpers() {
        assert_eq!(Dec::parse("0.09").unwrap().exact().decade_floor(), Some(-2));
        assert_eq!(Dec::parse("1.00").unwrap().exact().decade_floor(), Some(0));
        assert!(Dec::parse("1.00").unwrap().exact().is_power_of_ten());
        assert!(!Dec::parse("0.33").unwrap().exact().is_power_of_ten());
        assert_eq!(round_div(5, 2), Some(3));
        assert_eq!(round_div(-5, 2), Some(-3));
        assert_eq!(Dec::parse("0.125").unwrap().exact().fixed(2).unwrap(), "0.13");
    }
}
