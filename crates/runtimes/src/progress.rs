//! Bounded buffer of runtime stdout/stderr lines. Progress text is for display only. Nothing parsed from
//! it is applied to an artifact; changes arrive only through the job-bound MCP connection.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};

/// Lines kept per stream. Older lines are dropped and counted.
pub const MAX_LINES: usize = 500;
/// Characters kept per line.
pub const MAX_LINE_CHARS: usize = 4096;

#[derive(Debug, Default)]
pub struct ProgressBuffer {
    lines: Mutex<VecDeque<String>>,
    dropped: AtomicU64,
}

impl ProgressBuffer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends one line, truncated to [`MAX_LINE_CHARS`].
    pub fn push(&self, line: &str) {
        let text: String = line.chars().take(MAX_LINE_CHARS).collect();
        let mut lines = self.lines.lock().unwrap_or_else(PoisonError::into_inner);
        if lines.len() >= MAX_LINES {
            lines.pop_front();
            self.dropped.fetch_add(1, Ordering::SeqCst);
        }
        lines.push_back(text);
    }

    /// Copy of the retained lines, oldest first.
    pub fn snapshot(&self) -> Vec<String> {
        self.lines.lock().unwrap_or_else(PoisonError::into_inner).iter().cloned().collect()
    }

    /// Number of lines dropped because the buffer was full.
    pub fn dropped_lines(&self) -> u64 {
        self.dropped.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_the_newest_lines_and_counts_drops() {
        let buf = ProgressBuffer::new();
        for i in 0..(MAX_LINES + 3) {
            buf.push(&format!("line {i}"));
        }
        let lines = buf.snapshot();
        assert_eq!(lines.len(), MAX_LINES);
        assert_eq!(lines[0], "line 3");
        assert_eq!(buf.dropped_lines(), 3);
    }

    #[test]
    fn long_lines_are_truncated_by_characters_not_bytes() {
        let buf = ProgressBuffer::new();
        buf.push(&"한".repeat(MAX_LINE_CHARS + 10));
        let line = &buf.snapshot()[0];
        assert_eq!(line.chars().count(), MAX_LINE_CHARS);
    }
}
