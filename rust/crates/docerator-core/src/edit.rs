use ruff_text_size::TextRange;

/// A single splice: replace the bytes in `range` (against the ORIGINAL source) with `replacement`.
#[derive(Debug, Clone)]
pub struct TextEdit {
    pub range: TextRange,
    pub replacement: String,
}

impl TextEdit {
    pub fn new(range: TextRange, replacement: impl Into<String>) -> Self {
        Self {
            range,
            replacement: replacement.into(),
        }
    }
}

/// Apply a set of non-overlapping edits to `source` in one pass, splicing at each edit's
/// original byte range. Panics if edits overlap — callers are expected to guarantee this by
/// construction (distinct docstring entries never share a byte range).
pub fn apply_edits(source: &str, mut edits: Vec<TextEdit>) -> String {
    if edits.is_empty() {
        return source.to_string();
    }
    edits.sort_by_key(|e| e.range.start());
    for pair in edits.windows(2) {
        assert!(
            pair[0].range.end() <= pair[1].range.start(),
            "overlapping text edits: {:?} and {:?}",
            pair[0].range,
            pair[1].range
        );
    }

    let mut out = String::with_capacity(source.len());
    let mut cursor: usize = 0;
    for edit in &edits {
        let start: usize = edit.range.start().into();
        let end: usize = edit.range.end().into();
        out.push_str(&source[cursor..start]);
        out.push_str(&edit.replacement);
        cursor = end;
    }
    out.push_str(&source[cursor..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use ruff_text_size::TextSize;

    fn range(start: u32, end: u32) -> TextRange {
        TextRange::new(TextSize::from(start), TextSize::from(end))
    }

    #[test]
    fn no_edits_returns_source_unchanged() {
        let source = "hello world";
        assert_eq!(apply_edits(source, Vec::new()), source);
    }

    #[test]
    fn single_edit_splices_in_place() {
        let source = "hello world";
        let edits = vec![TextEdit::new(range(6, 11), "rust")];
        assert_eq!(apply_edits(source, edits), "hello rust");
    }

    #[test]
    fn multiple_non_overlapping_edits_apply_left_to_right() {
        let source = "aaa bbb ccc";
        let edits = vec![
            TextEdit::new(range(8, 11), "ZZZ"),
            TextEdit::new(range(0, 3), "XXX"),
        ];
        assert_eq!(apply_edits(source, edits), "XXX bbb ZZZ");
    }

    #[test]
    #[should_panic(expected = "overlapping text edits")]
    fn overlapping_edits_panic() {
        let source = "hello world";
        let edits = vec![
            TextEdit::new(range(0, 5), "a"),
            TextEdit::new(range(3, 8), "b"),
        ];
        apply_edits(source, edits);
    }
}
