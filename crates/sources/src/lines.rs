//! Groups glyphs into visual lines in content-stream order. Each line becomes one evidence entry.
//!
//! Numbers. A group is formed by the stream rule as it was before task 49m: a glyph joins while its baseline is within half
//! the largest font size of the group so far, it moves forward, and its gap to the previous glyph is at most 6 font sizes.
//! The groups number the lines of a page. A group that the cuts below do not divide keeps its number and its text. A group
//! that they divide gives all its non-empty pieces new numbers after the last group of the page.
//!
//! Order inside a line. Lines are cut in stream order; the glyphs of each line are then read in x order, so a glyph drawn right
//! to left, up to one font size before its predecessor, is read in its place.
//!
//! Cuts inside a group. A gap is judged with the larger font size of the two non-space glyphs around it. A space glyph has no
//! gap of its own: its width counts into the gap. A gap wider than 1.5 font sizes starts a new line. A glyph whose baseline is
//! more than half the larger of its own size and the line's main size away from the main baseline starts a new line. The main
//! glyph is the largest glyph of the line so far, so a small superscript at the start does not set the baseline.
//!
//! Counted. A joined gap wider than 0.6 font size is counted: at that width a column gap and a word space are not told apart.
//!
//! Limit. Two columns are cut only by a gap wider than 1.5 font sizes. Glyphs placed in the column gap, so that every gap
//! between neighbours is 1.5 font sizes or less, join the two columns into one line. Gaps of 0.6 font size or less are not
//! counted, so such a mix can come without a diagnostic. It cannot be told apart: a letter one space from each column sits
//! where the same letter of one genuine line sits. The region of a mixed line is the union of its glyphs, so it spans both
//! columns and still names what was read.

use crate::content::RawGlyph;
use designllm_contracts::Diagnostic;
use std::ops::Range;

/// A group joins glyphs whose gap is at most this many font sizes, as before task 49m. It numbers the lines.
const GROUP_MAX_GAP_FS: f64 = 6.0;
/// A glyph's baseline may differ from the line's main baseline by at most this many font sizes.
const BASELINE_TOLERANCE_FS: f64 = 0.5;
/// A gap wider than this many font sizes starts a new line.
const JOIN_MAX_GAP_FS: f64 = 1.5;
/// A joined gap wider than this many font sizes is counted.
const AMBIGUOUS_GAP_FS: f64 = 0.6;
/// A gap wider than this many font sizes, measured with the glyph after it, gets a space in the text.
const SPACE_GAP_FS: f64 = 0.2;
/// Code of the non-blocking diagnostic for joined gaps wider than `AMBIGUOUS_GAP_FS`.
pub(crate) const AMBIGUOUS_GAP_CODE: &str = "PDF_LINE_GAP_AMBIGUOUS";

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Line {
    pub text: String,
    /// Union of glyph cells in user space: x0, y0, x1, y1. The spaces that end a line before a cut are not in it.
    pub bbox: [f64; 4],
    /// The number of the line in its evidence id, `pdf-<hash>-p<page>-l<index>`.
    pub index: usize,
    /// Joined gaps of this line wider than `AMBIGUOUS_GAP_FS`.
    pub ambiguous_gaps: u32,
}

/// The larger of two font sizes. A gap is judged with it: the larger size of the two glyphs around the gap.
fn larger(a: f64, b: f64) -> f64 {
    a.max(b).max(1e-9)
}

/// Slack on both gap limits, in font sizes. A gap equal to a limit is not above it even when rounding adds a last-place
/// error: a Courier space of 0.6 font size at 12 pt otherwise counted as above 0.6.
const LIMIT_SLACK_FS: f64 = 1e-9;

/// How one gap is judged. `ambiguous`: wider than `AMBIGUOUS_GAP_FS`. `cut`: wider than `JOIN_MAX_GAP_FS`.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Judged {
    ambiguous: bool,
    cut: bool,
}

fn judge(gap: f64, size: f64) -> Judged {
    let slack = LIMIT_SLACK_FS * size;
    Judged {
        ambiguous: gap > AMBIGUOUS_GAP_FS * size + slack,
        cut: gap > JOIN_MAX_GAP_FS * size + slack,
    }
}

/// The gap from the line's edge `reference` to the glyph `g`, and how it is judged. `Current::accepts` and `add` both use it.
fn judged_gap(reference: Reference, g: &RawGlyph) -> (f64, Judged) {
    let gap = g.cell[0] - reference.x1;
    (gap, judge(gap, larger(reference.size, g.font_size)))
}

/// A space glyph: its text is only whitespace. An empty text is an ordinary glyph.
fn is_space(g: &RawGlyph) -> bool {
    !g.text.is_empty() && g.text.chars().all(char::is_whitespace)
}

fn union(bbox: [f64; 4], cell: [f64; 4]) -> [f64; 4] {
    [bbox[0].min(cell[0]), bbox[1].min(cell[1]), bbox[2].max(cell[2]), bbox[3].max(cell[3])]
}

/// The groups of the stream rule before task 49m: the font size of a group is the largest of the group so far, and the
/// baseline and the gap are compared with the previous glyph. These groups number the lines of a page.
fn old_groups(glyphs: &[RawGlyph]) -> Vec<Range<usize>> {
    let mut groups: Vec<Range<usize>> = Vec::new();
    let mut group_size = 0.0_f64;
    for (i, g) in glyphs.iter().enumerate() {
        let joins = groups.last().is_some_and(|range| {
            let last = &glyphs[range.end - 1];
            let fs = group_size.max(g.font_size).max(1e-9);
            (g.origin_y - last.origin_y).abs() <= 0.5 * fs
                && g.origin_x >= last.origin_x - fs
                && g.cell[0] - last.cell[2] <= GROUP_MAX_GAP_FS * fs
        });
        if joins {
            if let Some(range) = groups.last_mut() {
                range.end = i + 1;
            }
            group_size = group_size.max(g.font_size);
        } else {
            groups.push(i..i + 1);
            group_size = g.font_size;
        }
    }
    groups
}

/// The furthest right edge of the non-space glyphs of a line, and the size of the last of them. The gap to the next glyph is
/// measured from the edge.
#[derive(Debug, Clone, Copy)]
struct Reference {
    x1: f64,
    size: f64,
}

/// The main glyph of a line: the largest glyph so far. Its baseline is the line's baseline.
#[derive(Debug, Clone, Copy)]
struct Main {
    y: f64,
    size: f64,
}

/// A line under construction. A space waits in `pending` until a glyph that is not a space follows it in the same line, so a
/// line cut at a gap does not end with the spaces of that gap. The last line of a group keeps its trailing spaces.
#[derive(Default)]
struct Current {
    text: String,
    bbox: Option<[f64; 4]>,
    pending: String,
    pending_bbox: Option<[f64; 4]>,
    reference: Option<Reference>,
    main: Option<Main>,
    ambiguous_gaps: u32,
}

impl Current {
    /// Whether `g` continues this line. A space always does.
    fn accepts(&self, g: &RawGlyph) -> bool {
        if is_space(g) {
            return true;
        }
        let (Some(reference), Some(main)) = (self.reference, self.main) else {
            return true;
        };
        let (_, judged) = judged_gap(reference, g);
        !judged.cut && (g.origin_y - main.y).abs() <= BASELINE_TOLERANCE_FS * larger(main.size, g.font_size)
    }

    /// Adds `g`, which `accepts` took or which starts this line.
    fn add(&mut self, g: &RawGlyph) {
        if is_space(g) {
            self.pending.push_str(&g.text);
            self.pending_bbox = Some(self.pending_bbox.map_or(g.cell, |bbox| union(bbox, g.cell)));
            return;
        }
        match self.reference {
            Some(reference) => {
                let (gap, judged) = judged_gap(reference, g);
                if judged.ambiguous {
                    self.ambiguous_gaps += 1;
                }
                self.commit();
                // The space before a glyph is decided with the glyph's own size, as before task 49m.
                let starts_with_space = g.text.starts_with(char::is_whitespace);
                if gap > SPACE_GAP_FS * g.font_size.max(1e-9)
                    && !self.text.ends_with(char::is_whitespace)
                    && !starts_with_space
                {
                    self.text.push(' ');
                }
            }
            None => self.commit(),
        }
        self.text.push_str(&g.text);
        self.bbox = Some(self.bbox.map_or(g.cell, |bbox| union(bbox, g.cell)));
        if self.main.is_none_or(|main| g.font_size > main.size) {
            self.main = Some(Main { y: g.origin_y, size: g.font_size });
        }
        // The edge is the furthest one of the line, so a glyph drawn after a wider one does not move the edge back.
        let x1 = self.reference.map_or(g.cell[2], |reference| reference.x1.max(g.cell[2]));
        self.reference = Some(Reference { x1, size: g.font_size });
    }

    /// Moves the pending spaces into the line.
    fn commit(&mut self) {
        self.text.push_str(&self.pending);
        self.pending.clear();
        if let Some(pending) = self.pending_bbox.take() {
            self.bbox = Some(self.bbox.map_or(pending, |bbox| union(bbox, pending)));
        }
    }

    fn finish(mut self, last: bool) -> Line {
        if last {
            self.commit();
        }
        Line { text: self.text, bbox: self.bbox.unwrap_or([0.0; 4]), index: 0, ambiguous_gaps: self.ambiguous_gaps }
    }
}

/// Cuts one group into pieces in stream order. Each piece holds the glyphs of one line, still in stream order.
fn cut<'a>(glyphs: &[&'a RawGlyph]) -> Vec<Vec<&'a RawGlyph>> {
    let mut pieces: Vec<Vec<&RawGlyph>> = Vec::new();
    let mut probe = Current::default();
    for &g in glyphs {
        let fits = !pieces.is_empty() && probe.accepts(g);
        if !fits {
            pieces.push(Vec::new());
            probe = Current::default();
        }
        probe.add(g);
        if let Some(piece) = pieces.last_mut() {
            piece.push(g);
        }
    }
    pieces
}

/// Builds the line of one piece. Its glyphs are read in x order. The cut is already decided, so none is cut here.
fn read_line(mut glyphs: Vec<&RawGlyph>, last: bool) -> Line {
    // Adding +0.0 turns -0.0 into +0.0, so the two positions compare equal. The sort is stable, so glyphs at the same x keep
    // the stream order of the piece.
    glyphs.sort_by(|a, b| (a.origin_x + 0.0).total_cmp(&(b.origin_x + 0.0)));
    let mut line = Current::default();
    for g in glyphs {
        line.add(g);
    }
    line.finish(last)
}

/// The lines of a page in reading order, each with its number. See the module notes on numbers.
pub(crate) fn group_lines(glyphs: &[RawGlyph]) -> Vec<Line> {
    let groups = old_groups(glyphs);
    // The first new number is the number after the last group. The pieces of cut groups take the numbers from here on.
    let mut next = groups.len();
    let mut lines = Vec::new();
    for (index, range) in groups.into_iter().enumerate() {
        // The lines are cut in stream order, as before. The glyphs of each line are then read in x order.
        let refs: Vec<&RawGlyph> = glyphs[range].iter().collect();
        let cut_pieces = cut(&refs);
        let last = cut_pieces.len().saturating_sub(1);
        let mut pieces: Vec<Line> =
            cut_pieces.into_iter().enumerate().map(|(k, piece)| read_line(piece, k == last)).collect();
        if pieces.len() == 1 {
            if let Some(mut line) = pieces.pop() {
                line.index = index;
                lines.push(line);
            }
            continue;
        }
        pieces.retain(|piece| !piece.text.trim().is_empty());
        for mut line in pieces {
            line.index = next;
            next += 1;
            lines.push(line);
        }
    }
    lines
}

/// The diagnostic of a page with gaps or glyphs that cannot be judged, with their count. None when there are none.
pub(crate) fn gap_diagnostic(lines: &[Line]) -> Option<Diagnostic> {
    let count = lines.iter().fold(0_u32, |sum, line| sum.saturating_add(line.ambiguous_gaps));
    (count > 0).then(|| {
        Diagnostic {
            code: AMBIGUOUS_GAP_CODE.into(),
            message: format!(
                "{count} gap(s) of 0.6 to 1.5 font sizes were joined; a column gap of that width is not told apart from a word space"
            ),
            node_id: None,
            blocking: false,
            params: Default::default(),
        }
        .with_param("count", count)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn glyph(text: &str, ox: f64, oy: f64, width: f64, size: f64) -> RawGlyph {
        RawGlyph {
            text: text.into(),
            origin_x: ox,
            origin_y: oy,
            cell: [ox, oy - 0.2 * size, ox + width, oy + 0.8 * size],
            font_size: size,
            render_mode: 0,
            clipped: false,
            width_known: true,
        }
    }

    /// One glyph per letter of a Courier-like run: each letter is 0.6 font size wide, starting at `x`.
    fn letters(text: &str, x: f64, y: f64, size: f64) -> Vec<RawGlyph> {
        text.chars()
            .enumerate()
            .map(|(i, c)| glyph(&c.to_string(), x + 0.6 * size * i as f64, y, 0.6 * size, size))
            .collect()
    }

    fn texts(lines: &[Line]) -> Vec<&str> {
        lines.iter().map(|line| line.text.as_str()).collect()
    }

    fn counted(lines: &[Line]) -> u32 {
        lines.iter().map(|line| line.ambiguous_gaps).sum()
    }

    #[test]
    fn same_baseline_glyphs_form_one_line_and_baseline_change_splits() {
        let glyphs = vec![
            glyph("A", 10.0, 100.0, 7.0, 12.0),
            glyph("B", 17.0, 100.0, 7.0, 12.0),
            glyph("C", 10.0, 80.0, 7.0, 12.0),
        ];
        let lines = group_lines(&glyphs);
        assert_eq!(texts(&lines), ["AB", "C"]);
    }

    #[test]
    fn large_gap_inserts_one_space_but_explicit_space_is_not_doubled() {
        let glyphs = vec![
            glyph("A", 0.0, 0.0, 6.0, 10.0),
            glyph("B", 9.0, 0.0, 6.0, 10.0), // gap 3 > 0.2 * 10
            glyph(" ", 15.0, 0.0, 3.0, 10.0),
            glyph("C", 18.0, 0.0, 6.0, 10.0), // no gap after explicit space
        ];
        let lines = group_lines(&glyphs);
        assert_eq!(lines[0].text, "A B C");
    }

    #[test]
    fn bbox_is_union_of_glyph_cells() {
        let glyphs = vec![glyph("A", 0.0, 0.0, 6.0, 10.0), glyph("B", 6.0, 0.0, 6.0, 10.0)];
        let lines = group_lines(&glyphs);
        assert_eq!(lines[0].bbox, [0.0, -2.0, 12.0, 8.0]);
    }

    #[test]
    fn a_half_font_size_gap_is_joined_without_a_count() {
        let glyphs = vec![glyph("A", 0.0, 0.0, 6.0, 10.0), glyph("B", 11.0, 0.0, 6.0, 10.0)];
        let lines = group_lines(&glyphs);
        assert_eq!(texts(&lines), ["A B"]);
        assert_eq!(counted(&lines), 0);
    }

    #[test]
    fn a_gap_of_one_and_a_half_font_sizes_stays_in_the_line_and_is_counted() {
        let glyphs = vec![glyph("A", 0.0, 0.0, 6.0, 10.0), glyph("B", 21.0, 0.0, 6.0, 10.0)];
        let lines = group_lines(&glyphs);
        assert_eq!(texts(&lines), ["A B"]);
        assert_eq!(counted(&lines), 1);
    }

    #[test]
    fn a_gap_above_one_and_a_half_font_sizes_starts_a_new_line() {
        let glyphs = vec![glyph("A", 0.0, 0.0, 6.0, 10.0), glyph("B", 21.5, 0.0, 6.0, 10.0)];
        let lines = group_lines(&glyphs);
        assert_eq!(texts(&lines), ["A", "B"]);
        assert_eq!(counted(&lines), 0);
    }

    #[test]
    fn cut_pieces_take_new_numbers_and_the_other_lines_keep_theirs() {
        let glyphs = vec![
            glyph("A", 0.0, 100.0, 6.0, 10.0),
            glyph("B", 20.0, 80.0, 6.0, 10.0),
            glyph("C", 46.0, 80.0, 6.0, 10.0), // gap 20 = 2 font sizes: cut
            glyph("D", 0.0, 60.0, 6.0, 10.0),
        ];
        let lines = group_lines(&glyphs);
        let numbered: Vec<(&str, usize)> = lines.iter().map(|line| (line.text.as_str(), line.index)).collect();
        assert_eq!(numbered, vec![("A", 0), ("B", 3), ("C", 4), ("D", 2)]);
    }

    #[test]
    fn a_large_glyph_does_not_raise_the_limits_of_the_small_text() {
        // X is 30 pt and ends 50 pt before the 10 pt text. The gap is judged with the larger of its two glyphs, 30 pt:
        // 50 > 45, so it is cut. The 30 pt gap between the 10 pt glyphs A and B is judged with 10 pt and is cut too.
        let glyphs = vec![
            glyph("X", 4.0, 700.0, 18.0, 30.0),
            glyph("A", 72.0, 700.0, 6.0, 10.0),
            glyph("B", 108.0, 700.0, 6.0, 10.0),
        ];
        let lines = group_lines(&glyphs);
        assert_eq!(texts(&lines), ["X", "A", "B"]);
        assert_eq!(counted(&lines), 0);
    }

    #[test]
    fn a_space_glyph_adds_its_width_to_the_gap() {
        // A, a 10 pt space, then B: the gap from A to B is 16 pt = 1.6 font sizes, so the line is cut and the space is left out.
        let glyphs = vec![
            glyph("A", 0.0, 0.0, 6.0, 10.0),
            glyph(" ", 6.0, 0.0, 10.0, 10.0),
            glyph("B", 22.0, 0.0, 6.0, 10.0),
        ];
        let lines = group_lines(&glyphs);
        assert_eq!(texts(&lines), ["A", "B"]);
        assert_eq!(lines[0].bbox, [0.0, -2.0, 6.0, 8.0]);
    }

    #[test]
    fn limit_letter_in_a_gap_joins_its_line_silently() {
        // A, then X (15 pt, 9 pt wide) with 6 pt on each side, then B. Each gap is 0.4 font size of the larger glyph: not cut,
        // not counted. A letter placed in the gap between two words joins the line.
        let glyphs = vec![
            glyph("A", 0.0, 0.0, 6.0, 10.0),
            glyph("X", 12.0, 0.0, 9.0, 15.0),
            glyph("B", 27.0, 0.0, 6.0, 10.0),
        ];
        let lines = group_lines(&glyphs);
        assert_eq!(texts(&lines), ["A X B"]);
        assert_eq!(counted(&lines), 0);
    }

    #[test]
    fn limit_space_wrapped_letter_between_columns_joins_the_line() {
        // L, a 2 pt space, X, a 4 pt space, R. The gaps L-X and X-R are 6 pt, which is 0.6 font size and not above it, so
        // nothing is cut or counted: the letter joins the line.
        let glyphs = vec![
            glyph("L", -6.0, 0.0, 6.0, 10.0),
            glyph(" ", 2.0, 0.0, 4.0, 10.0),
            glyph("X", 6.0, 0.0, 6.0, 10.0),
            glyph(" ", 12.0, 0.0, 4.0, 10.0),
            glyph("R", 18.0, 0.0, 6.0, 10.0),
        ];
        let lines = group_lines(&glyphs);
        assert_eq!(texts(&lines), ["L X R"]);
        assert_eq!(counted(&lines), 0);
    }

    #[test]
    fn limit_two_letters_in_a_gap_join_the_line_and_are_counted() {
        // L, 7 pt, X, 7 pt, Y, 7 pt, R: every gap is 0.7 font size, so the line is joined and each gap is counted.
        let glyphs = vec![
            glyph("L", 0.0, 0.0, 6.0, 10.0),
            glyph("X", 13.0, 0.0, 6.0, 10.0),
            glyph("Y", 26.0, 0.0, 6.0, 10.0),
            glyph("R", 39.0, 0.0, 6.0, 10.0),
        ];
        let lines = group_lines(&glyphs);
        assert_eq!(texts(&lines), ["L X Y R"]);
        assert_eq!(counted(&lines), 3);
    }

    #[test]
    fn limit_exact_boundary_two_letters_join_silently() {
        // The review's numbers: 6, 3, 6, 3, 6 pt. Every gap is exactly 0.6 font size, which is not above 0.6, so nothing is
        // cut and nothing is counted. A known limit of the rule.
        let glyphs = vec![
            glyph("L", -6.0, 0.0, 6.0, 10.0),
            glyph("X", 6.0, 0.0, 3.0, 10.0),
            glyph("Y", 15.0, 0.0, 3.0, 10.0),
            glyph("R", 24.0, 0.0, 6.0, 10.0),
        ];
        let lines = group_lines(&glyphs);
        assert_eq!(texts(&lines), ["L X Y R"]);
        assert_eq!(counted(&lines), 0);
    }

    #[test]
    fn normal_courier_text_with_a_one_letter_word_stays_one_line() {
        // "hello a world" in Courier 10 pt. Every space is 0.6 font size, which is not above 0.6: no cut, no count.
        let glyphs = letters("hello a world", 0.0, 0.0, 10.0);
        let lines = group_lines(&glyphs);
        assert_eq!(texts(&lines), ["hello a world"]);
        assert_eq!(counted(&lines), 0);
    }

    #[test]
    fn normal_fixed_width_digits_stay_one_line() {
        // "1 2 3" as a fixed-width table row, one cell per glyph.
        let glyphs = letters("1 2 3", 0.0, 0.0, 10.0);
        let lines = group_lines(&glyphs);
        assert_eq!(texts(&lines), ["1 2 3"]);
        assert_eq!(counted(&lines), 0);
    }

    #[test]
    fn normal_korean_one_syllable_word_stays_one_line() {
        // "앞 수 뒤": 1.0 font size glyphs with 0.3 font size spaces. The one-syllable word is not set apart.
        let glyphs = vec![
            glyph("앞", 0.0, 0.0, 10.0, 10.0),
            glyph(" ", 10.0, 0.0, 3.0, 10.0),
            glyph("수", 13.0, 0.0, 10.0, 10.0),
            glyph(" ", 23.0, 0.0, 3.0, 10.0),
            glyph("뒤", 26.0, 0.0, 10.0, 10.0),
        ];
        let lines = group_lines(&glyphs);
        assert_eq!(texts(&lines), ["앞 수 뒤"]);
        assert_eq!(counted(&lines), 0);
    }

    #[test]
    fn normal_dash_between_spaces_stays_one_line() {
        // "A — B" in a proportional font: a 1.0 font size dash with spaces of 0.278 font size.
        let glyphs = vec![
            glyph("A", 0.0, 0.0, 6.67, 10.0),
            glyph(" ", 6.67, 0.0, 2.78, 10.0),
            glyph("\u{2014}", 9.45, 0.0, 10.0, 10.0),
            glyph(" ", 19.45, 0.0, 2.78, 10.0),
            glyph("B", 22.23, 0.0, 6.67, 10.0),
        ];
        let lines = group_lines(&glyphs);
        assert_eq!(texts(&lines), ["A \u{2014} B"]);
        assert_eq!(counted(&lines), 0);
    }

    #[test]
    fn normal_justified_one_letter_word_stays_one_line() {
        // A one-letter word with word gaps of 0.55 font size on both sides, so the neighbours are 1.55 font sizes apart.
        // Neither gap is above 0.6, so nothing is cut or counted.
        let glyphs = vec![
            glyph("x", 0.0, 0.0, 6.0, 10.0),
            glyph("a", 11.5, 0.0, 4.5, 10.0),
            glyph("y", 21.5, 0.0, 6.0, 10.0),
        ];
        let lines = group_lines(&glyphs);
        assert_eq!(texts(&lines), ["x a y"]);
        assert_eq!(counted(&lines), 0);
    }

    #[test]
    fn a_baseline_drift_of_half_a_font_size_per_glyph_is_split() {
        // Each glyph is 5 pt (0.5 font size) lower than the one before. Compared with the line's main glyph, the drift
        // reaches 10 pt after two glyphs, so the staircase is cut into lines of two glyphs.
        let glyphs: Vec<RawGlyph> = "ABCDEF"
            .chars()
            .enumerate()
            .map(|(i, c)| glyph(&c.to_string(), 72.0 + 6.0 * i as f64, 700.0 - 5.0 * i as f64, 6.0, 10.0))
            .collect();
        let lines = group_lines(&glyphs);
        assert_eq!(texts(&lines), ["AB", "CD", "EF"]);
    }

    #[test]
    fn jitter_within_half_a_font_size_stays_in_one_line() {
        let glyphs = vec![
            glyph("A", 72.0, 700.0, 6.0, 10.0),
            glyph("B", 78.0, 704.9, 6.0, 10.0),
            glyph("C", 84.0, 700.0, 6.0, 10.0),
            glyph("D", 90.0, 704.9, 6.0, 10.0),
        ];
        let lines = group_lines(&glyphs);
        assert_eq!(texts(&lines), ["ABCD"]);
    }

    #[test]
    fn first_and_middle_subscripts_stay_in_the_line() {
        // A 6 pt superscript first, then 10 pt text with a 6 pt subscript in the middle.
        let glyphs = vec![
            glyph("1", 0.0, 703.5, 3.6, 6.0),
            glyph("a", 3.6, 700.0, 6.0, 10.0),
            glyph("b", 9.6, 700.0, 6.0, 10.0),
            glyph("2", 15.6, 697.0, 3.6, 6.0),
            glyph("c", 19.2, 700.0, 6.0, 10.0),
        ];
        let lines = group_lines(&glyphs);
        assert_eq!(texts(&lines), ["1ab2c"]);
        assert_eq!(counted(&lines), 0);
    }

    #[test]
    fn a_large_word_next_to_a_footnote_keeps_its_word_spaces() {
        // "Big Word" at 36 pt with a 10 pt word space, and a 6 pt footnote 2 pt after the word.
        let glyphs = vec![
            glyph("Big", 0.0, 0.0, 64.8, 36.0),
            glyph("Word", 74.8, 0.0, 86.4, 36.0),
            glyph("1", 163.2, 0.0, 3.6, 6.0),
        ];
        let lines = group_lines(&glyphs);
        assert_eq!(texts(&lines), ["Big Word 1"]);
        assert_eq!(counted(&lines), 0);
    }

    #[test]
    fn justified_word_gaps_are_joined_and_counted() {
        // Word gaps of 7 pt (0.7 font size) in a 10 pt line. Every word is drawn letter by letter.
        let mut glyphs = letters("The", 0.0, 0.0, 10.0);
        glyphs.extend(letters("quick", 25.0, 0.0, 10.0));
        glyphs.extend(letters("brown", 62.0, 0.0, 10.0));
        glyphs.extend(letters("fox", 99.0, 0.0, 10.0));
        let lines = group_lines(&glyphs);
        assert_eq!(texts(&lines), ["The quick brown fox"]);
        assert_eq!(counted(&lines), 3);
    }

    #[test]
    fn a_justified_word_gap_above_one_and_a_half_font_sizes_cuts_the_line() {
        // "The" then a 16 pt (1.6 font sizes) gap, then "quick brown" with a 7 pt gap.
        let mut glyphs = letters("The", 0.0, 0.0, 10.0);
        glyphs.extend(letters("quick", 34.0, 0.0, 10.0));
        glyphs.extend(letters("brown", 71.0, 0.0, 10.0));
        let lines = group_lines(&glyphs);
        assert_eq!(texts(&lines), ["The", "quick brown"]);
        assert_eq!(counted(&lines), 1);
    }

    #[test]
    fn a_combining_mark_drawn_last_does_not_open_a_space() {
        // The mark has no width and starts 1 pt into "a". Read in x order it comes after "a", and it must not set the
        // right edge of the line to 1 pt: the gap to "b" would then be 5.5 pt and a space would be put in.
        let glyphs = vec![
            glyph("\u{0301}", 1.0, 0.0, 0.0, 10.0),
            glyph("a", 0.0, 0.0, 6.0, 10.0),
            glyph("b", 6.5, 0.0, 6.0, 10.0),
        ];
        let lines = group_lines(&glyphs);
        assert_eq!(texts(&lines), ["a\u{0301}b"]);
    }

    #[test]
    fn a_staircase_keeps_the_line_cuts_of_the_stream_order() {
        // A, then B 5 pt lower and 6 pt to the left, then C 5 pt lower again. C is 10 pt below A, so it is cut as before.
        // The line [A, B] is read in x order, so B comes first.
        let glyphs = vec![
            glyph("A", 72.0, 700.0, 6.0, 10.0),
            glyph("B", 66.0, 695.0, 6.0, 10.0),
            glyph("C", 60.0, 690.0, 6.0, 10.0),
        ];
        let lines = group_lines(&glyphs);
        assert_eq!(texts(&lines), ["BA", "C"]);
    }

    #[test]
    fn a_column_drawn_backwards_within_one_font_size_is_read_in_x_order() {
        // "Lf" ends at 12 pt. The column is drawn b first (starts at 36 pt), then a (starts at 30 pt, 6 pt left of b). The gap
        // from f to b is 24 pt = 2.4 font sizes, so the cut is made before b in stream order. a is 12 pt left of b's edge at
        // 42 pt, so it joins the column, and the column reads "ab" in x order.
        let glyphs = vec![
            glyph("L", 0.0, 0.0, 6.0, 10.0),
            glyph("f", 6.0, 0.0, 6.0, 10.0),
            glyph("b", 36.0, 0.0, 6.0, 10.0),
            glyph("a", 30.0, 0.0, 6.0, 10.0),
        ];
        let lines = group_lines(&glyphs);
        assert_eq!(texts(&lines), ["Lf", "ab"]);
    }

    #[test]
    fn a_glyph_drawn_after_a_positive_zero_origin_keeps_the_stream_order_at_negative_zero() {
        // A is drawn at +0.0 and B at -0.0 with the same x. total_cmp would put B first; the stream order must hold.
        let glyphs = vec![
            glyph("A", 0.0, 0.0, 6.0, 10.0),
            glyph("B", -0.0, 0.0, 6.0, 10.0),
            glyph("C", 6.0, 0.0, 6.0, 10.0),
        ];
        let lines = group_lines(&glyphs);
        assert_eq!(texts(&lines), ["ABC"]);
    }

    #[test]
    fn a_backward_glyph_inside_a_cut_gap_does_not_close_the_gap_limit() {
        // P at 0 pt, then G at 22 pt: the gap 16 pt is above 1.5 font sizes, so G starts a new line. Q at 16 pt is drawn
        // later and lies in that gap, but the cut was made in stream order. Q joins G's line and reads before it.
        // Limit: a line boundary is decided by stream order; a glyph drawn later inside the gap does not close it (49za).
        let glyphs = vec![
            glyph("P", 0.0, 0.0, 6.0, 10.0),
            glyph("G", 22.0, 0.0, 6.0, 10.0),
            glyph("Q", 16.0, 0.0, 6.0, 10.0),
        ];
        let lines = group_lines(&glyphs);
        assert_eq!(texts(&lines), ["P", "QG"]);
    }

    #[test]
    fn diagnostic_counts_the_joined_ambiguous_gaps_of_the_page() {
        assert!(gap_diagnostic(&[]).is_none());
        let line = |ambiguous_gaps| Line { text: String::new(), bbox: [0.0; 4], index: 0, ambiguous_gaps };
        let diagnostic = gap_diagnostic(&[line(2), line(0), line(1)]).expect("three gaps");
        assert_eq!(diagnostic.code, AMBIGUOUS_GAP_CODE);
        assert!(!diagnostic.blocking);
        assert_eq!(diagnostic.params.get("count").and_then(|value| value.as_u64()), Some(3));
    }
}
