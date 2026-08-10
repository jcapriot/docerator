//! Converts a diagnostic's byte-offset `TextRange` into a 1-indexed line/column for display —
//! `Diagnostic` itself only ever carries byte offsets (the same coordinate space edits use), so
//! this conversion happens once, at presentation time, not during resolution.

use ruff_source_file::LineIndex;
use ruff_text_size::TextSize;

/// 1-indexed line and column for a byte offset within `source`.
pub fn line_column(source: &str, offset: TextSize) -> (usize, usize) {
    let index = LineIndex::from_source_text(source);
    let location = index.line_column(offset, source);
    (location.line.get(), location.column.get())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_line_first_column() {
        assert_eq!(line_column("hello\nworld", TextSize::from(0)), (1, 1));
    }

    #[test]
    fn second_line_after_newline() {
        assert_eq!(line_column("hello\nworld", TextSize::from(6)), (2, 1));
    }

    #[test]
    fn mid_line_column() {
        assert_eq!(line_column("hello\nworld", TextSize::from(8)), (2, 3));
    }
}
