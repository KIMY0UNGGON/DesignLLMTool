//! Logical text projection of a paragraph.
//!
//! Text ranges count Unicode 16.0 extended grapheme clusters (UAX #29). Each protected inline atom
//! (picture, table, field, tab, line break, marker object) is exactly one grapheme and never merges
//! with a neighbouring combining mark. Ranges are `[start, end)` over this projection.
use designllm_contracts::TextRun;
use unicode_segmentation::UnicodeSegmentation;

/// Object replacement character used as the display text of inline objects.
pub const OBJECT_CHAR: &str = "\u{FFFC}";
/// Display text of a tab atom.
pub const TAB_CHAR: &str = "\t";
/// Display text of a forced line break atom.
pub const LINE_BREAK_CHAR: &str = "\n";

/// Graphemes contributed by one run.
pub fn run_graphemes(run: &TextRun) -> u32 {
    if run.protected {
        1
    } else {
        run.text.graphemes(true).count() as u32
    }
}

/// Total logical length of a paragraph.
pub fn total_graphemes(runs: &[TextRun]) -> u32 {
    runs.iter().map(run_graphemes).sum()
}

/// Grapheme ranges occupied by protected atoms, in document order.
pub fn atom_ranges(runs: &[TextRun]) -> Vec<(u32, u32)> {
    let mut at = 0u32;
    let mut out = Vec::new();
    for run in runs {
        let n = run_graphemes(run);
        if run.protected {
            out.push((at, at + n));
        }
        at += n;
    }
    out
}

/// True when `[start, end)` intersects a protected atom.
pub fn overlaps_atom(runs: &[TextRun], start: u32, end: u32) -> bool {
    atom_ranges(runs).iter().any(|&(s, e)| start < e && s < end)
}

/// Paragraph text with atoms removed (used for preview text and search).
pub fn plain_text(runs: &[TextRun]) -> String {
    runs.iter().filter(|r| !r.protected).map(|r| r.text.as_str()).collect()
}

/// Ranges of one run's graphemes mapped to byte offsets inside that run's text.
/// Protected runs are reported as the whole display text.
pub fn grapheme_byte_offsets(run: &TextRun) -> Vec<usize> {
    if run.protected {
        return vec![0, run.text.len()];
    }
    let mut offsets: Vec<usize> = run.text.grapheme_indices(true).map(|(i, _)| i).collect();
    offsets.push(run.text.len());
    offsets
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(text: &str, protected: bool) -> TextRun {
        TextRun { protected, ..TextRun::new(String::new(), text) }
    }

    #[test]
    fn protected_atoms_are_single_graphemes_even_before_combining_marks() {
        // U+FFFC followed by U+0301 must not merge into one cluster.
        let runs = vec![run(OBJECT_CHAR, true), run("\u{0301}a", false)];
        assert_eq!(total_graphemes(&runs), 3);
        assert_eq!(atom_ranges(&runs), vec![(0, 1)]);
        assert!(overlaps_atom(&runs, 0, 1));
        assert!(!overlaps_atom(&runs, 1, 3));
    }

    #[test]
    fn hangul_and_combining_sequences_count_as_graphemes_not_scalars() {
        let runs = vec![run("가e\u{0301}", false), run("👨\u{200D}👩", false)];
        // 가 (1) + e+acute (1) + family ZWJ sequence (1) = 3 graphemes, not 6 scalars.
        assert_eq!(total_graphemes(&runs), 3);
        assert_eq!(plain_text(&runs).chars().count(), 6);
    }

    #[test]
    fn plain_text_drops_atoms() {
        let runs = vec![run("가", false), run(OBJECT_CHAR, true), run("나", false)];
        assert_eq!(plain_text(&runs), "가나");
    }
}
