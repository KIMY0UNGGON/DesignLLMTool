//! Typesetting of a Diagram math node (`NodeKind::Text` with `NodeExt.math`, `05-decisions/figures.md` "수식"). The
//! source is parsed by `designllm_contracts::figure_math::parse`, the grammar the desktop checks before it sends a change;
//! this module only places the parsed items. Text is measured through the caller's `measure` (the injected
//! RenderProvider), and the result is a list of pieces relative to the formula's left edge and baseline:
//!
//! - text pieces: one visual line each. A run of characters with no space between them is one piece; a script is a
//!   piece of its own at 0.7 times the size (0.49 times from the second script level on) with a moved baseline;
//! - rules: the fraction bar and the radical overbar (`Primitive::Line`);
//! - radical signs: an open stroked path (`Primitive::Path`, no fill).
//!
//! The placement follows TeX's display style in a reduced form, in whole EMU. Characters are not italicised and `-` is
//! drawn as written. Ink heights are estimated from the font metrics (cap height for text), because a shaped line carries
//! no glyph bounds; the extent of a formula (`ascent`, `descent`) is the union of the line boxes and strokes it draws, so
//! every primitive lies inside it.

use designllm_contracts::figure_math::MathItem;
use designllm_contracts::{DltError, DltResult, FillRule, PathCommand, PathSpec, Point, Rect, ShapedText, Style};

/// Measures one line of text in a style (the layout's cached `RenderProvider::shape_text`).
pub(crate) type Measure<'m> = dyn FnMut(&str, &Style) -> DltResult<ShapedText> + 'm;

/// One drawn part of a formula. `x` and `y` are relative to the formula's left edge and baseline; y grows downwards.
pub(crate) enum Piece {
    /// One visual line whose baseline starts at (`x`, `baseline`).
    Text { x: i64, baseline: i64, text: String, style: Style, shaped: ShapedText },
    /// A horizontal rule along its centre line.
    Rule { from: Point, to: Point, style: Style },
    /// A radical sign: `path.view_box` is the size of `bounds`, so its commands are EMU offsets inside `bounds`.
    Radical { bounds: Rect, path: PathSpec, style: Style },
}

/// A typeset formula: its width and how far its drawing reaches above and below the baseline.
pub(crate) struct Formula {
    pub(crate) width: i64,
    pub(crate) ascent: i64,
    pub(crate) descent: i64,
    pub(crate) pieces: Vec<Piece>,
}

/// Typesets parsed items in `style` (its font size is the size of the formula's main line).
///
/// The font size is checked as for every other figure label: the main size is measured first, so the renderer's own size
/// rule (`RenderProvider::shape_text`; the shared renderer takes 1..=10000 pt) refuses a size it does not draw before
/// anything is placed. A size that is not positive is refused here too, for a provider that does not check. The
/// arithmetic saturates, so no size or text metric can overflow it.
pub(crate) fn typeset(items: &[MathItem], style: &Style, measure: &mut Measure<'_>) -> DltResult<Formula> {
    if style.font_size <= 0 {
        return Err(DltError::invalid(format!("font size {} EMU of a formula must be positive", style.font_size)));
    }
    let mut setter = Setter { style, measure };
    setter.metrics(0)?;
    let row = setter.list(items, 0)?;
    Ok(Formula { width: row.width, ascent: row.over, descent: row.under, pieces: row.pieces })
}

/// `value * n / d`, rounded toward zero, without overflow (saturated to the i64 range). `d` is a positive constant.
fn part(value: i64, n: i64, d: i64) -> i64 {
    let exact = i128::from(value) * i128::from(n) / i128::from(d);
    i64::try_from(exact).unwrap_or(if exact < 0 { i64::MIN } else { i64::MAX })
}

/// Font size at a script level: 1 at the main line, 0.7 in a script, 0.49 in a script of a script and deeper.
fn size_at(base: i64, level: u8) -> i64 {
    match level {
        0 => base,
        1 => part(base, 7, 10),
        _ => part(base, 49, 100),
    }
}

/// In 1/18 of the font size (TeX's mu).
fn mu(size: i64, n: i64) -> i64 {
    part(size, n, 18)
}

/// TeX atom classes, as far as they decide the spacing of a line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    Ord,
    Bin,
    Rel,
    Punct,
    Open,
    Close,
}

fn class_of(c: char) -> Class {
    match c {
        '+' | '-' | '*' | '\u{00B1}' | '\u{00D7}' | '\u{00B7}' | '\u{00F7}' => Class::Bin,
        '=' | '<' | '>' | ':' | '\u{2264}' | '\u{2265}' | '\u{2260}' | '\u{2248}' | '\u{2190}' | '\u{2192}' => Class::Rel,
        ',' | ';' => Class::Punct,
        '(' | '[' | '{' => Class::Open,
        ')' | ']' | '}' | '!' => Class::Close,
        _ => Class::Ord,
    }
}

fn item_class(item: &MathItem) -> Class {
    match item {
        MathItem::Glyph(c) => class_of(*c),
        _ => Class::Ord,
    }
}

/// Space between two atoms on the main line, in mu (TeX's table, without the inner class). Scripts have none.
fn space_mu(previous: Class, next: Class) -> i64 {
    use Class::{Bin, Close, Open, Punct, Rel};
    match (previous, next) {
        (Bin, _) | (_, Bin) => 4,
        (Rel, Rel) | (Open, Rel) | (Rel, Close) | (Rel, Punct) => 0,
        (Rel, _) | (_, Rel) => 5,
        (Punct, _) => 3,
        _ => 0,
    }
}

/// A box under construction: its pieces, its width, its estimated ink height and depth, and the extent of its drawing.
#[derive(Default)]
struct Row {
    pieces: Vec<Piece>,
    width: i64,
    /// Estimated ink above the baseline, for placing scripts, fraction parts and radicals.
    height: i64,
    /// Estimated ink below the baseline.
    depth: i64,
    /// Drawn extent above the baseline (line boxes and strokes).
    over: i64,
    /// Drawn extent below the baseline.
    under: i64,
    /// One run of characters and nothing else: scripts attach to it by the font parameters only (TeX rule 18a).
    simple: bool,
}

fn shift(point: Point, dx: i64, dy: i64) -> Point {
    Point { x: point.x.saturating_add(dx), y: point.y.saturating_add(dy) }
}

impl Piece {
    fn moved(self, dx: i64, dy: i64) -> Piece {
        match self {
            Piece::Text { x, baseline, text, style, shaped } => {
                Piece::Text { x: x.saturating_add(dx), baseline: baseline.saturating_add(dy), text, style, shaped }
            }
            Piece::Rule { from, to, style } => Piece::Rule { from: shift(from, dx, dy), to: shift(to, dx, dy), style },
            Piece::Radical { bounds, path, style } => {
                Piece::Radical { bounds: Rect { x: bounds.x.saturating_add(dx), y: bounds.y.saturating_add(dy), ..bounds }, path, style }
            }
        }
    }
}

impl Row {
    /// Places `inner` with its left edge at `x` and its baseline `dy` below this row's baseline.
    fn put(&mut self, inner: Row, x: i64, dy: i64) {
        self.height = self.height.max(inner.height.saturating_sub(dy));
        self.depth = self.depth.max(inner.depth.saturating_add(dy));
        self.over = self.over.max(inner.over.saturating_sub(dy));
        self.under = self.under.max(inner.under.saturating_add(dy));
        self.width = self.width.max(x.saturating_add(inner.width));
        self.pieces.extend(inner.pieces.into_iter().map(|piece| piece.moved(x, dy)));
    }

    /// Appends `inner` at the end of the row, on the same baseline.
    fn append(&mut self, inner: Row) {
        let x = self.width;
        self.put(inner, x, 0);
    }

    /// A horizontal rule from `x0` to `x1` whose centre line is at `y`.
    fn rule(&mut self, x0: i64, x1: i64, y: i64, style: Style) {
        let half = style.stroke_width / 2;
        self.height = self.height.max(half.saturating_sub(y));
        self.over = self.over.max(half.saturating_sub(y));
        self.under = self.under.max(y.saturating_add(half));
        self.width = self.width.max(x1);
        self.pieces.push(Piece::Rule { from: Point { x: x0, y }, to: Point { x: x1, y }, style });
    }
}

struct Setter<'s, 'm> {
    style: &'s Style,
    measure: &'s mut Measure<'m>,
}

/// Size-dependent quantities of one script level.
struct Metrics {
    size: i64,
    /// Cap height of the font at this size.
    cap: i64,
    /// Thickness of fraction bars and radical strokes.
    rule: i64,
}

impl Metrics {
    /// The x-height, estimated as 3/4 of the cap height.
    fn x_height(&self) -> i64 {
        part(self.cap, 3, 4)
    }

    /// The math axis (where a fraction bar sits): half the cap height above the baseline.
    fn axis(&self) -> i64 {
        self.cap / 2
    }
}

impl Setter<'_, '_> {
    fn style_at(&self, level: u8) -> Style {
        Style { font_size: size_at(self.style.font_size, level), ..self.style.clone() }
    }

    fn metrics(&mut self, level: u8) -> DltResult<Metrics> {
        let style = self.style_at(level);
        let shaped = (self.measure)("+", &style)?;
        let size = style.font_size;
        let cap = if shaped.cap_height > 0 { shaped.cap_height } else { part(size, 7, 10) };
        // 0.05 em; one and a half times that for bold text, so the strokes keep the weight of the characters.
        let thin = (size / 20).max(1);
        Ok(Metrics { size, cap, rule: if self.style.bold { part(thin, 3, 2) } else { thin } })
    }

    /// The style of a rule or radical stroke: the text colour, the rule thickness, no fill, and the node's opacity.
    fn stroke(&self, thickness: i64) -> Style {
        Style {
            color: self.style.color.clone(),
            stroke: self.style.color.clone(),
            fill: "none".to_owned(),
            stroke_width: thickness,
            opacity_milli: self.style.opacity_milli,
            ..Style::default()
        }
    }

    /// One run of characters at a script level.
    fn text(&mut self, text: String, level: u8) -> DltResult<Row> {
        let style = self.style_at(level);
        let shaped = (self.measure)(&text, &style)?;
        let cap = if shaped.cap_height > 0 { shaped.cap_height } else { part(style.font_size, 7, 10) };
        Ok(Row {
            width: shaped.size.width,
            // Ascenders and round letters reach a little above the cap height.
            height: part(cap, 11, 10),
            depth: shaped.descent.max(0),
            over: shaped.baseline,
            under: shaped.size.height.saturating_sub(shaped.baseline),
            simple: true,
            pieces: vec![Piece::Text { x: 0, baseline: 0, text, style, shaped }],
        })
    }

    /// A list of items set side by side at a script level, with TeX's spacing on the main line.
    fn list(&mut self, items: &[MathItem], level: u8) -> DltResult<Row> {
        let size = size_at(self.style.font_size, level);
        let mut row = Row::default();
        let mut run = String::new();
        let mut previous: Option<Class> = None;
        for (i, item) in items.iter().enumerate() {
            let class = match item {
                MathItem::Glyph(c) => {
                    let class = class_of(*c);
                    // A binary operator with no left or right operand is an ordinary character (a sign: -x, a = -b).
                    let lone = matches!(previous, None | Some(Class::Bin | Class::Rel | Class::Open | Class::Punct))
                        || matches!(items.get(i + 1).map(item_class), None | Some(Class::Rel | Class::Close | Class::Punct));
                    if class == Class::Bin && lone { Class::Ord } else { class }
                }
                MathItem::ThinSpace => {
                    self.flush(&mut row, &mut run, level)?;
                    row.width = row.width.saturating_add(mu(size, 3));
                    continue;
                }
                _ => Class::Ord,
            };
            let space = match previous {
                Some(before) if level == 0 => mu(size, space_mu(before, class)),
                _ => 0,
            };
            if space > 0 {
                self.flush(&mut row, &mut run, level)?;
                row.width = row.width.saturating_add(space);
            }
            match item {
                MathItem::Glyph(c) => run.push(*c),
                other => {
                    self.flush(&mut row, &mut run, level)?;
                    let inner = self.item(other, level)?;
                    row.append(inner);
                }
            }
            previous = Some(class);
        }
        self.flush(&mut row, &mut run, level)?;
        // A row that is one run keeps the run's script rule.
        row.simple = row.pieces.len() == 1 && matches!(row.pieces[0], Piece::Text { .. });
        Ok(row)
    }

    /// Ends the current run of characters: it becomes one text piece at the end of the row.
    fn flush(&mut self, row: &mut Row, run: &mut String, level: u8) -> DltResult<()> {
        if run.is_empty() {
            return Ok(());
        }
        let piece = self.text(std::mem::take(run), level)?;
        row.append(piece);
        Ok(())
    }

    fn item(&mut self, item: &MathItem, level: u8) -> DltResult<Row> {
        match item {
            MathItem::Glyph(c) => self.text(c.to_string(), level),
            MathItem::ThinSpace => Ok(Row { width: mu(size_at(self.style.font_size, level), 3), ..Row::default() }),
            MathItem::Group(items) => {
                let mut row = self.list(items, level)?;
                // A script on a group is placed by the group's height.
                row.simple = false;
                Ok(row)
            }
            MathItem::Scripts { base, sub, sup } => self.scripts(base, sub.as_deref(), sup.as_deref(), level),
            MathItem::Fraction { numerator, denominator } => self.fraction(numerator, denominator, level),
            MathItem::Root(radicand) => self.root(radicand, level),
        }
    }

    /// A base with a subscript, a superscript, or both (TeX rules 18a-18f, display style).
    fn scripts(&mut self, base: &MathItem, sub: Option<&[MathItem]>, sup: Option<&[MathItem]>, level: u8) -> DltResult<Row> {
        let base = self.item(base, level)?;
        let m = self.metrics(level)?;
        let inner = level.saturating_add(1);
        let script_size = size_at(self.style.font_size, inner);
        let (mut up, mut down) = if base.simple {
            (0, 0)
        } else {
            (base.height.saturating_sub(part(script_size, 386, 1000)), base.depth.saturating_add(script_size / 20))
        };
        let sub = match sub {
            Some(items) => Some(self.list(items, inner)?),
            None => None,
        };
        let sup = match sup {
            Some(items) => Some(self.list(items, inner)?),
            None => None,
        };
        let xh = m.x_height();
        if let Some(p) = &sup {
            up = up.max(part(m.size, 413, 1000)).max(p.depth.saturating_add(xh / 4));
        }
        match (&sub, &sup) {
            (Some(b), None) => down = down.max(part(m.size, 15, 100)).max(b.height.saturating_sub(part(xh, 4, 5))),
            (Some(b), Some(p)) => {
                down = down.max(part(m.size, 247, 1000));
                let min_gap = part(m.rule, 4, 1);
                let gap = up.saturating_sub(p.depth).saturating_sub(b.height.saturating_sub(down));
                if gap < min_gap {
                    down = down.saturating_add(min_gap.saturating_sub(gap));
                    let lift = part(xh, 4, 5).saturating_sub(up.saturating_sub(p.depth));
                    if lift > 0 {
                        up = up.saturating_add(lift);
                        down = down.saturating_sub(lift);
                    }
                }
            }
            _ => {}
        }
        let mut row = Row::default();
        let x = base.width;
        row.append(base);
        if let Some(p) = sup {
            row.put(p, x, up.saturating_neg());
        }
        if let Some(b) = sub {
            row.put(b, x, down);
        }
        // TeX's script space after the scripts.
        row.width = row.width.saturating_add(script_size / 20);
        Ok(row)
    }

    /// `\frac`: the numerator over the denominator, both at the current size, with the bar on the math axis.
    fn fraction(&mut self, numerator: &[MathItem], denominator: &[MathItem], level: u8) -> DltResult<Row> {
        let m = self.metrics(level)?;
        let num = self.list(numerator, level)?;
        let den = self.list(denominator, level)?;
        let t = m.rule;
        let clearance = part(t, 3, 1);
        // The bar reaches a little past the wider part, and the fraction keeps a small space on each side.
        let (overhang, side) = (m.size / 12, m.size / 20);
        let bar = num.width.max(den.width).saturating_add(part(overhang, 2, 1));
        let axis = m.axis().saturating_neg();
        let top = axis.saturating_sub(t / 2);
        let bottom = top.saturating_add(t);
        let num_dy = top.saturating_sub(clearance).saturating_sub(num.depth);
        let den_dy = bottom.saturating_add(clearance).saturating_add(den.height);
        let mut row = Row::default();
        let centred = |width: i64| side.saturating_add(bar.saturating_sub(width) / 2);
        let (num_x, den_x) = (centred(num.width), centred(den.width));
        row.put(num, num_x, num_dy);
        row.put(den, den_x, den_dy);
        let style = self.stroke(t);
        row.rule(side, side.saturating_add(bar), axis, style);
        row.width = bar.saturating_add(part(side, 2, 1));
        Ok(row)
    }

    /// `\sqrt`: a radical sign as tall as the radicand, and an overbar over it.
    fn root(&mut self, radicand: &[MathItem], level: u8) -> DltResult<Row> {
        let m = self.metrics(level)?;
        let body = self.list(radicand, level)?;
        let t = m.rule;
        let clearance = t.saturating_add(m.x_height() / 4);
        // Centre line of the overbar, and the lowest point of the sign (never above the bar).
        let bar_y = body.height.saturating_add(clearance).saturating_add(t / 2).saturating_neg();
        let bottom = body.depth.max(0);
        // At least 1 EMU wide and high, so the path has an area to be drawn in at any size.
        let sign_w = (m.size / 2).max(1);
        let sign_h = bottom.saturating_sub(bar_y).max(1);
        // The stroke reaches half its width past the path, so the sign starts that far in.
        let half = t / 2 + t % 2;
        let x0 = half;
        let path = PathSpec {
            view_box: Rect { x: 0, y: 0, width: sign_w, height: sign_h },
            fill_rule: FillRule::Nonzero,
            commands: vec![
                PathCommand::MoveTo { x: 0, y: part(sign_h, 55, 100) },
                PathCommand::LineTo { x: part(sign_w, 22, 100), y: part(sign_h, 45, 100) },
                PathCommand::LineTo { x: sign_w / 2, y: sign_h },
                PathCommand::LineTo { x: sign_w, y: 0 },
            ],
        };
        let gap = m.size / 20;
        let sign_end = x0.saturating_add(sign_w);
        let body_x = sign_end.saturating_add(gap);
        let bar_end = body_x.saturating_add(body.width).saturating_add(gap);
        let mut row = Row::default();
        row.put(body, body_x, 0);
        let style = self.stroke(t);
        row.height = row.height.max(half.saturating_sub(bar_y));
        row.over = row.over.max(half.saturating_sub(bar_y));
        row.depth = row.depth.max(bottom);
        row.under = row.under.max(bottom.saturating_add(half));
        row.pieces.push(Piece::Radical { bounds: Rect { x: x0, y: bar_y, width: sign_w, height: sign_h }, path, style: style.clone() });
        row.rule(sign_end, bar_end, bar_y, style);
        row.width = bar_end.saturating_add(half);
        Ok(row)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn script_sizes_shrink_twice_and_then_stay() {
        assert_eq!(size_at(1000, 0), 1000);
        assert_eq!(size_at(1000, 1), 700);
        assert_eq!(size_at(1000, 2), 490);
        assert_eq!(size_at(1000, 8), 490);
    }

    #[test]
    fn spacing_follows_the_operator_classes() {
        assert_eq!(space_mu(Class::Ord, Class::Bin), 4);
        assert_eq!(space_mu(Class::Ord, Class::Rel), 5);
        assert_eq!(space_mu(Class::Rel, Class::Rel), 0);
        assert_eq!(space_mu(Class::Punct, Class::Ord), 3);
        assert_eq!(space_mu(Class::Ord, Class::Ord), 0);
        assert_eq!(class_of('\u{00B7}'), Class::Bin, "\\cdot is a binary operator");
        assert_eq!(class_of('\u{2192}'), Class::Rel, "\\to is a relation");
    }
}
