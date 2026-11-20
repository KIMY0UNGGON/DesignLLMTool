//! SVG path data -> [`PathSpec`] with MoveTo / LineTo / CubicTo / Close only. Pure and
//! deterministic: the same `d` gives the same integers.
use crate::{Diagnostic, DltError, DltResult, FillRule, PathCommand, PathSpec, Rect};
use std::collections::BTreeMap;
use std::f64::consts::{FRAC_PI_2, PI};

/// Output units per SVG user unit.
const SCALE: f64 = 1000.0;
/// An arc whose curve stays within this distance of its chord (user units: half an output unit) is drawn as a line.
const LINE_TOLERANCE: f64 = 0.5 / SCALE;
/// Most commands one path may hold. The parser refuses the command after the last allowed one, and the canvas model
/// refuses a path with more.
pub const PATH_MAX_COMMANDS: usize = 4096;
/// Largest |coordinate| in output units (SVG user units × 1000), for path coordinates and view box values alike. The
/// canvas model uses the same limit, so every path this parser returns passes the canvas check.
pub const PATH_MAX_COORDINATE: i64 = 100_000_000;
const COMMA: &str = "a comma must separate two arguments and be followed by a number";

impl PathSpec {
    /// Parses SVG path data (M/m L/l H/h V/v C/c S/s Q/q T/t A/a Z/z) into absolute commands in
    /// SVG user units × 1000, rounded half away from zero. H/V become lines; Q/T become cubics and
    /// S/T reflect the previous control point as SVG specifies; an arc becomes cubic pieces of at
    /// most 90° (an arc with a zero radius, or one flatter than half an output unit, is a line; one
    /// ending where it starts is dropped; pieces are cut finer while a control point is out of range,
    /// and an arc whose curve leaves the range is refused as a coordinate). After
    /// Z, a following command other than M starts its subpath with an explicit MoveTo to the closed
    /// subpath's start. `view_box` is in the same × 1000 units and must have a positive size and
    /// values within `PATH_MAX_COORDINATE`; the fill rule is nonzero. Between two arguments there is
    /// whitespace and at most one comma (SVG comma-wsp); a comma before a command's first argument or
    /// after its last one is refused. The parse stops at the first refusal, so its work is bounded by
    /// the command limit. A refusal is INVALID_ARGUMENT with one diagnostic: `code` is a `PATH_*` reason
    /// for the caller to word, and a syntax problem carries its byte offset in `params["byte"]`.
    pub fn from_svg(d: &str, view_box: Rect) -> DltResult<PathSpec> {
        let inside = |v: i64| (-PATH_MAX_COORDINATE..=PATH_MAX_COORDINATE).contains(&v);
        if view_box.width <= 0 || view_box.height <= 0 {
            return Err(refusal("PATH_VIEW_BOX", "path view_box must have a positive width and height", None));
        }
        if ![view_box.x, view_box.y, view_box.width, view_box.height].into_iter().all(inside) {
            return Err(refusal("PATH_VIEW_BOX_RANGE", "path view_box is out of range", None));
        }
        let mut parser = Parser { s: d.as_bytes(), i: 0, in_args: false };
        let mut out = Builder::default();
        parser.skip_whitespace();
        if parser.at_end() { return Err(refusal("PATH_EMPTY", "path data is empty", None)); }
        let mut previous: Option<u8> = None;
        while !parser.at_end() {
            let at = parser.i;
            let command = parser.s[at];
            if !command.is_ascii_alphabetic() { return Err(parser.refuse("PATH_COMMAND", "expected a path command")); }
            parser.i += 1;
            parser.in_args = false;
            if previous.is_none() && !matches!(command, b'M' | b'm') { return Err(refusal("PATH_START", "path data must start with M or m", Some(at))); }
            out.command(&mut parser, command)?;
            previous = Some(command);
            parser.skip_whitespace();
        }
        Ok(PathSpec { view_box, fill_rule: FillRule::Nonzero, commands: out.commands })
    }
}

/// `in_args` is set once the current command has an argument: from then on a comma may separate arguments.
struct Parser<'a> { s: &'a [u8], i: usize, in_args: bool }

impl Parser<'_> {
    fn at_end(&self) -> bool { self.i >= self.s.len() }
    fn peek(&self) -> Option<u8> { self.s.get(self.i).copied() }
    fn refuse(&self, code: &str, message: &str) -> DltError { refusal(code, message, Some(self.i)) }
    /// SVG whitespace.
    fn skip_whitespace(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r' | b'\x0c')) { self.i += 1; }
    }
    /// What comes before an argument: whitespace, and after the first argument of a command one comma at most.
    fn argument_start(&mut self) -> DltResult<()> {
        self.skip_whitespace();
        if self.in_args && self.peek() == Some(b',') {
            self.i += 1;
            self.skip_whitespace();
            if self.peek() == Some(b',') { return Err(self.refuse("PATH_COMMA", COMMA)); }
        }
        Ok(())
    }
    fn number_starts(&self) -> bool { matches!(self.peek(), Some(b'0'..=b'9' | b'.' | b'+' | b'-')) }
    /// Whether another argument set follows (implicit command repetition). A comma after the last argument is taken
    /// only when a number follows it.
    fn number_follows(&mut self) -> DltResult<bool> {
        self.skip_whitespace();
        if self.peek() != Some(b',') { return Ok(self.number_starts()); }
        self.i += 1;
        self.skip_whitespace();
        if !self.number_starts() { return Err(self.refuse("PATH_COMMA", COMMA)); }
        Ok(true)
    }
    fn number(&mut self) -> DltResult<f64> {
        self.argument_start()?;
        let start = self.i;
        if matches!(self.peek(), Some(b'+' | b'-')) { self.i += 1; }
        let int_digits = self.digits();
        let mut frac_digits = 0;
        if self.peek() == Some(b'.') { self.i += 1; frac_digits = self.digits(); }
        if int_digits == 0 && frac_digits == 0 { return Err(refusal("PATH_NUMBER", "expected a number", Some(start))); }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.i += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) { self.i += 1; }
            if self.digits() == 0 { return Err(refusal("PATH_NUMBER", "expected exponent digits", Some(start))); }
        }
        let value = std::str::from_utf8(&self.s[start..self.i]).ok().and_then(|text| text.parse::<f64>().ok());
        let value = value.ok_or_else(|| refusal("PATH_NUMBER", "expected a number", Some(start)))?;
        if !value.is_finite() { return Err(refusal("PATH_RANGE", "number out of range", Some(start))); }
        self.in_args = true;
        Ok(value)
    }
    fn digits(&mut self) -> usize {
        let start = self.i;
        while self.i < self.s.len() && self.s[self.i].is_ascii_digit() { self.i += 1; }
        self.i - start
    }
    /// An arc flag is one character, 0 or 1 (it may touch the next number).
    fn flag(&mut self) -> DltResult<bool> {
        self.argument_start()?;
        let value = match self.peek() {
            Some(b'0') => false,
            Some(b'1') => true,
            _ => return Err(self.refuse("PATH_FLAG", "expected an arc flag (0 or 1)")),
        };
        self.i += 1;
        self.in_args = true;
        Ok(value)
    }
    fn pair(&mut self) -> DltResult<(f64, f64)> { Ok((self.number()?, self.number()?)) }
}

#[derive(Default)]
struct Builder {
    commands: Vec<PathCommand>,
    /// Current point and subpath start, in user units.
    current: (f64, f64),
    start: (f64, f64),
    /// Second control point of the previous C/S, control point of the previous Q/T.
    cubic_control: Option<(f64, f64)>,
    quad_control: Option<(f64, f64)>,
    /// The previous command was Z: the next drawing command starts a subpath at `start`.
    after_close: bool,
}

/// `v` in output units, or `None` when it is not finite or past the coordinate limit.
fn scale_checked(v: f64) -> Option<i64> {
    let s = (v * SCALE).round();
    (s.is_finite() && s.abs() <= PATH_MAX_COORDINATE as f64).then_some(s as i64)
}

fn scaled(v: f64) -> DltResult<i64> {
    scale_checked(v).ok_or_else(|| refusal("PATH_RANGE", "path coordinate is out of range", None))
}

/// A refusal: INVALID_ARGUMENT with one blocking diagnostic. Its `code` is the reason for the caller to word, and
/// `params["byte"]` is the offset a syntax problem is at.
fn refusal(code: &str, message: &str, byte: Option<usize>) -> DltError {
    let mut params = BTreeMap::new();
    if let Some(byte) = byte { params.insert("byte".to_owned(), serde_json::json!(byte)); }
    let mut error = DltError::invalid(message);
    error.diagnostics.push(Diagnostic { code: code.to_owned(), message: message.to_owned(), node_id: None, blocking: true, params });
    error
}

impl Builder {
    /// Appends one command. The command after the last allowed one is refused, so a long path costs no more than the limit.
    fn push(&mut self, command: PathCommand) -> DltResult<()> {
        if self.commands.len() >= PATH_MAX_COMMANDS { return Err(refusal("PATH_COMMANDS", "path has too many commands", None)); }
        self.commands.push(command);
        Ok(())
    }

    fn command(&mut self, p: &mut Parser<'_>, command: u8) -> DltResult<()> {
        let relative = command.is_ascii_lowercase();
        let upper = command.to_ascii_uppercase();
        if upper == b'Z' {
            if self.after_close { self.reopen()?; }
            self.push(PathCommand::Close)?;
            self.current = self.start;
            self.cubic_control = None;
            self.quad_control = None;
            self.after_close = true;
            if p.number_follows()? { return Err(p.refuse("PATH_Z", "Z takes no arguments")); }
            return Ok(());
        }
        if !b"MLHVCSQTA".contains(&upper) { return Err(refusal("PATH_UNKNOWN_COMMAND", "unknown path command", Some(p.i - 1))); }
        let mut first = true;
        loop {
            let base = if relative { self.current } else { (0.0, 0.0) };
            let at = |(x, y): (f64, f64)| (base.0 + x, base.1 + y);
            if upper == b'M' && first {
                let point = at(p.pair()?);
                self.after_close = false;
                self.move_to(point)?;
            } else {
                if self.after_close { self.reopen()?; }
                match upper {
                    b'M' | b'L' => { let point = at(p.pair()?); self.line_to(point)?; }
                    b'H' => { let x = p.number()? + base.0; self.line_to((x, self.current.1))?; }
                    b'V' => { let y = p.number()? + base.1; self.line_to((self.current.0, y))?; }
                    b'C' => {
                        let (c1, c2, end) = (at(p.pair()?), at(p.pair()?), at(p.pair()?));
                        self.cubic_to(c1, c2, end)?;
                        self.cubic_control = Some(c2);
                    }
                    b'S' => {
                        let c1 = reflect(self.cubic_control, self.current);
                        let (c2, end) = (at(p.pair()?), at(p.pair()?));
                        self.cubic_to(c1, c2, end)?;
                        self.cubic_control = Some(c2);
                    }
                    b'Q' => { let (q, end) = (at(p.pair()?), at(p.pair()?)); self.quad_to(q, end)?; }
                    b'T' => { let q = reflect(self.quad_control, self.current); let end = at(p.pair()?); self.quad_to(q, end)?; }
                    b'A' => {
                        let (rx, ry, rotation) = (p.number()?, p.number()?, p.number()?);
                        let (large, sweep) = (p.flag()?, p.flag()?);
                        let end = at(p.pair()?);
                        self.arc_to(rx, ry, rotation, large, sweep, end)?;
                    }
                    _ => unreachable!("checked above"),
                }
            }
            first = false;
            if !p.number_follows()? { return Ok(()); }
        }
    }

    /// The explicit MoveTo that SVG implies after Z.
    fn reopen(&mut self) -> DltResult<()> { self.after_close = false; let start = self.start; self.move_to(start) }

    fn move_to(&mut self, point: (f64, f64)) -> DltResult<()> {
        self.push(PathCommand::MoveTo { x: scaled(point.0)?, y: scaled(point.1)? })?;
        self.current = point;
        self.start = point;
        self.cubic_control = None;
        self.quad_control = None;
        Ok(())
    }
    fn line_to(&mut self, point: (f64, f64)) -> DltResult<()> {
        self.push(PathCommand::LineTo { x: scaled(point.0)?, y: scaled(point.1)? })?;
        self.current = point;
        self.cubic_control = None;
        self.quad_control = None;
        Ok(())
    }
    fn cubic_to(&mut self, c1: (f64, f64), c2: (f64, f64), end: (f64, f64)) -> DltResult<()> {
        self.push(PathCommand::CubicTo { x1: scaled(c1.0)?, y1: scaled(c1.1)?, x2: scaled(c2.0)?, y2: scaled(c2.1)?, x: scaled(end.0)?, y: scaled(end.1)? })?;
        self.current = end;
        self.cubic_control = None;
        self.quad_control = None;
        Ok(())
    }
    /// Quadratic as the exact cubic: control points at 2/3 of the way to the quadratic control.
    fn quad_to(&mut self, q: (f64, f64), end: (f64, f64)) -> DltResult<()> {
        let from = self.current;
        let c1 = (from.0 + 2.0 / 3.0 * (q.0 - from.0), from.1 + 2.0 / 3.0 * (q.1 - from.1));
        let c2 = (end.0 + 2.0 / 3.0 * (q.0 - end.0), end.1 + 2.0 / 3.0 * (q.1 - end.1));
        self.cubic_to(c1, c2, end)?;
        self.quad_control = Some(q);
        Ok(())
    }
    /// SVG 1.1 implementation notes F.6.5/F.6.6, computed from the start point so that a huge radius keeps its precision:
    /// no center is written as an absolute coordinate, and a piece's control points are offsets from its end points along
    /// the tangents (k = 4/3 · tan(θ/4)). An arc whose curve stays within LINE_TOLERANCE of its chord is a line. Pieces are
    /// at most 90°, and finer while a control point is out of range.
    fn arc_to(&mut self, rx: f64, ry: f64, rotation_deg: f64, large: bool, sweep: bool, end: (f64, f64)) -> DltResult<()> {
        let from = self.current;
        if from == end { self.cubic_control = None; self.quad_control = None; return Ok(()); }
        let (mut rx, mut ry) = (rx.abs(), ry.abs());
        if rx == 0.0 || ry == 0.0 { return self.line_to(end); }
        let (sin_phi, cos_phi) = rotation_deg.to_radians().sin_cos();
        // The chord in the ellipse's frame over the radii: the end point is (2a, 2b) in the unit circle's frame.
        let (dx, dy) = (end.0 - from.0, end.1 - from.1);
        let (mut a, mut b) = ((cos_phi * dx + sin_phi * dy) / (2.0 * rx), (cos_phi * dy - sin_phi * dx) / (2.0 * ry));
        let mut half = a.hypot(b);
        // Radii too small to span the chord grow until it is a diameter (F.6.6 step 2).
        if half > 1.0 {
            (rx, ry) = (rx * half, ry * half);
            (a, b) = (a / half, b / half);
            half = 1.0;
        }
        let end_unit = (2.0 * a, 2.0 * b);
        // The center's distance from the chord's midpoint, in the unit circle's frame.
        let cos_center = (1.0 - half * half).max(0.0).sqrt();
        // The farthest the arc gets from its chord, in units of the larger radius: 1 - cos for the short arc (half²/(1 + cos)
        // is the same without cancellation), 1 + cos for the long one.
        let deviation = if large { 1.0 + cos_center } else { half * half / (1.0 + cos_center) };
        if rx.max(ry) * deviation < LINE_TOLERANCE { return self.line_to(end); }
        if half == 0.0 { return Err(refusal("PATH_RANGE", "path coordinate is out of range", None)); }
        // The center is end_unit / 2 + s · cos_center · perpendicular(end_unit) / |end_unit|, with s = +1 when large != sweep
        // (F.6.5). `radial` is the offset from the center to the start point, `along` the tangent direction there.
        let s = if large != sweep { 1.0 } else { -1.0 };
        let length = 2.0 * half;
        let perpendicular = (-end_unit.1 / length, end_unit.0 / length);
        let radial = (-(end_unit.0 / 2.0 + s * cos_center * perpendicular.0), -(end_unit.1 / 2.0 + s * cos_center * perpendicular.1));
        let along = (-radial.1, radial.0);
        let short = 2.0 * half.asin();
        let magnitude = if large { 2.0 * PI - short } else { short };
        let total = if sweep { magnitude } else { -magnitude };
        // The point at angle t from the start, relative to it: (cos t − 1)·radial + sin t·along, with cos t − 1 written as
        // −2·sin²(t/2). Its tangent is cos t·along − sin t·radial. Nothing here subtracts two large numbers.
        let point = |t: f64| {
            let c = -2.0 * (t / 2.0).sin().powi(2);
            let sin_t = t.sin();
            (c * radial.0 + sin_t * along.0, c * radial.1 + sin_t * along.1)
        };
        let tangent = |t: f64| {
            let (sin_t, cos_t) = t.sin_cos();
            (cos_t * along.0 - sin_t * radial.0, cos_t * along.1 - sin_t * radial.1)
        };
        // The unit circle's frame to the output: the radii and the rotation, about the start point.
        let to_absolute = |q: (f64, f64)| (from.0 + cos_phi * rx * q.0 - sin_phi * ry * q.1, from.1 + sin_phi * rx * q.0 + cos_phi * ry * q.1);
        let in_range = |p: (f64, f64)| scale_checked(p.0).is_some() && scale_checked(p.1).is_some();
        // `count` equal pieces, each as (control 1, control 2, end) in user units.
        let cut = |count: usize| -> Result<Vec<[(f64, f64); 3]>, ArcFault> {
            let step = total / count as f64;
            let k = 4.0 / 3.0 * (step / 4.0).tan();
            let mut pieces = Vec::with_capacity(count);
            for i in 0..count {
                let (t0, t1) = (step * i as f64, step * (i + 1) as f64);
                let (p0, p1) = (point(t0), point(t1));
                let (d0, d1) = (tangent(t0), tangent(t1));
                let control_1 = to_absolute((p0.0 + k * d0.0, p0.1 + k * d0.1));
                let control_2 = to_absolute((p1.0 - k * d1.0, p1.1 - k * d1.1));
                let piece_end = if i + 1 == count { end } else { to_absolute(p1) };
                pieces.push([control_1, control_2, piece_end]);
            }
            // The piece ends are on the curve, so one out of range means the curve leaves the range.
            if pieces.iter().any(|piece| !in_range(piece[2])) { return Err(ArcFault::Curve); }
            if pieces.iter().any(|piece| !in_range(piece[0]) || !in_range(piece[1])) { return Err(ArcFault::Control); }
            Ok(pieces)
        };
        let budget = PATH_MAX_COMMANDS - self.commands.len();
        let mut count = ((magnitude / FRAC_PI_2) - 1e-9).ceil().max(1.0) as usize;
        if count > budget { return Err(refusal("PATH_COMMANDS", "path has too many commands", None)); }
        let pieces = loop {
            match cut(count) {
                Ok(pieces) => break pieces,
                Err(ArcFault::Curve) => return Err(refusal("PATH_RANGE", "path coordinate is out of range", None)),
                Err(ArcFault::Control) if count * 2 <= budget => count *= 2,
                Err(ArcFault::Control) => {
                    return Err(refusal("PATH_CONTROL_RANGE", "a control point of the arc is out of the coordinate range, even in finer pieces", None));
                }
            }
        };
        for [control_1, control_2, piece_end] in pieces { self.cubic_to(control_1, control_2, piece_end)?; }
        Ok(())
    }
}

/// Why an arc's pieces are refused: the curve leaves the coordinate range, or a control point does and finer pieces
/// would not keep it in.
enum ArcFault {
    Curve,
    Control,
}

/// Reflection of the previous control point about the current point (or the current point).
fn reflect(control: Option<(f64, f64)>, current: (f64, f64)) -> (f64, f64) {
    match control { Some((x, y)) => (2.0 * current.0 - x, 2.0 * current.1 - y), None => current }
}

#[cfg(test)]
mod tests {
    use super::*;
    use PathCommand::{Close, CubicTo, LineTo, MoveTo};

    const VB: Rect = Rect { x: 0, y: 0, width: 24_000, height: 24_000 };
    fn parse(d: &str) -> Vec<PathCommand> { PathSpec::from_svg(d, VB).unwrap_or_else(|e| panic!("{d:?}: {}", e.message)).commands }
    fn cubic(x1: i64, y1: i64, x2: i64, y2: i64, x: i64, y: i64) -> PathCommand { CubicTo { x1, y1, x2, y2, x, y } }

    #[test]
    fn lines_and_relative_coordinates() {
        assert_eq!(parse("M 1 2 L 3 4 H 5 V 6 Z"), [MoveTo { x: 1000, y: 2000 }, LineTo { x: 3000, y: 4000 }, LineTo { x: 5000, y: 4000 }, LineTo { x: 5000, y: 6000 }, Close]);
        assert_eq!(parse("m1 2 l3 4 h-1 v-2 z"), [MoveTo { x: 1000, y: 2000 }, LineTo { x: 4000, y: 6000 }, LineTo { x: 3000, y: 6000 }, LineTo { x: 3000, y: 4000 }, Close]);
        // Implicit repetition: pairs after M are lines (relative after m).
        assert_eq!(parse("M0 0 10 0 10 10"), [MoveTo { x: 0, y: 0 }, LineTo { x: 10_000, y: 0 }, LineTo { x: 10_000, y: 10_000 }]);
        assert_eq!(parse("m1 1 1 1"), [MoveTo { x: 1000, y: 1000 }, LineTo { x: 2000, y: 2000 }]);
        // Packed numbers, exponents, commas, rounding to the nearest and half away from zero.
        assert_eq!(parse("M.5.5L-1-1,1e1 2E-1 l0.0006-0.0006 M-0.0005 0.0005"),
            [MoveTo { x: 500, y: 500 }, LineTo { x: -1000, y: -1000 }, LineTo { x: 10_000, y: 200 }, LineTo { x: 10_001, y: 199 }, MoveTo { x: -1, y: 1 }]);
    }

    #[test]
    fn s_and_t_reflect_the_previous_control_point() {
        assert_eq!(parse("M0 0 C 1 0 2 1 3 3 S 5 6 6 6"), [MoveTo { x: 0, y: 0 }, cubic(1000, 0, 2000, 1000, 3000, 3000), cubic(4000, 5000, 5000, 6000, 6000, 6000)]);
        // S without a preceding C uses the current point.
        assert_eq!(parse("M0 0 L 1 1 S 2 2 3 1"), [MoveTo { x: 0, y: 0 }, LineTo { x: 1000, y: 1000 }, cubic(1000, 1000, 2000, 2000, 3000, 1000)]);
        // Q as the exact cubic, T reflects the quadratic control: (3,3) about (6,0) is (9,-3).
        assert_eq!(parse("M0 0 Q 3 3 6 0 T 12 0"), [MoveTo { x: 0, y: 0 }, cubic(2000, 2000, 4000, 2000, 6000, 0), cubic(8000, -2000, 10_000, -2000, 12_000, 0)]);
        assert_eq!(parse("m0 0 q3 3 6 0 t6 0"), parse("M0 0 Q 3 3 6 0 T 12 0"));
        // T after a cubic does not reflect it: its control is the current point (3,0).
        assert_eq!(parse("M0 0 C 1 1 2 1 3 0 T 6 0")[2], cubic(3000, 0, 4000, 0, 6000, 0));
    }

    #[test]
    fn arcs_become_cubic_pieces_of_at_most_90_degrees() {
        // Upper half circle, radius 10: two quarter pieces, k = 4/3 tan(22.5°).
        assert_eq!(parse("M 0 0 A 10 10 0 0 1 20 0"), [MoveTo { x: 0, y: 0 }, cubic(0, -5523, 4477, -10_000, 10_000, -10_000), cubic(15_523, -10_000, 20_000, -5523, 20_000, 0)]);
        // The same arc in relative form, and the lower half with the other sweep.
        assert_eq!(parse("m0 0 a10 10 0 0 1 20 0"), parse("M 0 0 A 10 10 0 0 1 20 0"));
        assert_eq!(parse("M 0 0 A 10 10 0 0 0 20 0")[1], cubic(0, 5523, 4477, 10_000, 10_000, 10_000));
        // A 270° arc around (0,0): three quarter pieces, the last ending exactly at the target.
        assert_eq!(parse("M 10 0 A 10 10 0 1 1 0 -10"), [MoveTo { x: 10_000, y: 0 }, cubic(10_000, 5523, 5523, 10_000, 0, 10_000),
            cubic(-5523, 10_000, -10_000, 5523, -10_000, 0), cubic(-10_000, -5523, -5523, -10_000, 0, -10_000)]);
        // Too small radii are scaled up (here to 5), a zero radius is a line, a zero-length arc is dropped.
        assert_eq!(parse("M0 0 A 1 1 0 0 1 10 0").len(), 3);
        assert_eq!(parse("M0 0 A 0 5 0 0 1 10 0"), [MoveTo { x: 0, y: 0 }, LineTo { x: 10_000, y: 0 }]);
        assert_eq!(parse("M3 3 A 5 5 0 0 1 3 3 L 4 4"), [MoveTo { x: 3000, y: 3000 }, LineTo { x: 4000, y: 4000 }]);
        // Flags may touch the next number; a rotated ellipse still ends on its target.
        assert_eq!(parse("M0 0 a5 5 0 0010 0"), parse("M0 0 A 5 5 0 0 0 10 0"));
        let rotated = parse("M 0 0 A 20 10 30 0 1 15 7");
        assert!(matches!(rotated.last(), Some(CubicTo { x: 15_000, y: 7000, .. })), "{rotated:?}");
        assert_eq!(parse("M 0 0 A 20 10 30 0 1 15 7"), rotated, "deterministic");
    }

    /// The points a command holds, in output units.
    fn points(command: &PathCommand) -> Vec<(i64, i64)> {
        match *command {
            MoveTo { x, y } | LineTo { x, y } => vec![(x, y)],
            CubicTo { x1, y1, x2, y2, x, y } => vec![(x1, y1), (x2, y2), (x, y)],
            Close => Vec::new(),
        }
    }

    #[test]
    fn a_flat_arc_is_a_line_whatever_its_radius() {
        // A radius of 1e15 over a chord of 0.001 bulges about 1e-22: a line. A radius of 1 over a chord of 0.1 bulges 0.00125,
        // more than half an output unit, so it stays a curve.
        assert_eq!(parse("M 0 0 A 1e15 1e15 0 0 1 0.001 0"), [MoveTo { x: 0, y: 0 }, LineTo { x: 1, y: 0 }]);
        assert_eq!(parse("M 0 0 A 1e12 1e12 0 0 1 0.001 0"), [MoveTo { x: 0, y: 0 }, LineTo { x: 1, y: 0 }]);
        assert_eq!(parse("M 0 0 A 1e15 1e15 0 0 1 10 0"), [MoveTo { x: 0, y: 0 }, LineTo { x: 10_000, y: 0 }]);
        assert!(matches!(parse("M 0 0 A 1 1 0 0 1 0.1 0")[1], CubicTo { .. }));
        // A radius too small to span the chord is scaled up to half the chord: the semicircle of radius 5.
        assert_eq!(parse("M 0 0 A 1e-200 1e-200 0 0 1 10 0"), parse("M 0 0 A 5 5 0 0 1 10 0"));
        // A chord that vanishes at this precision under a huge radius is a line as well.
        assert_eq!(parse("M 0 0 A 1e300 1e300 0 0 1 1e-300 0"), [MoveTo { x: 0, y: 0 }, LineTo { x: 0, y: 0 }]);
    }

    #[test]
    fn a_curved_arc_with_a_huge_radius_keeps_its_bulge() {
        // Radius 1e12 over the chord from (-100 000, 0) to (100 000, 0): the sagitta is 0.005 user units, 5 output units, toward
        // negative y as the sweep flag says. The cubic's midpoint (t = 1/2) is that point.
        let commands = parse("M -100000 0 A 1e12 1e12 0 0 1 100000 0");
        let CubicTo { x1, y1, x2, y2, x, y } = commands[1] else { panic!("one cubic piece: {commands:?}") };
        let mid = ((-100_000_000 + 3 * x1 + 3 * x2 + x) as f64 / 8.0, (3 * y1 + 3 * y2 + y) as f64 / 8.0);
        assert!(mid.0.abs() <= 1.0 && (mid.1 + 5.0).abs() <= 1.0, "{mid:?}");
    }

    #[test]
    fn an_arc_is_cut_finer_while_a_control_point_leaves_the_range() {
        // Both ends are in range and the semicircle reaches x = 100 000, the limit: two 90° pieces would put a control point at
        // x ≈ 109 762. Four pieces keep every point in range, and the path ends on the end point.
        let d = "M 70710.678 -70710.678 A 100000 100000 0 0 1 -70710.678 70710.678";
        let commands = parse(d);
        assert_eq!(commands.len(), 5, "a move and four pieces: {commands:?}");
        assert!(commands.iter().flat_map(points).all(|(x, y)| x.abs() <= 100_000_000 && y.abs() <= 100_000_000), "{commands:?}");
        assert!(matches!(commands[4], CubicTo { x: -70_710_678, y: 70_710_678, .. }), "{commands:?}");
        // Two such arcs make the circle, and each is cut the same way.
        assert_eq!(parse(&format!("{d} A 100000 100000 0 0 1 70710.678 -70710.678")).len(), 9);
    }

    #[test]
    fn a_control_point_that_no_finer_cut_fits_in_the_command_limit_is_refused_as_a_control_point() {
        // The semicircle after `lines` lines (plus the move). Its four pieces need four commands: after 4092 commands four are
        // left and the pieces fit exactly; after 4093 only three are left, and no finer cut is allowed.
        let arc = " A 100000 100000 0 0 1 -70710.678 70710.678";
        let after = |lines: usize| format!("M 70710.678 -70710.678{}{arc}", " L 70710.678 -70710.678".repeat(lines));
        assert_eq!(parse(&after(4091)).len(), PATH_MAX_COMMANDS);
        assert_eq!(refused(&after(4092)).0, "PATH_CONTROL_RANGE");
    }

    #[test]
    fn an_arc_whose_curve_leaves_the_range_is_refused_as_a_coordinate() {
        // A major arc of radius 1e15 over a short chord is nearly a circle of that radius, and its points leave the range.
        assert_eq!(refused("M 0 0 A 1e15 1e15 0 1 1 0.001 0").0, "PATH_RANGE");
        // A chord that vanishes at this precision under a huge radius, with the long arc: refused, not drawn as a point.
        assert_eq!(refused("M 0 0 A 1e300 1e300 0 1 1 1e-300 0").0, "PATH_RANGE");
    }

    #[test]
    fn a_command_after_z_starts_its_subpath_at_the_closed_start() {
        assert_eq!(parse("M 1 1 L 10 1 L 10 10 Z L 5 5"), [MoveTo { x: 1000, y: 1000 }, LineTo { x: 10_000, y: 1000 }, LineTo { x: 10_000, y: 10_000 }, Close, MoveTo { x: 1000, y: 1000 }, LineTo { x: 5000, y: 5000 }]);
        // Relative after Z is relative to that start.
        assert_eq!(parse("M1 1 l1 1 z l2 2"), [MoveTo { x: 1000, y: 1000 }, LineTo { x: 2000, y: 2000 }, Close, MoveTo { x: 1000, y: 1000 }, LineTo { x: 3000, y: 3000 }]);
        // An explicit M after Z needs no extra MoveTo.
        assert_eq!(parse("M0 0 L1 1 Z M5 5 L6 6"), [MoveTo { x: 0, y: 0 }, LineTo { x: 1000, y: 1000 }, Close, MoveTo { x: 5000, y: 5000 }, LineTo { x: 6000, y: 6000 }]);
    }

    #[test]
    fn bad_input_is_refused() {
        for bad in ["", "   ", "L 1 2", "1 2", "M 1", "M 1 2 X 3", "M 1 2 L 3", "M1 2 Z 3", "M 1e 2", "M nan 2", "M 1e999 0", "M 1e13 0",
                    "M 0 0 A 1 1 0 2 0 1 1", "M 0 0 A 1 1 0 0 1 1", "M 0 0 --1 2", "M 0 0 L 1 2 3", "M\u{2028}0 0"] {
            let e = PathSpec::from_svg(bad, VB).unwrap_err();
            assert_eq!(e.code, crate::ErrorCode::InvalidArgument, "{bad:?}");
        }
        assert!(PathSpec::from_svg("M0 0", Rect { x: 0, y: 0, width: 0, height: 10 }).is_err(), "empty view box");
    }

    /// The reason code of a refusal and the byte it is at: the tests compare these, not the wording.
    fn refused(d: &str) -> (String, Option<u64>) {
        let error = PathSpec::from_svg(d, VB).err().unwrap_or_else(|| panic!("{d:?} should be refused"));
        let diagnostic = &error.diagnostics[0];
        (diagnostic.code.clone(), diagnostic.params.get("byte").and_then(serde_json::Value::as_u64))
    }

    #[test]
    fn the_command_limit_is_4096_and_the_command_after_it_is_refused() {
        let at_limit = format!("M0 0{}", " L1 1".repeat(PATH_MAX_COMMANDS - 1));
        assert_eq!(PathSpec::from_svg(&at_limit, VB).expect("4096 commands").commands.len(), PATH_MAX_COMMANDS);
        let over = format!("M0 0{}", " L1 1".repeat(PATH_MAX_COMMANDS));
        assert_eq!(refused(&over).0, "PATH_COMMANDS");
        // Implicit repetitions are commands too.
        let implicit = format!("M0 0{}", " 1 1".repeat(PATH_MAX_COMMANDS));
        assert_eq!(refused(&implicit).0, "PATH_COMMANDS");
        // The refusal comes at the limit, before the bad token at the end is read.
        let tail = format!("M0 0{} L 1e999", " L1 1".repeat(PATH_MAX_COMMANDS));
        assert_eq!(refused(&tail).0, "PATH_COMMANDS");
    }

    #[test]
    fn coordinates_are_limited_to_the_canvas_range_inclusive() {
        // 100 000 user units is 100 000 000 output units, the largest value the canvas takes.
        assert_eq!(parse("M 100000 -100000"), [MoveTo { x: 100_000_000, y: -100_000_000 }]);
        assert_eq!(refused("M 100001 0").0, "PATH_RANGE");
        assert_eq!(refused("M -100001 0").0, "PATH_RANGE");
        assert_eq!(refused("M 0 0 C 0 0 100001 0 0 0").0, "PATH_RANGE");
        assert_eq!(refused("M 0 0 A 1 1 0 0 1 100001 0").0, "PATH_RANGE");
        // Rounding decides at the edge: 100000.0004 rounds onto the limit, 100000.0006 rounds past it.
        assert_eq!(parse("M 100000.0004 0"), [MoveTo { x: 100_000_000, y: 0 }]);
        assert_eq!(refused("M 100000.0006 0").0, "PATH_RANGE");
        // The view box has the same limit, and an extreme value is refused without overflow.
        let view_box_code = |x: i64, width: i64| PathSpec::from_svg("M0 0", Rect { x, y: 0, width, height: 24_000 }).err().map(|error| error.diagnostics[0].code.clone());
        assert_eq!(view_box_code(-100_000_000, 100_000_000), None, "the limit itself is kept");
        assert_eq!(view_box_code(0, 100_000_001).as_deref(), Some("PATH_VIEW_BOX_RANGE"));
        assert_eq!(view_box_code(i64::MIN, 24_000).as_deref(), Some("PATH_VIEW_BOX_RANGE"));
    }

    #[test]
    fn commas_separate_arguments_once_and_never_trail() {
        assert_eq!(parse("M 0,0 L 1 , 1 C 2,3 4 , 5 6,7"), parse("M0 0 L1 1 C2 3 4 5 6 7"));
        assert_eq!(parse("M0,0,1,1"), parse("M0 0 1 1"));
        assert_eq!(parse("M0 0 A 5 5 0,0,1,10,0"), parse("M0 0 A 5 5 0 0 1 10 0"));
        for (d, code) in [
            ("M,0 0", "PATH_NUMBER"),
            ("M 0,,0", "PATH_COMMA"),
            ("M 0 0 L 1 1,", "PATH_COMMA"),
            ("M 0 0 L 1 1, Z", "PATH_COMMA"),
            ("M 0 0,L 1 1", "PATH_COMMA"),
            ("M 0 0 ,, 1 1", "PATH_COMMA"),
            ("M 0 0 A 5 5 0,,0 1 10 0", "PATH_COMMA"),
            (", M 0 0", "PATH_COMMAND"),
        ] {
            assert_eq!(refused(d).0, code, "{d:?}");
        }
    }

    #[test]
    fn a_refusal_names_its_reason_and_the_byte_of_a_syntax_problem() {
        assert_eq!(refused("").0, "PATH_EMPTY");
        assert_eq!(refused("L 1 2"), ("PATH_START".to_owned(), Some(0)));
        assert_eq!(refused("M 0 0 X 1"), ("PATH_UNKNOWN_COMMAND".to_owned(), Some(6)));
        assert_eq!(refused("M 1 2 L 3"), ("PATH_NUMBER".to_owned(), Some(9)));
        assert_eq!(refused("M 0 0 L 1 1 #"), ("PATH_COMMAND".to_owned(), Some(12)));
        assert_eq!(refused("M 0 0 Z 2").0, "PATH_Z");
        assert_eq!(refused("M 0 0 A 1 1 0 2 0 1 1").0, "PATH_FLAG");
    }
}
