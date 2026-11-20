//! Logical paragraph projection (docs/CONTRACTS.md section 2).
//!
//! Rules implemented here:
//! - Text ranges count Unicode 16.0 UAX #29 extended graphemes, end exclusive.
//! - A protected run (`TextRun::protected`) is one inline atom and projects to exactly one
//!   U+FFFC grapheme, whatever its display text is. Atoms are forced grapheme boundaries.
//! - Consecutive unprotected runs are segmented jointly, so a grapheme may straddle a run
//!   boundary; such a grapheme has one piece per run.
//! - Tab and forced line break characters are ordinary graphemes (one each).
//! - The paragraph separator is not part of the range.
use designllm_contracts::{DltError, DltResult, Id, Paragraph};
use std::ops::Range;
use unicode_segmentation::UnicodeSegmentation;

/// Projection character of a protected inline atom.
pub const OBJECT_REPLACEMENT: char = '\u{FFFC}';

/// Part of a logical grapheme that lives in one run (byte range inside `TextRun::text`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunPiece {
    pub run: usize,
    pub bytes: Range<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogicalGrapheme {
    /// Byte range inside [`LogicalParagraph::text`].
    pub logical: Range<usize>,
    /// Source pieces in logical order. One piece unless the grapheme straddles runs.
    pub pieces: Vec<RunPiece>,
    /// True for a protected atom (whole run, projected as U+FFFC).
    pub protected: bool,
}

/// Logical text of a paragraph with the mapping of each grapheme to run bytes.
#[derive(Debug, Clone)]
pub struct LogicalParagraph {
    pub paragraph_id: Id,
    /// Logical projection: unprotected run text, U+FFFC per protected run.
    pub text: String,
    pub graphemes: Vec<LogicalGrapheme>,
}

impl LogicalParagraph {
    pub fn new(paragraph: &Paragraph) -> Self {
        let runs = &paragraph.runs;
        let mut text = String::new();
        let mut graphemes = Vec::new();
        let mut i = 0;
        while i < runs.len() {
            if runs[i].protected {
                let start = text.len();
                text.push(OBJECT_REPLACEMENT);
                graphemes.push(LogicalGrapheme { logical: start..text.len(), pieces: vec![RunPiece { run: i, bytes: 0..runs[i].text.len() }], protected: true });
                i += 1;
                continue;
            }
            let mut stretch = String::new();
            let mut starts: Vec<(usize, usize)> = Vec::new();
            while i < runs.len() && !runs[i].protected {
                starts.push((i, stretch.len()));
                stretch.push_str(&runs[i].text);
                i += 1;
            }
            let base = text.len();
            text.push_str(&stretch);
            for (offset, grapheme) in stretch.grapheme_indices(true) {
                let end = offset + grapheme.len();
                let mut pieces = Vec::new();
                for (k, &(run, run_start)) in starts.iter().enumerate() {
                    let run_end = starts.get(k + 1).map(|s| s.1).unwrap_or(stretch.len());
                    let a = offset.max(run_start);
                    let b = end.min(run_end);
                    if a < b {
                        pieces.push(RunPiece { run, bytes: (a - run_start)..(b - run_start) });
                    }
                }
                graphemes.push(LogicalGrapheme { logical: (base + offset)..(base + end), pieces, protected: false });
            }
        }
        Self { paragraph_id: paragraph.id.clone(), text, graphemes }
    }

    /// Number of logical graphemes.
    pub fn len(&self) -> u32 { self.graphemes.len() as u32 }
    pub fn is_empty(&self) -> bool { self.graphemes.is_empty() }

    /// Byte offset of grapheme boundary `index` (0..=len) in the logical text.
    pub fn boundary_byte(&self, index: u32) -> DltResult<usize> {
        let index = index as usize;
        if index == self.graphemes.len() { return Ok(self.text.len()); }
        self.graphemes.get(index).map(|g| g.logical.start).ok_or_else(|| out_of_range(&self.paragraph_id, index as u32, self.len()))
    }

    /// Reject reversed or out-of-range grapheme ranges.
    pub fn check_range(&self, start: u32, end: u32) -> DltResult<()> {
        if start > end { return Err(DltError::invalid(format!("Text range start {start} is after end {end} in paragraph {}", self.paragraph_id))); }
        if end > self.len() { return Err(out_of_range(&self.paragraph_id, end, self.len())); }
        Ok(())
    }

    /// True when `[start, end)` contains a protected atom.
    pub fn range_has_protected(&self, start: u32, end: u32) -> bool {
        self.graphemes.get(start as usize..end as usize).map(|gs| gs.iter().any(|g| g.protected)).unwrap_or(false)
    }

    /// Unprotected run byte spans covered by `[start, end)`, merged per run, in logical order.
    /// Fails if the range contains a protected atom (atoms cannot be edited as text).
    pub fn text_spans(&self, start: u32, end: u32) -> DltResult<Vec<RunPiece>> {
        self.check_range(start, end)?;
        if self.range_has_protected(start, end) {
            return Err(DltError::unsupported(format!("Range [{start}, {end}) of paragraph {} contains a protected inline object; text edits cannot change it", self.paragraph_id)));
        }
        Ok(self.spans_unchecked(start, end))
    }

    /// Run byte spans covered by `[start, end)` including protected atoms (whole runs).
    pub fn all_spans(&self, start: u32, end: u32) -> DltResult<Vec<RunPiece>> {
        self.check_range(start, end)?;
        Ok(self.spans_unchecked(start, end))
    }

    fn spans_unchecked(&self, start: u32, end: u32) -> Vec<RunPiece> {
        let mut spans: Vec<RunPiece> = Vec::new();
        for g in &self.graphemes[start as usize..end as usize] {
            for piece in &g.pieces {
                match spans.last_mut() {
                    Some(last) if last.run == piece.run && last.bytes.end == piece.bytes.start => last.bytes.end = piece.bytes.end,
                    _ => spans.push(piece.clone()),
                }
            }
        }
        spans
    }

    /// Run position for inserting text at boundary `index`, with upstream affinity: the end of
    /// the preceding unprotected grapheme, else the start of the following unprotected grapheme.
    /// `None` means the boundary touches only protected atoms (or the paragraph has no text);
    /// the caller must choose or create a run.
    pub fn insertion_point(&self, index: u32) -> Option<(usize, usize)> {
        let index = index as usize;
        if index > 0 {
            if let Some(g) = self.graphemes.get(index - 1) {
                if !g.protected { return g.pieces.last().map(|p| (p.run, p.bytes.end)); }
            }
        }
        if let Some(g) = self.graphemes.get(index) {
            if !g.protected { return g.pieces.first().map(|p| (p.run, p.bytes.start)); }
        }
        None
    }

    /// Logical grapheme index of the first grapheme of `run`, or the boundary where an empty
    /// run sits.
    pub fn run_start_index(&self, run: usize) -> u32 {
        self.graphemes.iter().position(|g| g.pieces.iter().any(|p| p.run >= run)).unwrap_or(self.graphemes.len()) as u32
    }

    /// Grapheme range `[start, end)` that a run covers (empty runs give an empty range).
    pub fn run_range(&self, run: usize) -> (u32, u32) {
        let start = self.graphemes.iter().position(|g| g.pieces.iter().any(|p| p.run == run));
        match start {
            None => { let at = self.run_start_index(run); (at, at) }
            Some(s) => {
                let end = self.graphemes.iter().rposition(|g| g.pieces.iter().any(|p| p.run == run)).unwrap_or(s) + 1;
                (s as u32, end as u32)
            }
        }
    }
}

fn out_of_range(paragraph: &str, index: u32, len: u32) -> DltError {
    DltError::invalid(format!("Grapheme offset {index} is out of range for paragraph {paragraph} ({len} graphemes)"))
}

/// Logical projection text of a paragraph.
pub fn logical_text(paragraph: &Paragraph) -> String { LogicalParagraph::new(paragraph).text }

/// Number of logical graphemes in a paragraph.
pub fn grapheme_count(paragraph: &Paragraph) -> u32 { LogicalParagraph::new(paragraph).len() }

/// Text accepted by text edits: no U+FFFC (atoms are not typed), and no control characters
/// other than tab and line feed.
pub fn validate_edit_text(text: &str) -> DltResult<()> {
    if let Some(c) = text.chars().find(|&c| c == OBJECT_REPLACEMENT || (c.is_control() && c != '\t' && c != '\n')) {
        return Err(DltError::invalid(format!("Text contains unsupported character U+{:04X}", c as u32)));
    }
    Ok(())
}

/// Mandatory line break graphemes inside a paragraph.
pub fn is_forced_break(grapheme: &str) -> bool {
    matches!(grapheme, "\n" | "\r\n" | "\r" | "\u{000B}" | "\u{000C}" | "\u{0085}" | "\u{2028}" | "\u{2029}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use designllm_contracts::{ParagraphFormat, TextRun};

    fn run(id: &str, text: &str, protected: bool) -> TextRun { TextRun { protected, ..TextRun::new(id, text) } }
    fn para(runs: Vec<TextRun>) -> Paragraph { Paragraph { id: "p".into(), runs, style_ref: None, format: ParagraphFormat::default() } }

    #[test]
    fn atoms_are_one_grapheme_and_runs_join() {
        let p = para(vec![run("a", "A한e", false), run("b", "\u{0301}B", false), run("f", "{ PAGE 12 }", true), run("c", "\tx\n", false)]);
        let lp = LogicalParagraph::new(&p);
        assert_eq!(lp.text, "A한e\u{0301}B\u{FFFC}\tx\n");
        assert_eq!(lp.len(), 8);
        // e + combining acute straddles runs a and b
        assert_eq!(lp.graphemes[2].pieces, vec![RunPiece { run: 0, bytes: 4..5 }, RunPiece { run: 1, bytes: 0..2 }]);
        assert!(lp.graphemes[4].protected);
        assert_eq!(lp.graphemes[4].pieces, vec![RunPiece { run: 2, bytes: 0..11 }]);
        assert!(lp.text_spans(3, 6).is_err());
        assert_eq!(lp.text_spans(5, 7).unwrap(), vec![RunPiece { run: 3, bytes: 0..2 }]);
        assert_eq!(lp.insertion_point(5), Some((3, 0)));
        assert_eq!(lp.insertion_point(4), Some((1, 3)));
        assert!(lp.check_range(0, 9).is_err());
        assert!(lp.check_range(3, 2).is_err());
        assert_eq!(lp.run_range(2), (4, 5));
        // contract helper agrees on the plain stretch
        assert_eq!(designllm_contracts::grapheme_to_byte(&lp.text, 4).unwrap(), lp.boundary_byte(4).unwrap());
    }

    #[test]
    fn edit_text_rejects_atoms_and_controls() {
        assert!(validate_edit_text("a\tb\nc").is_ok());
        assert!(validate_edit_text("a\u{FFFC}").is_err());
        assert!(validate_edit_text("a\rb").is_err());
    }
}
